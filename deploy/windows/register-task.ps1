# Registers a daily "bdg run --once" job in Windows Task Scheduler.
# Keep this file ASCII-only: Windows PowerShell 5.1 misreads UTF-8 (no BOM) Japanese text.
#
# Usage (as the user who can read the secret files, not elevated):
#   powershell -ExecutionPolicy Bypass -File register-task.ps1 -Exe F:\backup-to-discord-by-github\bin\bdg.exe -Config F:\backup-to-discord-by-github\bdg.toml
# Add -WhatIf to see what would be registered without changing anything.
# Remove later with: Unregister-ScheduledTask -TaskName bdg-backup-to-discord -Confirm:$false
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [Parameter(Mandatory = $true)][string]$Config,
  [string]$Time = '03:30',
  [string]$TaskName = 'bdg-backup-to-discord'
)
if (-not (Test-Path $Exe)) { throw "Exe not found: $Exe" }
if (-not (Test-Path $Config)) { throw "Config not found: $Config" }
$action = New-ScheduledTaskAction -Execute $Exe -Argument "--config `"$Config`" run --once" -WorkingDirectory (Split-Path $Config)
$trigger = New-ScheduledTaskTrigger -Daily -At $Time
# If the PC was off at the scheduled time, run as soon as it is available. Wait for the network.
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -RunOnlyIfNetworkAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Hours 6)
if ($PSCmdlet.ShouldProcess($TaskName, 'Register scheduled task')) {
  Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings -Description 'Encrypted GitHub backup to a private Discord server (bdg run --once)' -Force | Out-Null
  Write-Host "registered: $TaskName (daily $Time)"
}
