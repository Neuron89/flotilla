<#
.SYNOPSIS
  Install flotilla-agent on a Windows host (desktop / Windows laptop) as a
  Scheduled Task that starts at logon and restarts on failure.

.NOTES
  Run from the repo root in PowerShell:
      .\deploy\install-windows.ps1
  A console daemon doesn't behave well under the Windows Service Manager, so we
  use a logon-triggered Scheduled Task instead — it runs in your user context so
  spawned Claude/Hermes sessions inherit your PATH and ~/.claude config.
#>
param(
  [int]$Port = 7878,
  [string]$Token = ""
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

# dlltool from the Rust GNU toolchain must be on PATH to build here.
$sc = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
if (Test-Path $sc) { $env:Path = "$env:USERPROFILE\.cargo\bin;$sc;$env:Path" }

Write-Host "==> Building release binary" -ForegroundColor Cyan
Push-Location $repo
cargo build --release -p flotilla-agent
Pop-Location

$installDir = Join-Path $env:LOCALAPPDATA "Flotilla"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Copy-Item "$repo\target\release\flotilla-agent.exe" "$installDir\flotilla-agent.exe" -Force

$configPath = Join-Path $installDir "flotilla.toml"
if (-not (Test-Path $configPath)) {
  Copy-Item "$repo\config\flotilla.example.toml" $configPath
  (Get-Content $configPath) -replace '^host_label = .*', "host_label = `"$env:COMPUTERNAME`"" |
    Set-Content $configPath
  if ($Token) { (Get-Content $configPath) -replace '^token = .*', "token = `"$Token`"" | Set-Content $configPath }
  Write-Host "    wrote $configPath"
} else {
  Write-Host "    $configPath exists - leaving it"
}

$taskName = "FlotillaAgent"
$action = New-ScheduledTaskAction -Execute "$installDir\flotilla-agent.exe" `
  -Argument "--config `"$configPath`" --bind 0.0.0.0:$Port"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
  -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit (New-TimeSpan -Seconds 0)

Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings `
  -Description "Flotilla agent - AI session daemon" -RunLevel Limited | Out-Null

Start-ScheduledTask -TaskName $taskName
Start-Sleep -Seconds 1
Write-Host "==> Installed and started scheduled task '$taskName'" -ForegroundColor Green
Write-Host "Tailnet URL for the fleet:  http://$env:COMPUTERNAME`:$Port"
Write-Host "Manage:  Get-ScheduledTask FlotillaAgent | Stop-ScheduledTask"
