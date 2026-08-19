$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-MeReleaseAsset {
    param([string]$Architecture)

    $normalized = $Architecture.Trim().ToUpperInvariant()
    if ($normalized -eq "AMD64" -or $normalized -eq "X86_64") {
        return "me-s-windows-x86_64.exe"
    }
    throw "me-s does not provide a Windows release for $Architecture"
}

function Get-MeExpectedChecksum {
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

function Invoke-MeDownload {
    param(
        [string]$Uri,
        [string]$OutFile
    )

    $arguments = @{
        Uri                = $Uri
        OutFile            = $OutFile
        Headers            = @{ "User-Agent" = "me-s-installer" }
        MaximumRedirection = 10
    }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        $arguments["UseBasicParsing"] = $true
    }
    Invoke-WebRequest @arguments
}

function Add-MeToUserPath {
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

function Install-Me {
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
    $asset = Get-MeReleaseAsset $architecture

    $repository = if ([string]::IsNullOrWhiteSpace($env:ME_INSTALL_REPOSITORY)) {
        "LytsingStudio/me-rust"
    } else {
        $env:ME_INSTALL_REPOSITORY
    }
    $baseUrl = if ([string]::IsNullOrWhiteSpace($env:ME_INSTALL_BASE_URL)) {
        "https://github.com/$repository/releases/latest/download"
    } else {
        $env:ME_INSTALL_BASE_URL.TrimEnd('/')
    }
    $installDirectory = if ([string]::IsNullOrWhiteSpace($env:ME_INSTALL_DIR)) {
        Join-Path $env:LOCALAPPDATA "Programs\me"
    } else {
        $env:ME_INSTALL_DIR
    }
    $installDirectory = [System.IO.Path]::GetFullPath($installDirectory)

    $temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "me-s-install-$([Guid]::NewGuid().ToString('N'))"
    $downloadedAsset = Join-Path $temporaryDirectory $asset
    $manifest = Join-Path $temporaryDirectory "SHA256SUMS"
    $staging = Join-Path $installDirectory ".me-s-install-$([Guid]::NewGuid().ToString('N')).exe"
    $destination = Join-Path $installDirectory "me-s.exe"

    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    try {
        Write-Host "Downloading $asset..."
        Invoke-MeDownload "$baseUrl/$asset" $downloadedAsset
        Invoke-MeDownload "$baseUrl/SHA256SUMS" $manifest

        $expected = Get-MeExpectedChecksum $manifest $asset
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $downloadedAsset).Hash.ToLowerInvariant()
        if ($actual -cne $expected) {
            throw "checksum verification failed for $asset"
        }
        Write-Host "Checksum verified."

        & $downloadedAsset version | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "downloaded $asset cannot run on this system"
        }

        New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
        Copy-Item -LiteralPath $downloadedAsset -Destination $staging
        Move-Item -Force -LiteralPath $staging -Destination $destination
        Add-MeToUserPath $installDirectory

        & $destination version
        if ($LASTEXITCODE -ne 0) {
            throw "me-s was installed to $destination but could not be started"
        }
        Write-Host "Installed me-s to $destination"
        Write-Host "Open a new terminal if the me-s command is not immediately available."
    } finally {
        if (Test-Path -LiteralPath $staging) {
            Remove-Item -Force -LiteralPath $staging
        }
        if (Test-Path -LiteralPath $temporaryDirectory) {
            Remove-Item -Recurse -Force -LiteralPath $temporaryDirectory
        }
    }
}

if ($env:ME_INSTALL_NO_MAIN -ne "1") {
    Install-Me
}
