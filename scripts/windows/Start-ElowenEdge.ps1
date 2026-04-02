[CmdletBinding()]
param(
    [string]$EnvFile,
    [string]$TunnelUser,
    [string]$TunnelHost,
    [int]$TunnelLocalPort = 4222,
    [int]$TunnelRemotePort = 4222,
    [string]$BinaryPath,
    [switch]$Release,
    [switch]$SkipTunnel,
    [switch]$Detach
)

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

if (-not $EnvFile) {
    $EnvFile = Join-Path $repoRoot "edge.env.local"
}

if (-not (Test-Path -LiteralPath $EnvFile)) {
    throw "Edge env file not found: $EnvFile"
}

if (-not $BinaryPath) {
    $profile = if ($Release) { "release" } else { "debug" }
    $BinaryPath = Join-Path $repoRoot "target\$profile\elowen-edge.exe"
}

if (-not (Test-Path -LiteralPath $BinaryPath)) {
    throw "Edge binary not found: $BinaryPath"
}

$tunnelProcess = $null

if (-not $SkipTunnel) {
    if (-not $TunnelUser -or -not $TunnelHost) {
        throw "TunnelUser and TunnelHost are required unless -SkipTunnel is set."
    }

    $sshArgs = @(
        "-N"
        "-L"
        "${TunnelLocalPort}:127.0.0.1:${TunnelRemotePort}"
        "${TunnelUser}@${TunnelHost}"
    )

    $tunnelProcess = Start-Process `
        -FilePath "ssh.exe" `
        -ArgumentList $sshArgs `
        -PassThru `
        -WindowStyle Hidden

    Write-Host "Started SSH tunnel PID $($tunnelProcess.Id)"
    Start-Sleep -Seconds 2
}

$edgeArgs = @("--env-file", $EnvFile)

if ($Detach) {
    $edgeProcess = Start-Process `
        -FilePath $BinaryPath `
        -ArgumentList $edgeArgs `
        -WorkingDirectory $repoRoot `
        -PassThru

    Write-Host "Started elowen-edge PID $($edgeProcess.Id)"
    if ($tunnelProcess) {
        Write-Host "SSH tunnel remains detached as PID $($tunnelProcess.Id)"
    }
    return
}

try {
    & $BinaryPath @edgeArgs
}
finally {
    if ($tunnelProcess) {
        Stop-Process -Id $tunnelProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
