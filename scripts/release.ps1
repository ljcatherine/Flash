# Builds Flash and packages a release into dist\:
#   Flash-<version>-x64.exe            the executable
#   Flash-<version>-x64-portable.zip   the executable in a folder, run from anywhere
#   Flash-<version>-x64.msi            installer (needs the WiX Toolset 'wix' command)
#   SHA256SUMS.txt
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Flash $version"

cargo test
if ($LASTEXITCODE) { throw 'tests failed' }
cargo build --release
if ($LASTEXITCODE) { throw 'build failed' }
$exe = Join-Path $root 'target\release\flash.exe'

$dist = Join-Path $root 'dist'
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory $dist | Out-Null
$name = "Flash-$version-x64"

# 1. Plain executable
Copy-Item $exe (Join-Path $dist "$name.exe")

# 2. Portable zip: a Flash folder with the exe and the readme
$stage = Join-Path $env:TEMP "flash-portable-$version"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory (Join-Path $stage 'Flash') | Out-Null
Copy-Item $exe (Join-Path $stage 'Flash\Flash.exe')
Copy-Item README.md (Join-Path $stage 'Flash\README.md')
Compress-Archive -Path (Join-Path $stage 'Flash') -DestinationPath (Join-Path $dist "$name-portable.zip")
Remove-Item $stage -Recurse -Force

# 3. MSI installer
wix build packaging\flash.wxs -arch x64 -d "Version=$version" -d "Exe=$exe" -o (Join-Path $dist "$name.msi")
if ($LASTEXITCODE) { throw 'MSI build failed' }
Remove-Item (Join-Path $dist '*.wixpdb') -ErrorAction SilentlyContinue

# 4. Checksums
Get-ChildItem $dist -File | Where-Object Name -ne 'SHA256SUMS.txt' | ForEach-Object {
  '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
} | Set-Content (Join-Path $dist 'SHA256SUMS.txt') -Encoding ascii

Get-ChildItem $dist | Format-Table Name, Length
