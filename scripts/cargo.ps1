param([Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$env:CARGO_HOME = Join-Path $projectRoot '.tools\cargo'
$env:RUSTUP_HOME = Join-Path $projectRoot '.tools\rustup'
$cargoBin = Join-Path $env:CARGO_HOME 'bin'
if (($env:PATH -split ';') -notcontains $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }
# VS 2026 is not discovered by every version of the Rust C compiler helper.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if ((Test-Path -LiteralPath $vswhere) -and -not $env:VSCMD_VER) {
    $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($vs) {
        $devShell = Join-Path $vs 'Common7\Tools\Launch-VsDevShell.ps1'
        if (Test-Path -LiteralPath $devShell) {
            & $devShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Null
        }
    }
}
Push-Location $projectRoot
try {
    & (Join-Path $env:CARGO_HOME 'bin\cargo.exe') @CargoArgs
    exit $LASTEXITCODE
} finally { Pop-Location }
