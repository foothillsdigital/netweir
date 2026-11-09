# What the machine's sockets and Chrome processes look like, for a test
# that failed with net::ERR_NO_BUFFER_SPACE (WSAENOBUFS) on Windows. The
# Python and Rust tests both print it then; nothing runs it otherwise.

$tcp = Get-NetTCPConnection -ErrorAction SilentlyContinue
$dynamic = netsh int ipv4 show dynamicport tcp | Select-String 'Start Port|Number of Ports'
"TCP: $($tcp.Count) in all; " + (($tcp | Group-Object State | Sort-Object Count -Descending |
    ForEach-Object { "$($_.Name) $($_.Count)" }) -join ', ')
"Ephemeral ports in use: $(($tcp | Where-Object LocalPort -ge 49152 | Measure-Object).Count) ($($dynamic -join '; '))"
"UDP endpoints: $((Get-NetUDPEndpoint -ErrorAction SilentlyContinue | Measure-Object).Count)"
"Most TCP by process: " + (($tcp | Group-Object OwningProcess | Sort-Object Count -Descending |
    Select-Object -First 5 | ForEach-Object {
        "$((Get-Process -Id $_.Name -ErrorAction SilentlyContinue).ProcessName)($($_.Name)) $($_.Count)"
    }) -join ', ')

$memory = Get-CimInstance Win32_PerfFormattedData_PerfOS_Memory
"Nonpaged pool: $([math]::Round($memory.PoolNonpagedBytes / 1MB)) MB; available memory: $($memory.AvailableMBytes) MB"
$processes = Get-Process
"Handles: $(($processes | Measure-Object HandleCount -Sum).Sum) in all; most: " + (($processes |
    Sort-Object HandleCount -Descending | Select-Object -First 5 |
    ForEach-Object { "$($_.ProcessName)($($_.Id)) $($_.HandleCount)" }) -join ', ')

# Each browser is the chrome.exe (or firefox.exe) whose parent isn't one:
# the test process that started it, or one that has already ended.
foreach ($name in 'chrome.exe', 'firefox.exe') {
    $all = @(Get-CimInstance Win32_Process -Filter "Name = '$name'")
    $ids = $all | ForEach-Object { $_.ProcessId }
    $browsers = @($all | Where-Object { $ids -notcontains $_.ParentProcessId })
    "${name}: $($all.Count) processes, $($browsers.Count) browsers"
    foreach ($b in $browsers) {
        $parent = Get-Process -Id $b.ParentProcessId -ErrorAction SilentlyContinue
        $by = if ($parent) { "$($parent.ProcessName)($($parent.Id))" } else { "ended($($b.ParentProcessId))" }
        "  browser $($b.ProcessId) started $($b.CreationDate.ToString('HH:mm:ss')) by $by"
    }
}
