# What one process costs over a stretch of time: its processor time as a
# share of one core, its private memory and thread count at their highest,
# and what it wrote to disk, sampled every five seconds.
#
#   powershell -File measure.ps1 -Name relay-pp -Seconds 30
param(
    [Parameter(Mandatory)] [string] $Name,
    [int] $Seconds = 30
)

$process = Get-Process -Name $Name -ErrorAction Stop | Select-Object -First 1
$id = $process.Id
$written = { (Get-CimInstance Win32_Process -Filter "ProcessId=$id").WriteTransferCount }

$cpu0 = $process.TotalProcessorTime.TotalSeconds
$write0 = & $written
$t0 = Get-Date
$memory = 0
$threads = 0
for ($i = 0; $i -lt [math]::Ceiling($Seconds / 5); $i++) {
    Start-Sleep -Seconds 5
    $process.Refresh()
    $memory = [math]::Max($memory, $process.PrivateMemorySize64)
    $threads = [math]::Max($threads, $process.Threads.Count)
}
$process.Refresh()
$elapsed = ((Get-Date) - $t0).TotalSeconds
$cpu = 100 * ($process.TotalProcessorTime.TotalSeconds - $cpu0) / $elapsed
$writeMbps = ((& $written) - $write0) * 8 / 1e6 / $elapsed
"cpu_pct={0:N1} private_mb={1:N0} threads={2} write_mbps={3:N1}" -f $cpu, ($memory / 1MB), $threads, $writeMbps
