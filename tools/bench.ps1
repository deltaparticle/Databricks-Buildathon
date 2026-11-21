param([string]$Data, [string]$Exe, [int]$Runs = 3, [int]$Threads = 8)
$env:DATA_DIR = $Data; $env:NCPU = "$Threads"; $env:SHOP_PROFILE = "1"
$out = "D:\Databricks Buildathon\solution\out"; $env:OUTPUT_DIR = $out
for ($i = 1; $i -le $Runs; $i++) {
  if (Test-Path $out) { Remove-Item -Recurse -Force $out }
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $Exe -NoNewWindow -PassThru -RedirectStandardError "D:\Databricks Buildathon\tmp\err.txt"
  $peak = 0; $commit = 0
  while (-not $p.HasExited) { try { $p.Refresh(); $peak = [Math]::Max($peak, $p.PeakWorkingSet64); $commit = [Math]::Max($commit, $p.PeakPagedMemorySize64) } catch {}; Start-Sleep -Milliseconds 20 }
  $sw.Stop()
  $phases = (Get-Content "D:\Databricks Buildathon\tmp\err.txt" | Where-Object { $_ -match 's (parse|dedup|sessions|write)$' }) -join ' |'
  "run {0}: {1:N3}s exit={2} peakRSS={3:N0}MB peakCommit={4:N0}MB  [{5}]" -f $i, $sw.Elapsed.TotalSeconds, $p.ExitCode, ($peak/1MB), ($commit/1MB), $phases
}
