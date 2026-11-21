# ort-sys extracts the target-specific, checksum-verified DirectML distribution here.
$ort = Get-ChildItem "$env:LOCALAPPDATA/ort.pyke.io/dfbin/x86_64-pc-windows-msvc" -Recurse -Filter DirectML.dll
if (@($ort).Count -ne 1) { throw 'Expected exactly one DirectML.dll from the ORT build' }

$vswhere = "${env:ProgramFiles(x86)}/Microsoft Visual Studio/Installer/vswhere.exe"
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$version = (Get-Content "$vs/VC/Auxiliary/Build/Microsoft.VCRedistVersion.default.txt").Trim()
$crt = Get-Item "$vs/VC/Redist/MSVC/$version/x64/Microsoft.VC*.CRT"
if (@($crt).Count -ne 1) { throw 'Expected the x64 MSVC redistributable CRT directory' }
@{ DirectML = $ort.DirectoryName; CRT = $crt.FullName }
