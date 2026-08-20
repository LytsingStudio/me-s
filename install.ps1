$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-MeSReleaseAssets {
    param([string]$Architecture)

    $normalized = $Architecture.Trim().ToUpperInvariant()
    if ($normalized -eq "AMD64" -or $normalized -eq "X86_64") {
        return @("me-s-windows-x86_64.exe", "me-gateway-windows-x86_64.exe")
    }
    throw "ME does not provide a Windows release for $Architecture"
}

function Get-MeSExpectedChecksum {
    param(
        [string]$Manifest,
        [string]$Asset
    )

    $entries = @()
    foreach ($line in [System.IO.File]::ReadLines($Manifest)) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$') {
            if ($Matches[2] -ceq $Asset) {
                $entries += $Matches[1].ToLowerInvariant()
            }
        }
    }
    if ($entries.Count -ne 1) {
        throw "SHA256SUMS has no single valid entry for $Asset"
    }
    return $entries[0]
}

function Invoke-MeSDownload {
    param(
        [string]$Uri,
        [string]$OutFile
    )

    $arguments = @{
        Uri                = $Uri
        OutFile            = $OutFile
        Headers            = @{ "User-Agent" = "me-installer" }
        MaximumRedirection = 10
    }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        $arguments["UseBasicParsing"] = $true
    }
    Invoke-WebRequest @arguments
}

