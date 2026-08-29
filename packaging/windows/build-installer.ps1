param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$MeS,
    [Parameter(Mandatory = $true)][string]$Gateway,
    [Parameter(Mandatory = $true)][string]$Client,
    [Parameter(Mandatory = $true)][string]$Output,
    [string]$Makensis = "makensis.exe"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

foreach ($path in @($MeS, $Gateway, $Client)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "missing installer input: $path"
    }
}
if ($Version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "Windows installer requires a stable three-part version: $Version"
}

$script = Join-Path $PSScriptRoot "installer.nsi"
$icon = Join-Path $PSScriptRoot "..\..\me-client\src-tauri\icons\icon.ico"
$parent = Split-Path -Parent $Output
New-Item -ItemType Directory -Force -Path $parent | Out-Null
Remove-Item -Force -LiteralPath $Output -ErrorAction SilentlyContinue

& $Makensis "/DVERSION=$Version" "/DME_S=$([IO.Path]::GetFullPath($MeS))" "/DME_GATEWAY=$([IO.Path]::GetFullPath($Gateway))" "/DME_CLIENT=$([IO.Path]::GetFullPath($Client))" "/DOUTPUT=$([IO.Path]::GetFullPath($Output))" "/DICON=$([IO.Path]::GetFullPath($icon))" $script
if ($LASTEXITCODE -ne 0) {
    throw "makensis failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path -LiteralPath $Output -PathType Leaf)) {
    throw "makensis did not create $Output"
}
Write-Host "built $Output"
