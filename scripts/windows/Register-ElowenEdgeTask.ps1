[CmdletBinding()]
param(
    [string]$TaskName = "ElowenEdge",
    [string]$ConfigFile,
    [string]$TunnelUser,
    [string]$TunnelHost,
    [string]$BinaryPath,
    [switch]$Release,
    [switch]$SkipTunnel,
    [ValidateSet("Startup", "LogOn")]
    [string]$Trigger = "Startup",
    [ValidateSet("S4U", "Interactive")]
    [string]$LogonType = "S4U",
    [ValidateSet("Limited", "Highest")]
    [string]$RunLevel = "Limited",
    [string]$UserId = "$env:USERDOMAIN\$env:USERNAME",
    [int]$RestartDelaySeconds = 15,
    [string]$LogDirectory,
    [switch]$RequireServiceGrade,
    [switch]$NoInteractiveFallback
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$startScript = Join-Path $PSScriptRoot "Start-ElowenEdge.ps1"

if (-not $ConfigFile) {
    $ConfigFile = Join-Path $repoRoot "edge.toml"
}

if (-not (Test-Path -LiteralPath $ConfigFile)) {
    throw "Edge TOML config not found: $ConfigFile"
}

$ConfigFile = (Resolve-Path -LiteralPath $ConfigFile).Path

if (-not $SkipTunnel -and (-not $TunnelUser -or -not $TunnelHost)) {
    throw "TunnelUser and TunnelHost are required unless -SkipTunnel is set."
}

function Quote-TaskArgument {
    param([string]$Value)

    '"' + $Value.Replace('"', '\"') + '"'
}

function Test-IsElevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function New-TaskRegistrationPlan {
    param(
        [string]$PlanTrigger,
        [string]$PlanLogonType,
        [string]$PlanRunLevel,
        [string]$PlanUserId
    )

    [PSCustomObject]@{
        Trigger = $PlanTrigger
        LogonType = $PlanLogonType
        RunLevel = $PlanRunLevel
        UserId = $PlanUserId
    }
}

function Get-TaskRegistrationErrorMessage {
    param([System.Exception]$Exception)

    if ($Exception.InnerException) {
        return $Exception.InnerException.Message
    }

    return $Exception.Message
}

function ConvertTo-VbScriptString {
    param([string]$Value)

    '"' + $Value.Replace('"', '""') + '"'
}

function Write-HiddenPowerShellLauncher {
    param(
        [string]$LauncherPath,
        [string]$PowerShellArguments,
        [string]$WorkingDirectory
    )

    $powerShellPath = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
    if (-not (Test-Path -LiteralPath $powerShellPath)) {
        $powerShellPath = "powershell.exe"
    }

    $command = (Quote-TaskArgument $powerShellPath) + " " + $PowerShellArguments
    $launcher = @(
        'Set shell = CreateObject("WScript.Shell")'
        ('shell.CurrentDirectory = ' + (ConvertTo-VbScriptString $WorkingDirectory))
        ('exitCode = shell.Run(' + (ConvertTo-VbScriptString $command) + ', 0, True)')
        'WScript.Quit exitCode'
    ) -join "`r`n"

    Set-Content -LiteralPath $LauncherPath -Value $launcher -Encoding ASCII
}

function Register-WithPlan {
    param(
        [string]$Name,
        [string]$Description,
        [string]$TaskExecutable,
        [string]$TaskArguments,
        [string]$SelectedUserId,
        [pscustomobject]$Plan
    )

    $action = New-ScheduledTaskAction `
        -Execute $TaskExecutable `
        -Argument $TaskArguments

    $triggerObject = if ($Plan.Trigger -eq "Startup") {
        New-ScheduledTaskTrigger -AtStartup
    } else {
        New-ScheduledTaskTrigger -AtLogOn -User $SelectedUserId
    }

    $settings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -MultipleInstances IgnoreNew `
        -StartWhenAvailable `
        -ExecutionTimeLimit ([TimeSpan]::Zero) `
        -RestartCount 999 `
        -RestartInterval (New-TimeSpan -Minutes 1)
    $principal = New-ScheduledTaskPrincipal `
        -UserId $Plan.UserId `
        -LogonType $Plan.LogonType `
        -RunLevel $Plan.RunLevel

    Register-ScheduledTask `
        -TaskName $Name `
        -Action $action `
        -Trigger $triggerObject `
        -Settings $settings `
        -Principal $principal `
        -Description $Description `
        -Force `
        -ErrorAction Stop | Out-Null
}

$argumentParts = @(
    "-NoProfile"
    "-WindowStyle"
    "Hidden"
    "-ExecutionPolicy"
    "Bypass"
    "-File"
    (Quote-TaskArgument $startScript)
    "-ConfigFile"
    (Quote-TaskArgument $ConfigFile)
    "-RunLoop"
    "-RestartDelaySeconds"
    $RestartDelaySeconds.ToString()
)

if ($BinaryPath) {
    $BinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path
    $argumentParts += "-BinaryPath"
    $argumentParts += (Quote-TaskArgument $BinaryPath)
}

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

if ($LogDirectory) {
    $argumentParts += "-LogDirectory"
    $argumentParts += (Quote-TaskArgument $LogDirectory)
}

$taskArguments = $argumentParts -join " "
$launcherPath = Join-Path $PSScriptRoot "$TaskName.launcher.vbs"
Write-HiddenPowerShellLauncher `
    -LauncherPath $launcherPath `
    -PowerShellArguments $taskArguments `
    -WorkingDirectory $repoRoot
$taskExecutable = "wscript.exe"
$taskArguments = Quote-TaskArgument $launcherPath
$description = "Starts the Elowen laptop edge wrapper with a $Trigger scheduled task using $LogonType logon semantics."
$requestedPlan = New-TaskRegistrationPlan -PlanTrigger $Trigger -PlanLogonType $LogonType -PlanRunLevel $RunLevel -PlanUserId $UserId
$selectedPlan = $requestedPlan
$isElevated = Test-IsElevated
$canFallback = -not $RequireServiceGrade -and -not $NoInteractiveFallback

try {
    Register-WithPlan `
        -Name $TaskName `
        -Description $description `
        -TaskExecutable $taskExecutable `
        -TaskArguments $taskArguments `
        -SelectedUserId $UserId `
        -Plan $selectedPlan
}
catch {
    $errorMessage = Get-TaskRegistrationErrorMessage -Exception $_.Exception
    $requiresPrivilegedMode = $requestedPlan.Trigger -ne "LogOn" -or $requestedPlan.LogonType -ne "Interactive"

    if (-not $canFallback -or -not $requiresPrivilegedMode -or $errorMessage -notmatch "Access is denied") {
        throw
    }

    $selectedPlan = New-TaskRegistrationPlan `
        -PlanTrigger "LogOn" `
        -PlanLogonType "Interactive" `
        -PlanRunLevel "Limited" `
        -PlanUserId $UserId

    Write-Warning "Task registration for trigger=$($requestedPlan.Trigger) logon_type=$($requestedPlan.LogonType) failed with 'Access is denied'."
    if (-not $isElevated) {
        Write-Warning "The current PowerShell session is not elevated. Falling back to a per-user interactive logon task."
    } else {
        Write-Warning "Falling back to a per-user interactive logon task for this host."
    }

    $description = "Starts the Elowen laptop edge wrapper with a LogOn scheduled task using Interactive logon semantics. Fallback from requested trigger=$Trigger logon_type=$LogonType."

    Register-WithPlan `
        -Name $TaskName `
        -Description $description `
        -TaskExecutable $taskExecutable `
        -TaskArguments $taskArguments `
        -SelectedUserId $UserId `
        -Plan $selectedPlan
}

Write-Host "Registered scheduled task $TaskName"
Write-Host "Requested trigger: $Trigger"
Write-Host "Requested logon type: $LogonType"
Write-Host "Effective trigger: $($selectedPlan.Trigger)"
Write-Host "Effective user: $($selectedPlan.UserId)"
Write-Host "Effective logon type: $($selectedPlan.LogonType)"
Write-Host "Task executable: $taskExecutable"
Write-Host "Hidden launcher: $launcherPath"
Write-Host "PowerShell session elevated: $isElevated"
Write-Host "Start it now with: Start-ScheduledTask -TaskName $TaskName"
