[CmdletBinding()]
param(
    [string]$BinaryPath,
    [string]$OutputDir,
    [string]$InnoSetupCompiler,
    [string]$AppVersion,
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

$BinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path

if (-not $OutputDir) {
    $OutputDir = Join-Path $repoRoot "dist"
}

if (-not (Test-Path -LiteralPath $OutputDir)) {
    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
}

$OutputDir = (Resolve-Path -LiteralPath $OutputDir).Path

if (-not $AppVersion) {
    $cargoToml = Get-Content -LiteralPath (Join-Path $repoRoot "Cargo.toml")
    $versionLine = $cargoToml | Where-Object { $_ -match '^\s*version\s*=\s*"([^"]+)"' } | Select-Object -First 1
    if ($versionLine -match '^\s*version\s*=\s*"([^"]+)"') {
        $AppVersion = $Matches[1]
    } else {
        $AppVersion = "0.1.0"
    }
}

function Resolve-InnoSetupCompiler {
    param([string]$ExplicitPath)

    if ($ExplicitPath) {
        if (-not (Test-Path -LiteralPath $ExplicitPath)) {
            throw "Inno Setup compiler not found: $ExplicitPath"
        }
        return (Resolve-Path -LiteralPath $ExplicitPath).Path
    }

    $command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $candidatePaths = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )

    foreach ($candidate in $candidatePaths) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }

    throw @"
Inno Setup compiler (ISCC.exe) was not found.

Install Inno Setup 6, then rerun this command. For example:
  winget install --id JRSoftware.InnoSetup -e

You can also pass -InnoSetupCompiler "C:\Path\To\ISCC.exe".
"@
}

$compiler = Resolve-InnoSetupCompiler -ExplicitPath $InnoSetupCompiler
$scriptPath = Join-Path $repoRoot "packaging\windows\elowen-edge.iss"

& $compiler `
    "/DSourceRoot=$repoRoot" `
    "/DBinaryPath=$BinaryPath" `
    "/DOutputDir=$OutputDir" `
    "/DAppVersion=$AppVersion" `
    $scriptPath

if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup compiler failed with exit code $LASTEXITCODE"
}

$installerPath = Join-Path $OutputDir "ElowenEdgeSetup.exe"
if (-not (Test-Path -LiteralPath $installerPath)) {
    throw "Expected installer was not created: $installerPath"
}

Write-Host "Created Elowen Edge Inno Setup installer at $installerPath"
