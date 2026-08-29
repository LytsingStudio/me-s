$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Join-Path $PSScriptRoot ".."
$env:ME_INSTALL_NO_MAIN = "1"
. (Join-Path $root "install.ps1")

$installerSource = Get-Content -Raw -LiteralPath (Join-Path $root "install.ps1")
if (-not $installerSource.Contains('"LytsingStudio/me-s"')) {
    throw "install.ps1 does not target the me-s release repository"
}
if ($installerSource.Contains('"LytsingStudio/me-rust"')) {
    throw "install.ps1 still targets the legacy me-rust release repository"
}
foreach ($required in @(
    'ME-windows-x86_64-setup.exe',
    '"Programs\ME"',
    'Start-Process -FilePath $package -ArgumentList "/S" -Wait -PassThru',
    '"me-s.exe"',
    '"me-gateway.exe"',
    '"me-client.exe"'
)) {
    if (-not $installerSource.Contains($required)) {
        throw "install.ps1 is missing complete-product behavior: $required"
    }
}
foreach ($legacy in @(
    'me-s-windows-x86_64.exe',
    'me-gateway-windows-x86_64.exe',
    '$installed = @($false, $false)',
    '$hadOriginal = @($false, $false)'
)) {
    if ($installerSource.Contains($legacy)) {
        throw "install.ps1 still contains a legacy two-program marker: $legacy"
    }
}

$assets = @(Get-MeProductAsset "AMD64")
if ($assets.Count -ne 1 -or $assets[0] -cne "ME-windows-x86_64-setup.exe") {
    throw "AMD64 did not select the complete Windows product package"
}
$assets = @(Get-MeProductAsset "x86_64")
if ($assets.Count -ne 1 -or $assets[0] -cne "ME-windows-x86_64-setup.exe") {
    throw "x86_64 did not select the complete Windows product package"
}

$unsupportedFailed = $false
try {
    Get-MeProductAsset "ARM64" | Out-Null
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
    $asset = "ME-windows-x86_64-setup.exe"
    $checksum = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    [System.IO.File]::WriteAllText($manifest, "$checksum  $asset`n")
    if ((Get-MeExpectedChecksum $manifest $asset) -cne $checksum) {
        throw "valid product-package checksum entry was not parsed"
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

$nsisSource = Get-Content -Raw -LiteralPath (Join-Path $root "packaging\windows\installer.nsi")
foreach ($required in @(
    'InstallDir "$LOCALAPPDATA\Programs\ME"',
    'RequestExecutionLevel user',
    'File /oname=me-s.exe "${ME_S}"',
    'File /oname=me-gateway.exe "${ME_GATEWAY}"',
    'File /oname=me-client.exe "${ME_CLIENT}"',
    'WriteUninstaller "$INSTDIR\Uninstall ME.exe"',
    'CreateShortcut "$SMPROGRAMS\ME\ME Client.lnk" "$INSTDIR\me-client.exe"',
    'CreateShortcut "$SMPROGRAMS\ME\Uninstall ME.lnk" "$INSTDIR\Uninstall ME.exe"',
    'Function AddInstallDirToPath',
    'Function un.RemoveInstallDirFromPath',
    'WriteRegExpandStr HKCU "Environment" "Path"',
    'Call AddInstallDirToPath',
    'Call un.RemoveInstallDirFromPath',
    'Delete "$INSTDIR\me-s.exe"',
    'Delete "$INSTDIR\me-gateway.exe"',
    'Delete "$INSTDIR\me-client.exe"',
    'DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ME"',
    'DeleteRegKey HKCU "Software\Lytsing Studio\ME"'
)) {
    if (-not $nsisSource.Contains($required)) {
        throw "installer.nsi is missing required install/uninstall structure: $required"
    }
}

Write-Host "complete Windows product installer tests: PASS"