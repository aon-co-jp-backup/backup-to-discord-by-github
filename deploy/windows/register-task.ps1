# Windowsのタスクスケジューラに、毎日 03:30 の `bdg run --once` を登録する。
# 使い方(管理者ではなく、秘密ファイルを読める自分のアカウントで):
#   powershell -ExecutionPolicy Bypass -File register-task.ps1 -Exe F:\bdg\bdg.exe -Config F:\bdg\bdg.toml
# 確認だけしたいときは -WhatIf を付ける。
[CmdletBinding(SupportsShouldProcess = $true)]
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [Parameter(Mandatory = $true)][string]$Config,
  [string]$Time = '03:30',
  [string]$TaskName = 'bdg-backup-to-discord'
)
$action  = New-ScheduledTaskAction -Execute $Exe -Argument "--config `"$Config`" run --once" -WorkingDirectory (Split-Path $Config)
$trigger = New-ScheduledTaskTrigger -Daily -At $Time
# 電源OFFで時刻を過ぎたら、起動後に実行。ネットワークが無ければ待つ。
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -RunOnlyIfNetworkAvailable -ExecutionTimeLimit (New-TimeSpan -Hours 6)
if ($PSCmdlet.ShouldProcess($TaskName, 'Register scheduled task')) {
  Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings -Description 'Encrypted GitHub backup to a private Discord server' | Out-Null
  Write-Host "registered: $TaskName (daily $Time)"
}
