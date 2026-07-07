param(
    [string]$Configuration = "release",
    [string]$OutputDirectory = "dist"
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$targetExe = Join-Path $repoRoot "target\$Configuration\kuma-remote.exe"
$distDir = Join-Path $repoRoot $OutputDirectory
$distExe = Join-Path $distDir "kuma-remote.exe"

Push-Location $repoRoot
try {
    cargo build --release

    New-Item -ItemType Directory -Force -Path $distDir | Out-Null
    Copy-Item -LiteralPath $targetExe -Destination $distExe -Force

    $size = (Get-Item -LiteralPath $distExe).Length
    Write-Host "Release executable written to $distExe ($([Math]::Round($size / 1MB, 2)) MB)"
}
finally {
    Pop-Location
}
