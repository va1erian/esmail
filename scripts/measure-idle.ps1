<#
.SYNOPSIS
  Measures the native frontend's memory footprint while idle, against an
  isolated profile (no accounts, nothing of the real profile is read or
  written).

.DESCRIPTION
  Launches esmail-win32.exe with ESMAIL_CONFIG_DIR / ESMAIL_DATA_DIR pointing at
  a scratch directory, samples working set, private (committed) memory, threads
  and handles, closes the window to show what a hidden-to-tray process still
  holds, then asks it to exit with `--quit`.

  With -MaxPrivateMB the script exits 1 if the final private (committed) size
  is above it, so a CI job can enforce the budget.

  A window and a tray icon appear for the duration of the run.

.EXAMPLE
  .\scripts\measure-idle.ps1 -Exe .\target\release\esmail-win32.exe
  .\scripts\measure-idle.ps1 -Exe .\target\release\esmail-win32.exe -MaxPrivateMB 15
#>
param(
    [Parameter(Mandatory)] [string]$Exe,
    [double]$MaxPrivateMB = 0,
    [int]$SettleSeconds = 8
)

$Exe = (Resolve-Path $Exe).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) "esmail-measure-$PID"
$cfg = Join-Path $scratch 'cfg'
$data = Join-Path $scratch 'data'
New-Item -ItemType Directory -Force $cfg, $data | Out-Null
$env:ESMAIL_CONFIG_DIR = $cfg
$env:ESMAIL_DATA_DIR = $data

function Sample([string]$label, $p) {
    $p.Refresh()
    $script:last = [pscustomobject]@{
        WorkingSetMB = [math]::Round($p.WorkingSet64 / 1MB, 1)
        PrivateMB    = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        Threads      = $p.Threads.Count
        Handles      = $p.HandleCount
    }
    '{0,-28} workingset={1,6} MB  private={2,6} MB  threads={3,3}  handles={4,4}' -f `
        $label, $last.WorkingSetMB, $last.PrivateMB, $last.Threads, $last.Handles
}

$p = Start-Process -FilePath $Exe -PassThru
try {
    Start-Sleep 3
    Sample '3 s after start' $p
    Start-Sleep $SettleSeconds
    Sample "$($SettleSeconds + 3) s after start" $p
    $null = $p.CloseMainWindow()
    Start-Sleep 2
    Sample 'window closed, +2 s' $p
    Start-Sleep $SettleSeconds
    Sample "window closed, +$($SettleSeconds + 2) s" $p
    $cpuBefore = $p.TotalProcessorTime
    Start-Sleep 5
    $p.Refresh()
    $cpuMs = [math]::Round(($p.TotalProcessorTime - $cpuBefore).TotalMilliseconds)
    "idle CPU over 5 s: $cpuMs ms"
}
finally {
    & $Exe --quit
    Start-Sleep 3
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
}

if ($MaxPrivateMB -gt 0 -and $last.PrivateMB -gt $MaxPrivateMB) {
    "FAIL: private memory $($last.PrivateMB) MB is above the budget of $MaxPrivateMB MB"
    exit 1
}