function Add-MeSToUserPath {
    param([string]$Directory)

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $alreadyPresent = $false
    foreach ($entry in $entries) {
        if ([string]::Equals(
                $entry.TrimEnd('\'),
                $Directory.TrimEnd('\'),
                [StringComparison]::OrdinalIgnoreCase)) {
            $alreadyPresent = $true
            break
        }
    }
    if (-not $alreadyPresent) {
        $newUserPath = if ([string]::IsNullOrWhiteSpace($userPath)) {
            $Directory
        } else {
            "$($userPath.TrimEnd(';'));$Directory"
        }
        [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
    }

    $processEntries = @($env:Path -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $processHasDirectory = $false
    foreach ($entry in $processEntries) {
        if ([string]::Equals(
                $entry.TrimEnd('\'),
                $Directory.TrimEnd('\'),
                [StringComparison]::OrdinalIgnoreCase)) {
            $processHasDirectory = $true
            break
        }
    }
    if (-not $processHasDirectory) {
        $env:Path = if ([string]::IsNullOrWhiteSpace($env:Path)) {
            $Directory
        } else {
            "$env:Path;$Directory"
        }
    }
}

function Install-MeS {
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    }

    $architecture = if (-not [string]::IsNullOrWhiteSpace($env:ME_INSTALL_ARCH)) {
        $env:ME_INSTALL_ARCH
    } elseif (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITEW6432
    } else {
        $env:PROCESSOR_ARCHITECTURE
    }
    $assets = @(Get-MeSReleaseAssets $architecture)

    $repository = if ([string]::IsNullOrWhiteSpace($env:ME_INSTALL_REPOSITORY)) {
        "LytsingStudio/me-s"
    } else {
        $env:ME_INSTALL_REPOSITORY
    }
    $baseUrl = if ([string]::IsNullOrWhiteSpace($env:ME_INSTALL_BASE_URL)) {
        "https://github.com/$repository/releases/latest/download"
    } else {
        $env:ME_INSTALL_BASE_URL.TrimEnd('/')
    }
    $installDirectory = if ([string]::IsNullOrWhiteSpace($env:ME_INSTALL_DIR)) {
        Join-Path $env:LOCALAPPDATA "Programs\me-s"
    } else {
        $env:ME_INSTALL_DIR
    }
    $installDirectory = [System.IO.Path]::GetFullPath($installDirectory)

    $temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "me-install-$([Guid]::NewGuid().ToString('N'))"
    $manifest = Join-Path $temporaryDirectory "SHA256SUMS"
    $downloaded = @(
        (Join-Path $temporaryDirectory $assets[0]),
        (Join-Path $temporaryDirectory $assets[1])
    )
    $destinations = @(
        (Join-Path $installDirectory "me-s.exe"),
        (Join-Path $installDirectory "me-gateway.exe")
    )
    $transactionId = [Guid]::NewGuid().ToString('N')
    $staging = @(
        (Join-Path $installDirectory ".me-s-install-$transactionId.exe"),
        (Join-Path $installDirectory ".me-gateway-install-$transactionId.exe")
    )
    $backups = @(
        (Join-Path $installDirectory ".me-s-backup-$transactionId.exe"),
        (Join-Path $installDirectory ".me-gateway-backup-$transactionId.exe")
    )
    $hadOriginal = @($false, $false)
    $installed = @($false, $false)
    $programNames = @("me-s", "me-gateway")
    $reportedVersions = @($null, $null)
    $expectedVersionOutputs = @($null, $null)
    $committed = $false

    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    try {
        Write-Host "Downloading $($assets[0]) and $($assets[1])..."
        for ($index = 0; $index -lt 2; $index++) {
            Invoke-MeSDownload "$baseUrl/$($assets[$index])" $downloaded[$index]
        }
        Invoke-MeSDownload "$baseUrl/SHA256SUMS" $manifest

        for ($index = 0; $index -lt 2; $index++) {
            $expected = Get-MeSExpectedChecksum $manifest $assets[$index]
            $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $downloaded[$index]).Hash.ToLowerInvariant()
            if ($actual -cne $expected) {
                throw "checksum verification failed for $($assets[$index])"
            }
            $outputLines = @(& $downloaded[$index] version)
            if ($LASTEXITCODE -ne 0) {
                throw "downloaded $($assets[$index]) cannot run on this system"
            }
            $versionOutput = ($outputLines -join "`n").Trim()
            $escapedName = [Regex]::Escape($programNames[$index])
            if ($versionOutput -cnotmatch "^$escapedName [0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$") {
                throw "downloaded $($assets[$index]) reported an unexpected program identity or version"
            }
            $reportedVersions[$index] = $versionOutput.Substring($programNames[$index].Length + 1)
            $expectedVersionOutputs[$index] = $versionOutput
        }
        if ($reportedVersions[0] -cne $reportedVersions[1]) {
            throw "downloaded ME programs report different versions"
        }
        Write-Host "Checksums verified."

        New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
        for ($index = 0; $index -lt 2; $index++) {
            if (Test-Path -LiteralPath $destinations[$index]) {
                $target = Get-Item -Force -LiteralPath $destinations[$index]
                $isReparsePoint = ($target.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
                if ($target.PSIsContainer -or $isReparsePoint -or -not (Test-Path -LiteralPath $destinations[$index] -PathType Leaf)) {
                    throw "install target is not a regular file: $($destinations[$index])"
                }
            }
            Copy-Item -LiteralPath $downloaded[$index] -Destination $staging[$index]
        }
        for ($index = 0; $index -lt 2; $index++) {
            if (Test-Path -LiteralPath $destinations[$index] -PathType Leaf) {
                Move-Item -Force -LiteralPath $destinations[$index] -Destination $backups[$index]
                $hadOriginal[$index] = $true
            }
        }
        for ($index = 0; $index -lt 2; $index++) {
            Move-Item -Force -LiteralPath $staging[$index] -Destination $destinations[$index]
            $installed[$index] = $true
        }
        for ($index = 0; $index -lt 2; $index++) {
            $outputLines = @(& $destinations[$index] version)
            if ($LASTEXITCODE -ne 0) {
                throw "installed program could not be started: $($destinations[$index])"
            }
            $versionOutput = ($outputLines -join "`n").Trim()
            if ($versionOutput -cne $expectedVersionOutputs[$index]) {
                throw "installed program reported an unexpected version: $($destinations[$index])"
            }
            Write-Host $versionOutput
        }
        $committed = $true
        foreach ($backup in $backups) {
            Remove-Item -Force -LiteralPath $backup -ErrorAction SilentlyContinue
        }
        Add-MeSToUserPath $installDirectory

        Write-Host "Installed ME to $installDirectory"
        Write-Host "  me-s: $($destinations[0])"
        Write-Host "  me-gateway: $($destinations[1])"
        Write-Host "Open a new terminal if the commands are not immediately available."
    } catch {
        $installError = $_.Exception.Message
        if (-not $committed) {
            $rollbackErrors = @()
            for ($index = 1; $index -ge 0; $index--) {
                try {
                    if ($installed[$index] -and (Test-Path -LiteralPath $destinations[$index])) {
                        Remove-Item -Force -LiteralPath $destinations[$index]
                    }
                    if ($hadOriginal[$index] -and (Test-Path -LiteralPath $backups[$index])) {
                        Move-Item -Force -LiteralPath $backups[$index] -Destination $destinations[$index]
                    }
                } catch {
                    $rollbackErrors += $_.Exception.Message
                }
            }
            if ($rollbackErrors.Count -gt 0) {
                throw "$installError; rollback also failed: $($rollbackErrors -join '; ')"
            }
        }
        throw $installError
    } finally {
        foreach ($path in $staging) {
            Remove-Item -Force -LiteralPath $path -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $temporaryDirectory) {
            Remove-Item -Recurse -Force -LiteralPath $temporaryDirectory
        }
    }
}

if ($env:ME_INSTALL_NO_MAIN -ne "1") {
    Install-MeS
}
