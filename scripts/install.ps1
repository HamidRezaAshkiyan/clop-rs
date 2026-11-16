$ErrorActionPreference = "Stop"
$Repo = "HamidRezaAshkiyan/clop-rs"

Write-Host "Fetching latest clop-rs release from $Repo..." -ForegroundColor Cyan
try {
    $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Tag = $Release.tag_name
} catch {
    $Tag = "v0.1.0"
}

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\clop-rs"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$ZipUrl = "https://github.com/$Repo/releases/download/$Tag/clop-rs-$Tag-windows-x64.zip"
$ZipPath = Join-Path $env:TEMP "clop-rs-$Tag-windows-x64.zip"

Write-Host "Downloading $ZipUrl..." -ForegroundColor Cyan
Invoke-WebRequest -Uri $ZipUrl -OutFile $ZipPath

Expand-Archive -Path $ZipPath -DestinationPath $InstallDir -Force
Remove-Item $ZipPath -Force

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    Write-Host "✓ Added $InstallDir to User PATH" -ForegroundColor Green
}

$ExePath = Join-Path $InstallDir "clop-rs.exe"
Write-Host "✓ Installed clop-rs to $ExePath" -ForegroundColor Green
Write-Host "Run 'clop-rs autostart --enable' to start automatically at login." -ForegroundColor Yellow
