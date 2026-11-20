# Builds Flash and packages a release into dist\:
#   Flash-<version>-x64.exe            the executable
#   Flash-<version>-x64-portable.zip   the executable, readme and license in a folder
#   Flash-<version>-x64.msi            installer (needs the WiX Toolset 'wix' command)
#   SHA256SUMS.txt
#
# Code signing (optional; set one of these in your environment, never in the repo):
#   Azure Trusted Signing:  FLASH_SIGN_DLIB     = path to Azure.CodeSigning.Dlib.dll
#                           FLASH_SIGN_METADATA = path to metadata.json (endpoint, account, profile)
#   Certificate (USB key or certificate store):
#                           FLASH_SIGN_THUMBPRINT = SHA-1 thumbprint of the code signing certificate
# Pass -RequireSigning for official releases so an unsigned build fails instead of shipping.
param([switch]$RequireSigning)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Flash $version"

# ---- Signing setup ----
$signArgs = $null
if ($env:FLASH_SIGN_DLIB -and $env:FLASH_SIGN_METADATA) {
  $signArgs = @('/tr', 'http://timestamp.acs.microsoft.com', '/td', 'SHA256', '/dlib', $env:FLASH_SIGN_DLIB, '/dmdf', $env:FLASH_SIGN_METADATA)
  Write-Host 'Signing with Azure Trusted Signing'
} elseif ($env:FLASH_SIGN_THUMBPRINT) {
  $signArgs = @('/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', '/sha1', $env:FLASH_SIGN_THUMBPRINT)
  Write-Host 'Signing with certificate' $env:FLASH_SIGN_THUMBPRINT
} elseif ($RequireSigning) {
  throw 'Signing is required but not configured (see the top of this script).'
} else {
  Write-Warning 'Signing not configured: building UNSIGNED packages.'
}

$signtool = $null
if ($signArgs) {
  $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
  if (-not $signtool) { throw 'signtool.exe not found; install the Windows SDK.' }
}

function Sign([string]$file) {
  if (-not $signArgs) { return }
  & $signtool sign /v /fd SHA256 @signArgs $file
  if ($LASTEXITCODE) { throw "signing failed: $file" }
  & $signtool verify /pa $file | Out-Null
  if ($LASTEXITCODE) { throw "signature didn't verify: $file" }
}

# ---- Build ----
cargo test
if ($LASTEXITCODE) { throw 'tests failed' }
cargo build --release
if ($LASTEXITCODE) { throw 'build failed' }

$dist = Join-Path $root 'dist'
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory $dist | Out-Null
$name = "Flash-$version-x64"

# 1. Executable, signed first: the zip and MSI are built from this signed copy.
$exe = Join-Path $dist "$name.exe"
Copy-Item (Join-Path $root 'target\release\flash.exe') $exe
Sign $exe

# 2. Portable zip: a Flash folder with the exe, readme and license
$stage = Join-Path $env:TEMP "flash-portable-$version"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory (Join-Path $stage 'Flash') | Out-Null
Copy-Item $exe (Join-Path $stage 'Flash\Flash.exe')
Copy-Item README.md (Join-Path $stage 'Flash\README.md')
Copy-Item LICENSE (Join-Path $stage 'Flash\LICENSE.txt')
Compress-Archive -Path (Join-Path $stage 'Flash') -DestinationPath (Join-Path $dist "$name-portable.zip")
Remove-Item $stage -Recurse -Force

# 3. MSI installer, then sign it too
$msi = Join-Path $dist "$name.msi"
wix build packaging\flash.wxs -arch x64 -d "Version=$version" -d "Exe=$exe" -o $msi
if ($LASTEXITCODE) { throw 'MSI build failed' }
Remove-Item (Join-Path $dist '*.wixpdb') -ErrorAction SilentlyContinue
Sign $msi

# 4. Checksums last, over the final (signed) files
Get-ChildItem $dist -File | Where-Object Name -ne 'SHA256SUMS.txt' | ForEach-Object {
  '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
} | Set-Content (Join-Path $dist 'SHA256SUMS.txt') -Encoding ascii

Get-ChildItem $dist | Format-Table Name, Length
if ($signArgs) { Write-Host 'Signed and verified.' } else { Write-Warning 'Packages are UNSIGNED.' }
