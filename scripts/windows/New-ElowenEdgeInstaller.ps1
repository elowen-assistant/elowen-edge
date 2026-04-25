[CmdletBinding()]
param(
    [string]$OutputPath,
    [string]$BinaryPath,
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

if (-not $BinaryPath) {
    $profile = if ($Release) { "release" } else { "debug" }
    $BinaryPath = Join-Path $repoRoot "target\$profile\elowen-edge.exe"
}

if (-not (Test-Path -LiteralPath $BinaryPath)) {
    throw "Edge binary not found: $BinaryPath"
}

if (-not $OutputPath) {
    $distDir = Join-Path $repoRoot "dist"
    $OutputPath = Join-Path $distDir "Install-ElowenEdge.ps1"
}

$OutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$outputDir = Split-Path -Parent $OutputPath
if (-not (Test-Path -LiteralPath $outputDir)) {
    New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
}

$stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) "elowen-edge-installer-$PID"
$payloadRoot = Join-Path $stageRoot "payload"
$zipPath = Join-Path $stageRoot "payload.zip"

if (Test-Path -LiteralPath $stageRoot) {
    Remove-Item -LiteralPath $stageRoot -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $payloadRoot | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $payloadRoot "scripts\windows") | Out-Null

Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $payloadRoot "elowen-edge.exe") -Force
Copy-Item -LiteralPath (Join-Path $repoRoot "edge.toml.example") -Destination (Join-Path $payloadRoot "edge.toml.example") -Force
Copy-Item -LiteralPath (Join-Path $repoRoot "README.md") -Destination (Join-Path $payloadRoot "README.md") -Force

$scriptNames = @(
    "Start-ElowenEdge.ps1",
    "Register-ElowenEdgeTask.ps1",
    "Install-ElowenEdgeTuiShortcut.ps1"
)
foreach ($scriptName in $scriptNames) {
    Copy-Item `
        -LiteralPath (Join-Path $PSScriptRoot $scriptName) `
        -Destination (Join-Path $payloadRoot "scripts\windows\$scriptName") `
        -Force
}

Compress-Archive -Path (Join-Path $payloadRoot "*") -DestinationPath $zipPath -Force
$payloadBase64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($zipPath))
$templatePath = Join-Path $PSScriptRoot "installer-template.ps1"
$template = Get-Content -LiteralPath $templatePath -Raw
$installer = $template.Replace("__ELOWEN_EDGE_PAYLOAD_BASE64__", $payloadBase64)
Set-Content -LiteralPath $OutputPath -Value $installer -Encoding UTF8

Remove-Item -LiteralPath $stageRoot -Recurse -Force

Write-Host "Created Elowen Edge installer at $OutputPath"
