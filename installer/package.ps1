# Builds the release files of ZTHING into dist\:
#   dist\ZTHING-<version>\          programs, zthing.dll, SDK, docs
#   dist\ZTHING-<version>.zip       the same folder as a ZIP
#   dist\ZTHING-<version>-setup.exe the installer, if Inno Setup 6 is installed
#
# Signing is optional. With ZTHING_SIGN_PFX (path to a .pfx code signing
# certificate) and ZTHING_SIGN_PASSWORD set, the programs, the DLL and the
# installer are signed with signtool and a RFC 3161 timestamp.
#
#   powershell -ExecutionPolicy Bypass -File installer\package.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root 'dist'
$out = Join-Path $dist "ZTHING-$version"
# A separate target folder lets the release build run while an installed
# service holds target\release\zthing-service.exe open.
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }

cargo build --release --workspace
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force "$out\sdk\include", "$out\sdk\lib", "$out\sdk\docs", "$out\sdk\examples" | Out-Null
foreach ($file in 'zthing-gui.exe', 'zthing-service.exe', 'zt.exe', 'zthing.dll') {
    Copy-Item "$target\release\$file" $out
}
Copy-Item "$target\release\zthing.dll.lib" "$out\sdk\lib"
Copy-Item crates\zthing-ffi\include\zthing.h "$out\sdk\include"
Copy-Item docs\API.md "$out\sdk\docs"
Copy-Item -Recurse python "$out\sdk\python"
Get-ChildItem "$out\sdk\python" -Recurse -Directory -Filter __pycache__ | Remove-Item -Recurse -Force
foreach ($example in 'c', 'csharp', 'python') {
    Copy-Item -Recurse "examples\$example" "$out\sdk\examples\$example"
}
Remove-Item -Recurse -Force "$out\sdk\examples\csharp\bin", "$out\sdk\examples\csharp\obj" -ErrorAction SilentlyContinue
Copy-Item README.md, README.de.md, LICENSE $out
# build.rs of zthing-gui renders the icon; the newest build has the current logo.
$icon = Get-ChildItem "$target\release\build\zthing-gui-*\out\zthing.ico" |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
Copy-Item $icon.FullName "$out\zthing.ico"

$signtool = $null
if ($env:ZTHING_SIGN_PFX) {
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $signtool) { throw 'ZTHING_SIGN_PFX is set, but signtool.exe from the Windows SDK was not found' }
}
function Sign([string[]]$files) {
    if (-not $signtool) { return }
    & $signtool.FullName sign /f $env:ZTHING_SIGN_PFX /p $env:ZTHING_SIGN_PASSWORD /fd sha256 `
        /tr http://timestamp.digicert.com /td sha256 /d ZTHING $files
    if ($LASTEXITCODE -ne 0) { throw 'signing failed' }
}
Sign (Get-ChildItem $out -Filter *.exe).FullName
Sign "$out\zthing.dll"

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $out -DestinationPath $zip

$iscc = Get-Command iscc -ErrorAction SilentlyContinue
if (-not $iscc) {
    # Machine-wide or, as winget installs it, for the current user only.
    $iscc = Get-ChildItem "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" -ErrorAction SilentlyContinue |
        Select-Object -First 1
}
if ($iscc) {
    & $iscc.FullName "/DVersion=$version" "/DSource=$out" installer\zthing.iss
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup failed' }
    Sign "$dist\ZTHING-$version-setup.exe"
} else {
    Write-Warning 'Inno Setup 6 not found, no installer built (https://jrsoftware.org/isinfo.php)'
}
Get-ChildItem $dist -File | Select-Object Name, Length
