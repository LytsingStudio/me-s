$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-MeProductAsset {
    param([string]$Architecture)
    $normalized = $Architecture.Trim().ToUpperInvariant()
    if ($normalized -eq "AMD64" -or $normalized -eq "X86_64") {
        return "ME-windows-x86_64-setup.exe"
    }
    throw "ME does not provide a Windows package for $Architecture"
}

function Get-MeExpectedChecksum {
    param([string]$Manifest, [string]$Asset)
    $entries = @()
    foreach ($line in [System.IO.File]::ReadLines($Manifest)) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$' -and $Matches[2] -ceq $Asset) {
            $entries += $Matches[1].ToLowerInvariant()
        }
    }
    if ($entries.Count -ne 1) {
        throw "SHA256SUMS has no single valid entry for $Asset"
    }
    return $entries[0]
}

function Invoke-MeDownload {
    param([string]$Uri, [string]$OutFile)
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

function Install-MeProduct {
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
    $asset = Get-MeProductAsset $architecture
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
    $installDirectory = Join-Path $env:LOCALAPPDATA "Programs\ME"
    if (-not [string]::IsNullOrWhiteSpace($env:ME_INSTALL_DIR)) {
        $requested = [IO.Path]::GetFullPath($env:ME_INSTALL_DIR)
        if (-not [string]::Equals($requested.TrimEnd('\'), $installDirectory.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)) {
            throw "the Windows product installer uses $installDirectory and does not support ME_INSTALL_DIR"
        }
    }

    $temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) "me-install-$([Guid]::NewGuid().ToString('N'))"
    $package = Join-Path $temporaryDirectory $asset
    $manifest = Join-Path $temporaryDirectory "SHA256SUMS"
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    try {
        Write-Host "Downloading $asset..."
        Invoke-MeDownload "$baseUrl/$asset" $package
        Invoke-MeDownload "$baseUrl/SHA256SUMS" $manifest
        $expected = Get-MeExpectedChecksum $manifest $asset
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $package).Hash.ToLowerInvariant()
        if ($actual -cne $expected) {
            throw "checksum verification failed for $asset"
        }
        Write-Host "Checksum verified."

        $process = Start-Process -FilePath $package -ArgumentList "/S" -Wait -PassThru
        if ($process.ExitCode -ne 0) {
            throw "ME installer exited with code $($process.ExitCode)"
        }

        $meS = Join-Path $installDirectory "me-s.exe"
        $gateway = Join-Path $installDirectory "me-gateway.exe"
        $client = Join-Path $installDirectory "me-client.exe"
        foreach ($path in @($meS, $gateway, $client)) {
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
                throw "ME installer did not create $path"
            }
        }
        $meSOutput = ((& $meS version) -join "`n").Trim()
        $gatewayOutput = ((& $gateway version) -join "`n").Trim()
        if ($meSOutput -cnotmatch '^me-s ([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)$') {
            throw "installed me-s reported an invalid version"
        }
        $version = $meSOutput.Substring(5)
        if ($gatewayOutput -cne "me-gateway $version") {
            throw "installed ME programs report different versions"
        }
        Write-Host $meSOutput
        Write-Host $gatewayOutput
        Write-Host "Installed ME Client: $client"
        Write-Host "Open a new terminal if me-s or me-gateway is not immediately available."
    } finally {
        if (Test-Path -LiteralPath $temporaryDirectory) {
            Remove-Item -Recurse -Force -LiteralPath $temporaryDirectory
        }
    }
}

if ($env:ME_INSTALL_NO_MAIN -ne "1") {
    Install-MeProduct
}
