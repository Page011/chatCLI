<#
.SYNOPSIS
  Samples the memory of one app, summed over its whole process tree, to a CSV, then prints
  min / median / peak / end. Method and the fixed session: docs/benchmark.md.

.DESCRIPTION
  Desktop chat apps spawn many helper processes, so the parent alone under-reports. Every
  sample adds up WorkingSet and Private bytes of the root process and all its descendants.

  This script only WATCHES. It never signs in, sends or types anything. Two ways to point it:
    -Launch <exe> [-LaunchArgs ...]   start the app itself (use this for `chatcli --demo`)
    -ProcessName <name>               attach to an app you already started and signed in
                                      (no .exe; every process of that name is a root)
  Sessions that need an account (a live chatcli, Teams, WhatsApp desktop) are started and
  driven by you, by hand, on a test account. The script prints each phase when it begins.

.EXAMPLE
  .\scripts\memory-bench.ps1 -Label chatcli-demo -Launch .\chatcli.exe -LaunchArgs '--demo' -BuildProfile release
.EXAMPLE
  .\scripts\memory-bench.ps1 -Label desktop-app -ProcessName ms-teams -AppVersion 'as shown in About'
.EXAMPLE
  .\scripts\memory-bench.ps1 -Summarize .\bench-out\chatcli-demo-run1.csv
