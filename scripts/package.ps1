$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
& (Join-Path $PSScriptRoot 'cargo.ps1') build --release --locked --offline
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$outputDir = Join-Path $projectRoot 'artifacts\NetMon'
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
Copy-Item -LiteralPath (Join-Path $projectRoot 'target\release\netmon.exe') -Destination (Join-Path $outputDir 'NetMon.exe')
Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination (Join-Path $outputDir 'README.md')
Copy-Item -LiteralPath (Join-Path $projectRoot 'VALIDATION.md') -Destination (Join-Path $outputDir 'VALIDATION.md')
Copy-Item -LiteralPath (Join-Path $projectRoot 'LICENSE.md') -Destination (Join-Path $outputDir 'LICENSE.md')
$hash = (Get-FileHash -LiteralPath (Join-Path $outputDir 'NetMon.exe') -Algorithm SHA256).Hash
Set-Content -LiteralPath (Join-Path $outputDir 'SHA256.txt') -Value "$hash  NetMon.exe"
Write-Output "Готово: $outputDir"
