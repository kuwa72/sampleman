# PowerShell script to build, install, and run Sampleman on Windows
$ErrorActionPreference = "Stop"

$ProjectRoot = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectRoot

Write-Host "[1/3] Building Sampleman (Release)..."
cargo build --release

$InstallDir = "$env:LOCALAPPDATA\Programs\Sampleman"
Write-Host "[2/3] Installing to $InstallDir..."
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

Copy-Item -Path "target\release\sampleman.exe" -Destination "$InstallDir\sampleman.exe" -Force
if (Test-Path "soundfont.sf2") {
    Copy-Item -Path "soundfont.sf2" -Destination "$InstallDir\soundfont.sf2" -Force
}

Write-Host "[3/3] Launching Sampleman..."
Start-Process -FilePath "$InstallDir\sampleman.exe" -WorkingDirectory $InstallDir

Write-Host "Done! Sampleman installed and started successfully."
