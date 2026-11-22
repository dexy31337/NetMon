$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
Set-Location -LiteralPath $projectRoot
$outputDir = Join-Path $projectRoot 'artifacts\probe'
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
try {
    & (Join-Path $projectRoot 'target\debug\netmon-probe.exe') --self-test --output $outputDir *> (Join-Path $outputDir 'console.log')
    $result = $LASTEXITCODE
} catch {
    $_ | Out-File -LiteralPath (Join-Path $outputDir 'console.log') -Append
    $result = 1
}
Set-Content -LiteralPath (Join-Path $outputDir 'exit-code.txt') -Value $result
exit $result
