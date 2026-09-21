<#
.SYNOPSIS
  Measures esMail's memory footprint while idle, against an isolated profile
  (no accounts, nothing of the real profile is read or written).

.DESCRIPTION
  Launches esmail.exe with ESMAIL_CONFIG_DIR / ESMAIL_DATA_DIR pointing at a
  scratch directory, samples working set, private (committed) memory, threads
  and loaded modules, then asks it to exit with `--quit`.

  Two modes:
    -Mode Gui         the normal window. After the first samples it closes the
                      window (what the close button does) and samples again, to
                      show what a hidden-to-tray process still holds.
    -Mode Background  `esmail.exe --background`, the resident listener (see
                      docs/BACKGROUND-LISTENER.md). No window to close.

  With -MaxPrivateMB the script exits 1 if the final private (committed) size
  is above it, so a CI job can enforce the budget.

  A window and a tray icon appear for the duration of a Gui run.

.EXAMPLE
  .\scripts\measure-idle.ps1 -Exe .\target\release\esmail.exe
  .\scripts\measure-idle.ps1 -Exe .\target\release\esmail.exe -Mode Background -MaxPrivateMB 15
#>
param(
    [Parameter(Mandatory)] [string]$Exe,
    [ValidateSet('Gui', 'Background')] [string]$Mode = 'Gui',
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
        GlLoaded     = [bool]($p.Modules | Where-Object { $_.ModuleName -match '^(opengl32|nvoglv|atio|ig.*icd|libGL)' })
    }
    '{0,-28} workingset={1,6} MB  private={2,6} MB  threads={3,3}  handles={4,4}  GL driver loaded={5}' -f `
        $label, $last.WorkingSetMB, $last.PrivateMB, $last.Threads, $last.Handles, $last.GlLoaded
}

$arguments = if ($Mode -eq 'Background') { @('--background') } else { @() }
$p = Start-Process -FilePath $Exe -ArgumentList $arguments -PassThru
try {
    Start-Sleep 3
    Sample "$Mode, 3 s after start" $p
    Start-Sleep $SettleSeconds
    Sample "$Mode, $($SettleSeconds + 3) s after start" $p
    if ($Mode -eq 'Gui') {
        $null = $p.CloseMainWindow()
        Start-Sleep 2
        Sample 'window closed, +2 s' $p
        Start-Sleep $SettleSeconds
        Sample "window closed, +$($SettleSeconds + 2) s" $p
    }
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
