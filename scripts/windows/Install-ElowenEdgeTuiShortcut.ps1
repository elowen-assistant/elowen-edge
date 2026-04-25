[CmdletBinding()]
param(
    [string]$ShortcutName = "Elowen Edge TUI",
    [string]$ConfigFile,
    [string]$BinaryPath,
    [switch]$Release,
    [ValidateSet("Desktop", "StartMenu")]
    [string]$Location = "Desktop"
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

if (-not $ConfigFile) {
    $ConfigFile = Join-Path $repoRoot "edge.toml"
}

if (-not (Test-Path -LiteralPath $ConfigFile)) {
    throw "Edge TOML config not found: $ConfigFile"
}

$ConfigFile = (Resolve-Path -LiteralPath $ConfigFile).Path

if (-not $BinaryPath) {
    $profile = if ($Release) { "release" } else { "debug" }
    $BinaryPath = Join-Path $repoRoot "target\$profile\elowen-edge.exe"
}

if (-not (Test-Path -LiteralPath $BinaryPath)) {
    throw "Edge binary not found: $BinaryPath"
}

$BinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path

if ($Location -eq "Desktop") {
    $shortcutDirectory = [Environment]::GetFolderPath("Desktop")
} else {
    $shortcutDirectory = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs"
}

if (-not (Test-Path -LiteralPath $shortcutDirectory)) {
    New-Item -ItemType Directory -Force -Path $shortcutDirectory | Out-Null
}

$shortcutPath = Join-Path $shortcutDirectory "$ShortcutName.lnk"
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $BinaryPath
$shortcut.Arguments = "tui --config `"$ConfigFile`""
$shortcut.WorkingDirectory = $repoRoot
$shortcut.Description = "Open the Elowen Edge TUI for local edge configuration and diagnostics."
$shortcut.IconLocation = "$BinaryPath,0"
$shortcut.Save()

Write-Host "Installed Elowen Edge TUI shortcut at $shortcutPath"
