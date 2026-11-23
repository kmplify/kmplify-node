# Start smoke test (Windows): start `kmplify-node --ui gui` against a gateway
# that does not exist and check that it reaches the dashboard and keeps
# running.
#
#   powershell -File scripts\smoke-start.ps1 -Bin target\release\kmplify-node.exe -Variant plain
#
# Variant:
#   plain   the machine as it is
#   hang    a docker.exe and an nvidia-smi.exe that never answer, first on PATH
#   down    a docker.exe that answers like Docker Desktop when it is not
#           started (the npipe dockerDesktopLinux error) and exits 1
#
# The fakes are real .exe files: Windows starts a bare name only as .exe, so
# a docker.cmd would never be the docker the node runs. Built with Add-Type,
# which needs Windows PowerShell 5.1 (`shell: powershell` in the workflow).
param(
    [Parameter(Mandatory = $true)][string]$Bin,
    [Parameter(Mandatory = $true)][ValidateSet('plain', 'hang', 'down')][string]$Variant
)
$ErrorActionPreference = 'Stop'
$Bin = (Resolve-Path $Bin).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ("kmplify-smoke-" + [guid]::NewGuid())
$fake = Join-Path $work 'fake'
$node = Join-Path $work 'node'
New-Item -ItemType Directory -Force -Path $fake, $node | Out-Null

$hang = @'
public static class P { public static int Main(string[] a) { System.Threading.Thread.Sleep(600000); return 0; } }
'@
$down = @'
public static class P { public static int Main(string[] a) {
  System.Console.Error.WriteLine("failed to connect to the docker API at npipe:////./pipe/dockerDesktopLinux; check if the path is correct and if the daemon is running: open //./pipe/dockerDesktopLinux: The system cannot find the file specified.");
  return 1; } }
'@
switch ($Variant) {
    'hang' {
        Add-Type -TypeDefinition $hang -OutputAssembly (Join-Path $fake 'docker.exe') -OutputType ConsoleApplication
        Add-Type -TypeDefinition $hang -OutputAssembly (Join-Path $fake 'nvidia-smi.exe') -OutputType ConsoleApplication
    }
    'down' {
        Add-Type -TypeDefinition $down -OutputAssembly (Join-Path $fake 'docker.exe') -OutputType ConsoleApplication
    }
}

$out = Join-Path $work 'out.txt'
$err = Join-Path $work 'err.txt'
$env:PATH = "$fake;$env:PATH"
$env:KMPLIFY_NODE_UI_OPEN = '0'
$env:KMPLIFY_NODE_DIR = $node
$env:PROVIDER_GATEWAY_URL = 'http://127.0.0.1:9'

$started = Get-Date
$p = Start-Process -FilePath $Bin -ArgumentList 'run', '--ui', 'gui' -NoNewWindow -PassThru `
    -RedirectStandardOutput $out -RedirectStandardError $err

$seen = $null
for ($i = 0; $i -lt 90; $i++) {
    if ((Test-Path $out) -and (Select-String -Path $out -Pattern 'dashboard on this computer' -Quiet)) {
        $seen = ((Get-Date) - $started).TotalSeconds
        break
    }
    if ($p.HasExited) { break }
    Start-Sleep -Seconds 1
}
# Alive at 20 s after the start, and 5 s after the dashboard line.
$until = $started.AddSeconds(20)
if ($seen -ne $null -and $started.AddSeconds($seen + 5) -gt $until) { $until = $started.AddSeconds($seen + 5) }
while ((Get-Date) -lt $until) { Start-Sleep -Seconds 1 }
$alive = -not $p.HasExited
$exitCode = $null
if ($alive) {
    Stop-Process -Id $p.Id -Force
    $p.WaitForExit(30000) | Out-Null
} else {
    $p.WaitForExit()
    $exitCode = $p.ExitCode
}

$text = ''
foreach ($f in $out, $err) { if (Test-Path $f) { $text += (Get-Content -Raw $f) + "`n" } }
Write-Host "=== output ($Variant)"
Write-Host $text
Write-Host "=== startup.log ($Variant)"
$log = Join-Path $node 'startup.log'
if (Test-Path $log) { Get-Content $log | Write-Host } else { Write-Host '(no startup.log)' }
$crash = Join-Path $node 'startup-crash.log'
if (Test-Path $crash) { Write-Host '=== startup-crash.log'; Get-Content $crash | Write-Host }

$fail = $false
function Check($pattern, $what) {
    if ($text -match [regex]::Escape($pattern)) { Write-Host "ok: $what" }
    else { Write-Host "FAIL: $what"; $script:fail = $true }
}
Check 'starting (node dir' 'the first start line'
Check 'joining' 'joining'
Check 'dashboard on this computer' 'the dashboard line'
if ($Variant -eq 'hang') { Check 'Docker does not answer' 'the Docker line' }
if ($Variant -eq 'down') { Check 'Docker is not running' 'the Docker line' }
if ($alive) { Write-Host 'ok: alive at 20 s' }
else {
    # 0xC00000FD is a stack overflow, 0xC0000005 an access violation.
    Write-Host ("FAIL: the node had exited with code {0} (0x{1:X8})" -f $exitCode, $exitCode)
    $fail = $true
}
if ($seen -ne $null) { Write-Host ("dashboard after {0:N1} s" -f $seen) }
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
if ($fail) { exit 1 }
