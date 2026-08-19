$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$env:ME_INSTALL_NO_MAIN = "1"
. (Join-Path $PSScriptRoot "..\install.ps1")

if ((Get-MeReleaseAsset "AMD64") -cne "me-s-windows-x86_64.exe") {
    throw "AMD64 selected the wrong release asset"
}
if ((Get-MeReleaseAsset "x86_64") -cne "me-s-windows-x86_64.exe") {
    throw "x86_64 selected the wrong release asset"
}

$unsupportedFailed = $false
try {
    Get-MeReleaseAsset "ARM64" | Out-Null
} catch {
    $unsupportedFailed = $true
}
if (-not $unsupportedFailed) {
    throw "unsupported Windows architecture was accepted"
}

$testDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "me-s-install-ps-test-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $testDirectory | Out-Null
try {
    $manifest = Join-Path $testDirectory "SHA256SUMS"
    $asset = "me-s-windows-x86_64.exe"
    $checksum = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    [System.IO.File]::WriteAllText($manifest, "$checksum  $asset`n")
    if ((Get-MeExpectedChecksum $manifest $asset) -cne $checksum) {
        throw "valid checksum entry was not parsed"
    }

    [System.IO.File]::AppendAllText($manifest, "$checksum *$asset`n")
    $duplicateFailed = $false
    try {
        Get-MeExpectedChecksum $manifest $asset | Out-Null
    } catch {
        $duplicateFailed = $true
    }
    if (-not $duplicateFailed) {
        throw "duplicate checksum entries were accepted"
    }

    [System.IO.File]::WriteAllText($manifest, "invalid  $asset`n")
    $invalidFailed = $false
    try {
        Get-MeExpectedChecksum $manifest $asset | Out-Null
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