#>
[CmdletBinding(DefaultParameterSetName = 'Run')]
param(
  [Parameter(ParameterSetName = 'Run', Mandatory)] [string] $Label,
  [Parameter(ParameterSetName = 'Run')] [string] $Launch,
  [Parameter(ParameterSetName = 'Run')] [string[]] $LaunchArgs = @(),
  [Parameter(ParameterSetName = 'Run')] [string] $ProcessName,
  # Free text recorded in the metadata: the app's version, as the app reports it.
  [Parameter(ParameterSetName = 'Run')] [string] $AppVersion = 'not recorded',
  # release or debug for a chatcli build; "n/a" for apps you did not build.
  [Parameter(ParameterSetName = 'Run')] [string] $BuildProfile = 'not recorded',
  # name:seconds,... in order. Default is the fixed session from docs/benchmark.md.
  [Parameter(ParameterSetName = 'Run')]
  [string] $Phases = 'idle-start:60,open-and-scroll:300,receive:600,idle-end:300',
  [Parameter(ParameterSetName = 'Run')] [ValidateRange(1, 60)] [int] $IntervalSeconds = 5,
  [Parameter(ParameterSetName = 'Run')] [string] $OutDir = '.\bench-out',
  # Existing CSV to summarize; nothing is sampled.
  [Parameter(ParameterSetName = 'Summarize', Mandatory)] [string] $Summarize
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---- pure helpers (also used by the self-check at the bottom) --------------------------------

# "a:60,b:5" -> @(@{Name='a';Seconds=60}, ...). Throws a one-sentence error on bad input.
function ConvertTo-Phases([string] $Text) {
  $out = @()
  foreach ($part in $Text.Split(',')) {
    if ($part -notmatch '^\s*([A-Za-z0-9_-]+)\s*:\s*(\d+)\s*$') {
      throw "Phase '$part' is not name:seconds. Example: idle:60,work:300"
    }
    $out += [pscustomobject]@{ Name = $Matches[1]; Seconds = [int]$Matches[2] }
  }
  if ($out.Count -eq 0) { throw 'No phases given.' }
  return , $out
}

# The phase name at an elapsed time; the last phase for anything after the end.
function Get-PhaseAt([object[]] $PhaseList, [double] $Elapsed) {
  $end = 0
  foreach ($p in $PhaseList) {
    $end += $p.Seconds
    if ($Elapsed -lt $end) { return $p.Name }
  }
  return $PhaseList[-1].Name
}

# Roots plus every descendant, from a flat list of (ProcessId, ParentProcessId). Cycle-safe:
# a pid is added once, so a recycled parent id cannot loop.
function Get-Tree([object[]] $Procs, [int[]] $RootIds) {
  $kids = @{}
  foreach ($p in $Procs) {
    $k = [int]$p.ParentProcessId
    if (-not $kids.ContainsKey($k)) { $kids[$k] = New-Object System.Collections.Generic.List[int] }
    $kids[$k].Add([int]$p.ProcessId)
  }
  $seen = New-Object System.Collections.Generic.HashSet[int]
  $queue = New-Object System.Collections.Generic.Queue[int]
  foreach ($r in $RootIds) { if ($seen.Add($r)) { $queue.Enqueue($r) } }
  while ($queue.Count -gt 0) {
    $id = $queue.Dequeue()
    if ($kids.ContainsKey($id)) {
      foreach ($c in $kids[$id]) { if ($seen.Add($c)) { $queue.Enqueue($c) } }
    }
  }
  return @($seen)
}

function Get-Median([double[]] $Values) {
  $s = $Values | Sort-Object
  $n = @($s).Count
  if ($n -eq 0) { return 0 }
  if ($n % 2 -eq 1) { return [double]$s[($n - 1) / 2] }
  return ([double]$s[$n / 2 - 1] + [double]$s[$n / 2]) / 2
}

# min / median / peak / end of one numeric column. "end" = the last sample.
function Get-Stats([double[]] $Values) {
  $m = $Values | Measure-Object -Minimum -Maximum
  return [pscustomobject]@{
    Min    = [math]::Round($m.Minimum, 1)
    Median = [math]::Round((Get-Median $Values), 1)
    Peak   = [math]::Round($m.Maximum, 1)
    End    = [math]::Round($Values[-1], 1)
  }
}

function Write-Summary([string] $CsvPath) {
  $rows = @(Import-Csv -LiteralPath $CsvPath)
  if ($rows.Count -eq 0) { throw "$CsvPath has no samples." }
  $ws = Get-Stats ([double[]]($rows | ForEach-Object { [double]::Parse($_.WorkingSetMB, [cultureinfo]::InvariantCulture) }))
  $pv = Get-Stats ([double[]]($rows | ForEach-Object { [double]::Parse($_.PrivateMB, [cultureinfo]::InvariantCulture) }))
  $maxProc = ($rows | ForEach-Object { [int]$_.Processes } | Measure-Object -Maximum).Maximum
  Write-Host ''
  Write-Host ("Samples: {0}   Largest process count: {1}" -f $rows.Count, $maxProc)
  Write-Host 'MB, summed over the whole process tree:'
  Write-Host ("  Working set   min {0}  median {1}  peak {2}  end {3}" -f $ws.Min, $ws.Median, $ws.Peak, $ws.End)
  Write-Host ("  Private bytes min {0}  median {1}  peak {2}  end {3}" -f $pv.Min, $pv.Median, $pv.Peak, $pv.End)
  return [pscustomobject]@{ WorkingSet = $ws; Private = $pv; Samples = $rows.Count; MaxProcesses = $maxProc }
}

# ---- one sample ----------------------------------------------------------------------------

# Returns Processes / WorkingSetMB / PrivateMB for the tree under the roots. One CIM query
# gives both numbers for every process, so the sum is one consistent snapshot.
function Get-TreeSample([scriptblock] $FindRoots) {
  $all = @(Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, WorkingSetSize, PrivatePageCount)
  $roots = @(& $FindRoots $all)
  if ($roots.Count -eq 0) { return $null }
  $ids = Get-Tree $all $roots
  $set = @{}
  foreach ($i in $ids) { $set[$i] = $true }
  $ws = 0.0; $pv = 0.0; $n = 0
  foreach ($p in $all) {
    if ($set.ContainsKey([int]$p.ProcessId)) {
      $ws += [double]$p.WorkingSetSize
      $pv += [double]$p.PrivatePageCount
      $n++
    }
  }
  return [pscustomobject]@{ Processes = $n; WorkingSetMB = $ws / 1MB; PrivateMB = $pv / 1MB }
}

# ---- main ----------------------------------------------------------------------------------

if ($PSCmdlet.ParameterSetName -eq 'Summarize') {
  $null = Write-Summary $Summarize
  return
}

if ([bool]$Launch -eq [bool]$ProcessName) {
  throw 'Give exactly one of -Launch <exe> or -ProcessName <name>.'
}
if ($ProcessName -and $ProcessName -match '\.exe$') {
  throw "-ProcessName is the name without .exe (for example ms-teams)."
}
$phaseList = ConvertTo-Phases $Phases
$total = ($phaseList | Measure-Object -Property Seconds -Sum).Sum

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$csv = Join-Path $OutDir "$Label-$stamp.csv"
$meta = Join-Path $OutDir "$Label-$stamp.meta.txt"

$os = Get-CimInstance Win32_OperatingSystem
@(
  "label:         $Label"
  "started:       $((Get-Date).ToString('s'))"
  "os:            $($os.Caption) $($os.Version) build $($os.BuildNumber)"
  "app version:   $AppVersion"
  "build profile: $BuildProfile"
  "target:        $(if ($Launch) { "launched: $(Split-Path -Leaf $Launch) $($LaunchArgs -join ' ')" } else { "attached to process name: $ProcessName" })"
  "phases:        $Phases"
  "interval:      $IntervalSeconds s"
  "measures:      working set and private bytes, summed over the process tree. Not measured: CPU, battery, network."
) | Set-Content -LiteralPath $meta -Encoding UTF8

$started = $null
if ($Launch) {
  if (-not (Test-Path -LiteralPath $Launch)) { throw "$Launch not found." }
  $startArgs = @{ FilePath = $Launch; PassThru = $true }
  if ($LaunchArgs.Count -gt 0) { $startArgs.ArgumentList = $LaunchArgs }
  $started = Start-Process @startArgs
  $rootId = $started.Id
  $findRoots = { param($all) if ($all | Where-Object { [int]$_.ProcessId -eq $rootId }) { @($rootId) } else { @() } }.GetNewClosure()
} else {
  $name = $ProcessName
  $findRoots = { param($all) @(Get-Process -Name $name -ErrorAction SilentlyContinue | ForEach-Object { $_.Id }) }.GetNewClosure()
  if (@(& $findRoots @()).Count -eq 0) { throw "No process named '$ProcessName' is running. Start and sign in to the app first." }
}

"ElapsedSec,Phase,Processes,WorkingSetMB,PrivateMB" | Set-Content -LiteralPath $csv -Encoding ASCII
Write-Host "Sampling every $IntervalSeconds s for $total s. Writing $csv"
$clock = [System.Diagnostics.Stopwatch]::StartNew()
$lastPhase = ''
$next = 0.0
try {
  while ($clock.Elapsed.TotalSeconds -lt $total) {
    $elapsed = $clock.Elapsed.TotalSeconds
    $phase = Get-PhaseAt $phaseList $elapsed
    if ($phase -ne $lastPhase) {
      Write-Host ("[{0,5:N0} s] PHASE: {1}" -f $elapsed, $phase)
      $lastPhase = $phase
    }
    $s = Get-TreeSample $findRoots
    if ($null -eq $s) {
      Write-Warning 'The app is gone: stopping early. The CSV holds what was sampled.'
      break
    }
    # Invariant culture: no thousands separator or decimal comma may break the CSV.
    [string]::Format([cultureinfo]::InvariantCulture, '{0:F1},{1},{2},{3:F1},{4:F1}',
      $elapsed, $phase, $s.Processes, $s.WorkingSetMB, $s.PrivateMB) |
      Add-Content -LiteralPath $csv -Encoding ASCII
    $next += $IntervalSeconds
    $wait = $next - $clock.Elapsed.TotalSeconds
    if ($wait -gt 0) { Start-Sleep -Milliseconds ([int]($wait * 1000)) }
  }
} finally {
  # Only a process this script started is stopped, by id, with its children. An app you
  # attached to is left running.
  if ($started -and -not $started.HasExited) {
    & taskkill.exe /PID $started.Id /T /F | Out-Null
  }
}

$null = Write-Summary $csv
Write-Host "Metadata: $meta"
