[CmdletBinding()]
param(
    [string]$ConfigFile,
    [string]$TunnelUser,
    [string]$TunnelHost,
    [int]$TunnelLocalPort = 4222,
    [int]$TunnelRemotePort = 4222,
    [string]$BinaryPath,
    [switch]$Release,
    [switch]$SkipTunnel,
    [switch]$Detach,
    [switch]$RunLoop,
    [int]$RestartDelaySeconds = 10,
    [int]$PollIntervalSeconds = 5,
    [string]$LogDirectory
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

if ($Detach -and $RunLoop) {
    throw "-Detach and -RunLoop cannot be used together."
}

if ($RestartDelaySeconds -lt 1) {
    throw "RestartDelaySeconds must be at least 1."
}

if ($PollIntervalSeconds -lt 1) {
    throw "PollIntervalSeconds must be at least 1."
}

function Resolve-WrapperLogDirectory {
    if ($LogDirectory) {
        return $LogDirectory
    }

    if ($env:LOCALAPPDATA) {
        return Join-Path $env:LOCALAPPDATA "Elowen\edge"
    }

    if ($env:ProgramData) {
        return Join-Path $env:ProgramData "Elowen\edge"
    }

    return Join-Path $repoRoot ".elowen\logs"
}

function Write-WrapperMessage {
    param(
        [string]$Message,
        [string]$Level = "INFO"
    )

    $timestamp = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    Write-Host "[$timestamp] [$Level] $Message"
}

function New-LogFilePath {
    param(
        [string]$Directory,
        [string]$Name,
        [string]$Stream
    )

    Join-Path $Directory "$Name-$Stream.log"
}

function Start-ManagedProcess {
    param(
        [string]$Name,
        [string]$FilePath,
        [string[]]$ArgumentList,
        [string]$WorkingDirectory,
        [string]$LogDirectory
    )

    $startParams = @{
        FilePath = $FilePath
        ArgumentList = $ArgumentList
        WorkingDirectory = $WorkingDirectory
        PassThru = $true
        WindowStyle = "Hidden"
    }

    if ($LogDirectory) {
        $startParams.RedirectStandardOutput = (New-LogFilePath -Directory $LogDirectory -Name $Name -Stream "stdout")
        $startParams.RedirectStandardError = (New-LogFilePath -Directory $LogDirectory -Name $Name -Stream "stderr")
    }

    $process = Start-Process @startParams
    Write-WrapperMessage "Started $Name PID $($process.Id)"
    return $process
}

function Stop-ManagedProcess {
    param(
        [System.Diagnostics.Process]$Process,
        [string]$Name
    )

    if (-not $Process) {
        return
    }

    try {
        $Process.Refresh()
        if (-not $Process.HasExited) {
            Stop-Process -Id $Process.Id -Force -ErrorAction Stop
            Write-WrapperMessage "Stopped $Name PID $($Process.Id)" "WARN"
        }
    }
    catch {
        Write-WrapperMessage "Failed to stop $Name cleanly: $($_.Exception.Message)" "WARN"
    }
}

function Invoke-EdgePair {
    param([string]$RuntimeLogDirectory)

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

        $tunnelProcess = Start-ManagedProcess `
            -Name "ssh-tunnel" `
            -FilePath "ssh.exe" `
            -ArgumentList $sshArgs `
            -WorkingDirectory $repoRoot `
            -LogDirectory $RuntimeLogDirectory

        Start-Sleep -Seconds 2
    }

    $edgeArgs = @("run", "--config", $ConfigFile)
    $edgeProcess = Start-ManagedProcess `
        -Name "elowen-edge" `
        -FilePath $BinaryPath `
        -ArgumentList $edgeArgs `
        -WorkingDirectory $repoRoot `
        -LogDirectory $RuntimeLogDirectory

    if ($Detach) {
        if ($tunnelProcess) {
            Write-WrapperMessage "SSH tunnel remains detached as PID $($tunnelProcess.Id)"
        }
        return @{
            Detached = $true
            EdgeExitCode = $null
            TunnelExitCode = $null
            ExitedComponent = "detached"
        }
    }

    try {
        while ($true) {
            Start-Sleep -Seconds $PollIntervalSeconds
            $edgeProcess.Refresh()
            if ($edgeProcess.HasExited) {
                $edgeExitCode = $edgeProcess.ExitCode
                Write-WrapperMessage "elowen-edge exited with code $edgeExitCode" "WARN"
                Stop-ManagedProcess -Process $tunnelProcess -Name "ssh-tunnel"
                return @{
                    Detached = $false
                    EdgeExitCode = $edgeExitCode
                    TunnelExitCode = $null
                    ExitedComponent = "edge"
                }
            }

            if ($tunnelProcess) {
                $tunnelProcess.Refresh()
                if ($tunnelProcess.HasExited) {
                    $tunnelExitCode = $tunnelProcess.ExitCode
                    Write-WrapperMessage "ssh tunnel exited with code $tunnelExitCode" "WARN"
                    Stop-ManagedProcess -Process $edgeProcess -Name "elowen-edge"
                    return @{
                        Detached = $false
                        EdgeExitCode = $null
                        TunnelExitCode = $tunnelExitCode
                        ExitedComponent = "tunnel"
                    }
                }
            }
        }
    }
    finally {
        Stop-ManagedProcess -Process $edgeProcess -Name "elowen-edge"
        Stop-ManagedProcess -Process $tunnelProcess -Name "ssh-tunnel"
    }
}

if ($RunLoop) {
    $runtimeLogDirectory = Resolve-WrapperLogDirectory
    New-Item -ItemType Directory -Path $runtimeLogDirectory -Force | Out-Null
    Write-WrapperMessage "Running elowen-edge under wrapper supervision"
    Write-WrapperMessage "Wrapper logs directory: $runtimeLogDirectory"

    while ($true) {
        $result = Invoke-EdgePair -RuntimeLogDirectory $runtimeLogDirectory
        if ($result.Detached) {
            return
        }

        Write-WrapperMessage "Restarting after $RestartDelaySeconds seconds because $($result.ExitedComponent) exited" "WARN"
        Start-Sleep -Seconds $RestartDelaySeconds
    }

    return
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

    Write-WrapperMessage "Started ssh tunnel PID $($tunnelProcess.Id)"
    Start-Sleep -Seconds 2
}

$edgeArgs = @("run", "--config", $ConfigFile)

if ($Detach) {
    $detachedLogDirectory = Resolve-WrapperLogDirectory
    New-Item -ItemType Directory -Path $detachedLogDirectory -Force | Out-Null
    $edgeProcess = Start-ManagedProcess `
        -Name "elowen-edge" `
        -FilePath $BinaryPath `
        -ArgumentList $edgeArgs `
        -WorkingDirectory $repoRoot `
        -LogDirectory $detachedLogDirectory

    if ($tunnelProcess) {
        Write-WrapperMessage "SSH tunnel remains detached as PID $($tunnelProcess.Id)"
    }
    Write-WrapperMessage "Detached edge logs directory: $detachedLogDirectory"
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
