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

  -Token is accepted for convenience but is written into the per-user config
  file rather than the Scheduled Task's command line — a command line is
  visible to any local process (Get-CimInstance Win32_Process, Task
  Scheduler's UI, etc.), so a token passed there isn't a secret. If -Token is
  omitted and the config doesn't already have one, a strong random token is
  generated and printed once.
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
  if ($Token) { (Get-Content $configPath) -replace '^token = .*', "token = `"$Token`"" | Set-Content $configPath }
}

# Fail-open guard: an empty (or still-placeholder) token means the daemon
# refuses non-loopback binds. Generate a strong random one instead of shipping
# with auth disabled, unless a real token is already in place.
$existingToken = (Select-String -Path $configPath -Pattern '^token = "(.*)"' | ForEach-Object { $_.Matches[0].Groups[1].Value } | Select-Object -First 1)
if ([string]::IsNullOrEmpty($existingToken) -or $existingToken -like 'REPLACE_ME*') {
  $bytes = New-Object byte[] 24
  [System.Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
  $genToken = -join ($bytes | ForEach-Object { $_.ToString("x2") })
  (Get-Content $configPath) -replace '^token = .*', "token = `"$genToken`"" | Set-Content $configPath
  Write-Host "    generated a random bearer token (shown once):" -ForegroundColor Yellow
  Write-Host "        $genToken" -ForegroundColor Yellow
  Write-Host "    saved in $configPath - copy it into flotilla-hosts.toml for the TUI/web client"
}

# The config holds the bearer token + ntfy token - restrict it to the current
# user only (strip inherited ACEs, grant just this account full control).
icacls $configPath /inheritance:r | Out-Null
icacls $configPath /grant:r "$($env:USERDOMAIN)\$($env:USERNAME):(F)" | Out-Null

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
