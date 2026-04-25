[CmdletBinding()]
param(
    [string]$InstallDir,
    [string]$ConfigSource,
    [string]$SecretSourceDir,
    [string]$TaskName = "ElowenEdge",
    [switch]$Release,
    [switch]$Start,
    [switch]$SkipTask,
    [switch]$SkipShortcut,
    [switch]$SkipTunnel,
    [string]$TunnelUser,
    [string]$TunnelHost
)

$ErrorActionPreference = "Stop"

if (-not $InstallDir) {
    if ($env:LOCALAPPDATA) {
        $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\Elowen\Edge"
    } else {
        $InstallDir = Join-Path $HOME "Elowen\Edge"
    }
}

$InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)
$payloadBase64 = "__ELOWEN_EDGE_PAYLOAD_BASE64__"
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) "elowen-edge-install-$PID"
$zipPath = Join-Path $tempRoot "payload.zip"
$extractRoot = Join-Path $tempRoot "payload"

function Copy-IfProvided {
    param(
        [string]$Source,
        [string]$Destination
    )

    if (-not $Source) {
        return $false
    }

    if (-not (Test-Path -LiteralPath $Source)) {
        throw "Source path not found: $Source"
    }

    $parent = Split-Path -Parent $Destination
    if (-not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }

    Copy-Item -LiteralPath $Source -Destination $Destination -Force
    return $true
}

try {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
    [IO.File]::WriteAllBytes($zipPath, [Convert]::FromBase64String($payloadBase64))
    Expand-Archive -LiteralPath $zipPath -DestinationPath $extractRoot -Force

    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    }

    if (-not (Test-Path -LiteralPath $InstallDir)) {
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    }

    Get-ChildItem -LiteralPath $extractRoot -Force | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $InstallDir -Recurse -Force
    }

    $configPath = Join-Path $InstallDir "edge.toml"
    if ($ConfigSource) {
        Copy-IfProvided -Source $ConfigSource -Destination $configPath | Out-Null
    } elseif (-not (Test-Path -LiteralPath $configPath)) {
        Copy-Item -LiteralPath (Join-Path $InstallDir "edge.toml.example") -Destination $configPath -Force
    }

    if ($SecretSourceDir) {
        if (-not (Test-Path -LiteralPath $SecretSourceDir)) {
            throw "Secret source directory not found: $SecretSourceDir"
        }
        $secretDestination = Join-Path $InstallDir "secrets"
        if (-not (Test-Path -LiteralPath $secretDestination)) {
            New-Item -ItemType Directory -Force -Path $secretDestination | Out-Null
        }
        $secretItems = @(Get-ChildItem -LiteralPath $SecretSourceDir -Force)
        if ($secretItems.Count -eq 0) {
            throw "Secret source directory is empty: $SecretSourceDir"
        }
        foreach ($item in $secretItems) {
            Copy-Item -LiteralPath $item.FullName -Destination $secretDestination -Recurse -Force
        }
    }

    $binaryPath = Join-Path $InstallDir "elowen-edge.exe"
    $registerScript = Join-Path $InstallDir "scripts\windows\Register-ElowenEdgeTask.ps1"
    $shortcutScript = Join-Path $InstallDir "scripts\windows\Install-ElowenEdgeTuiShortcut.ps1"

    if (-not $SkipTask) {
        $registerArgs = @{
            TaskName = $TaskName
            ConfigFile = $configPath
            BinaryPath = $binaryPath
        }
        if ($SkipTunnel) {
            $registerArgs.SkipTunnel = $true
        } else {
            if ($TunnelUser) { $registerArgs.TunnelUser = $TunnelUser }
            if ($TunnelHost) { $registerArgs.TunnelHost = $TunnelHost }
        }
        & $registerScript @registerArgs
    }

    if (-not $SkipShortcut) {
        & $shortcutScript -ConfigFile $configPath -BinaryPath $binaryPath
    }

    if ($Start -and -not $SkipTask) {
        Start-ScheduledTask -TaskName $TaskName
    }

    Write-Host "Elowen Edge installed to $InstallDir"
    Write-Host "Config: $configPath"
    Write-Host "TUI: $binaryPath tui --config `"$configPath`""
} finally {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
