[CmdletBinding()]
param(
    [string]$StartupName = "ElowenEdge",
    [string]$EnvFile,
    [string]$TunnelUser,
    [string]$TunnelHost,
    [switch]$Release,
    [switch]$SkipTunnel
)

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$startScript = Join-Path $PSScriptRoot "Start-ElowenEdge.ps1"
$startupFolder = [Environment]::GetFolderPath("Startup")

if (-not $EnvFile) {
    $EnvFile = Join-Path $repoRoot "edge.env.local"
}

if (-not (Test-Path -LiteralPath $EnvFile)) {
    throw "Edge env file not found: $EnvFile"
}

if (-not $SkipTunnel -and (-not $TunnelUser -or -not $TunnelHost)) {
    throw "TunnelUser and TunnelHost are required unless -SkipTunnel is set."
}

function Quote-StartupArgument {
    param([string]$Value)

    '"' + $Value.Replace('"', '""') + '"'
}

$argumentParts = @(
    "-NoProfile"
    "-ExecutionPolicy"
    "Bypass"
    "-File"
    (Quote-StartupArgument $startScript)
    "-EnvFile"
    (Quote-StartupArgument $EnvFile)
    "-Detach"
)

if ($Release) {
    $argumentParts += "-Release"
}

if ($SkipTunnel) {
    $argumentParts += "-SkipTunnel"
} else {
    $argumentParts += "-TunnelUser"
    $argumentParts += (Quote-StartupArgument $TunnelUser)
    $argumentParts += "-TunnelHost"
    $argumentParts += (Quote-StartupArgument $TunnelHost)
}

$launcherPath = Join-Path $startupFolder "$StartupName.cmd"
$launcherContent = @(
    "@echo off"
    "powershell.exe $($argumentParts -join ' ')"
) -join "`r`n"

Set-Content -LiteralPath $launcherPath -Value $launcherContent -Encoding ASCII

Write-Host "Installed startup launcher at $launcherPath"
Write-Host "Remove it later by deleting that file."
