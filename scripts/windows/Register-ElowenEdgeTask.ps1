[CmdletBinding()]
param(
    [string]$TaskName = "ElowenEdge",
    [string]$EnvFile,
    [string]$TunnelUser,
    [string]$TunnelHost,
    [switch]$Release,
    [switch]$SkipTunnel
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$startScript = Join-Path $PSScriptRoot "Start-ElowenEdge.ps1"

if (-not $EnvFile) {
    $EnvFile = Join-Path $repoRoot "edge.env.local"
}

if (-not (Test-Path -LiteralPath $EnvFile)) {
    throw "Edge env file not found: $EnvFile"
}

if (-not $SkipTunnel -and (-not $TunnelUser -or -not $TunnelHost)) {
    throw "TunnelUser and TunnelHost are required unless -SkipTunnel is set."
}

function Quote-TaskArgument {
    param([string]$Value)

    '"' + $Value.Replace('"', '\"') + '"'
}

$argumentParts = @(
    "-NoProfile"
    "-ExecutionPolicy"
    "Bypass"
    "-File"
    (Quote-TaskArgument $startScript)
    "-EnvFile"
    (Quote-TaskArgument $EnvFile)
    "-Detach"
)

if ($Release) {
    $argumentParts += "-Release"
}

if ($SkipTunnel) {
    $argumentParts += "-SkipTunnel"
} else {
    $argumentParts += "-TunnelUser"
    $argumentParts += (Quote-TaskArgument $TunnelUser)
    $argumentParts += "-TunnelHost"
    $argumentParts += (Quote-TaskArgument $TunnelHost)
}

$action = New-ScheduledTaskAction `
    -Execute "powershell.exe" `
    -Argument ($argumentParts -join " ")

$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew `
    -StartWhenAvailable
$principal = New-ScheduledTaskPrincipal `
    -UserId "$env:USERDOMAIN\$env:USERNAME" `
    -LogonType Interactive `
    -RunLevel Limited

Register-ScheduledTask `
    -TaskName $TaskName `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Principal $principal `
    -Description "Starts the Elowen laptop edge wrapper at logon." `
    -Force `
    -ErrorAction Stop | Out-Null

Write-Host "Registered scheduled task $TaskName"
Write-Host "Start it now with: Start-ScheduledTask -TaskName $TaskName"
