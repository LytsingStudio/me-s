$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$env:ME_INSTALL_NO_MAIN = "1"
. (Join-Path $PSScriptRoot "..\install.ps1")

$installerSource = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot "..\install.ps1")
if (-not $installerSource.Contains('"LytsingStudio/me-s"')) {
    throw "install.ps1 does not target the me-s release repository"
}
if ($installerSource.Contains('"LytsingStudio/me-rust"')) {
    throw "install.ps1 still targets the legacy me-rust release repository"
}
if (-not $installerSource.Contains('"Programs\me-s"')) {
    throw "install.ps1 does not use the independent ME install directory"
}
foreach ($required in @(
    "me-s.exe",
    "me-gateway.exe",
    '$installed = @($false, $false)',
    '$hadOriginal = @($false, $false)',
    '$reportedVersions = @($null, $null)',
    '$expectedVersionOutputs = @($null, $null)'
) ) {
    if (-not $installerSource.Contains($required)) {
        throw "install.ps1 is missing the dual-program transaction marker: $required"
    }
}

$assets = @(Get-MeSReleaseAssets "AMD64")
if ($assets.Count -ne 2) {
    throw "AMD64 did not select both release assets"
}
if ($assets[0] -cne "me-s-windows-x86_64.exe") {
    throw "AMD64 selected the wrong me-s release asset"
}
if ($assets[1] -cne "me-gateway-windows-x86_64.exe") {
    throw "AMD64 selected the wrong me-gateway release asset"
}
$assets = @(Get-MeSReleaseAssets "x86_64")
if ($assets.Count -ne 2) {
    throw "x86_64 did not select both release assets"
}

$unsupportedFailed = $false
try {
    Get-MeSReleaseAssets "ARM64" | Out-Null
} catch {
    $unsupportedFailed = $true
}
if (-not $unsupportedFailed) {
    throw "unsupported Windows architecture was accepted"
}

$testDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "me-install-ps-test-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $testDirectory | Out-Null
try {
    $manifest = Join-Path $testDirectory "SHA256SUMS"
    $meSAsset = "me-s-windows-x86_64.exe"
    $gatewayAsset = "me-gateway-windows-x86_64.exe"
    $meSChecksum = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    $gatewayChecksum = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
    [System.IO.File]::WriteAllText(
        $manifest,
        "$meSChecksum  $meSAsset`n$gatewayChecksum  $gatewayAsset`n"
    )
    if ((Get-MeSExpectedChecksum $manifest $meSAsset) -cne $meSChecksum) {
        throw "valid me-s checksum entry was not parsed"
    }
    if ((Get-MeSExpectedChecksum $manifest $gatewayAsset) -cne $gatewayChecksum) {
        throw "valid me-gateway checksum entry was not parsed"
    }

    [System.IO.File]::AppendAllText($manifest, "$meSChecksum *$meSAsset`n")
    $duplicateFailed = $false
    try {
        Get-MeSExpectedChecksum $manifest $meSAsset | Out-Null
    } catch {
        $duplicateFailed = $true
    }
    if (-not $duplicateFailed) {
        throw "duplicate checksum entries were accepted"
    }

    [System.IO.File]::WriteAllText($manifest, "invalid  $gatewayAsset`n")
    $invalidFailed = $false
    try {
        Get-MeSExpectedChecksum $manifest $gatewayAsset | Out-Null
    } catch {
        $invalidFailed = $true
    }
    if (-not $invalidFailed) {
        throw "invalid checksum entry was accepted"
    }
} finally {
    Remove-Item -Recurse -Force -LiteralPath $testDirectory
}

Write-Host "install.ps1 tests: PASS"
