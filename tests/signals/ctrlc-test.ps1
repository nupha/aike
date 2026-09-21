# Windows real-signal delivery test for Aike.onSignal.
#
# On Windows a catchable signal (SIGINT) can only be raised as a console
# CTRL_C_EVENT, so this harness starts an aike child with
# tests/signals/fixture.mjs and injects the event into its console, then checks
# the child's output/exit.
#
# The portable JS side (tests/signals/signal.mjs) covers validation,
# registration, GC survival and — on Unix — cross-process delivery. This
# harness covers the Windows-only console delivery path.
#
# Usage (from the repo root, on Windows, in a real console):
#   powershell -ExecutionPolicy Bypass -File tests\signals\ctrlc-test.ps1
#   powershell -ExecutionPolicy Bypass -File tests\signals\ctrlc-test.ps1 -AikeExe path\to\aike.exe
#
# NOTE 1: FreeConsole() permanently detaches the current PowerShell from its
# console, so each CTRL_C_EVENT delivery must run in a *fresh* PowerShell
# process. The driver below re-invokes this same script once per scenario.
# NOTE 2: CTRL_C_EVENT is broadcast to every process on the console; run this
# from a dedicated console window, not from a shell you care about.

param(
    [string]$AikeExe = "",
    [string]$Scenario = ""
)

$ErrorActionPreference = "Stop"

Add-Type -Namespace AikeSig -Name K32 -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern bool FreeConsole();
[DllImport("kernel32.dll")] public static extern bool AttachConsole(uint pid);
[DllImport("kernel32.dll")] public static extern bool GenerateConsoleCtrlEvent(uint e, uint g);
[DllImport("kernel32.dll")] public static extern bool SetConsoleCtrlHandler(IntPtr h, bool add);
'@

# CTRL_C_EVENT is broadcast to every process sharing the console, so this
# script (driver and worker) must ignore Ctrl-C — only the aike child under
# test should react.
$null = [AikeSig.K32]::SetConsoleCtrlHandler([IntPtr]::Zero, $true)

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$fixture = Join-Path $PSScriptRoot "fixture.mjs"

if ($AikeExe -eq "") {
    $AikeExe = (Resolve-Path (Join-Path $repoRoot "target\debug\aike.exe")).Path
}
if (-not (Test-Path $AikeExe)) { Write-Host "aike not found: $AikeExe"; exit 1 }
if (-not (Test-Path $fixture)) { Write-Host "fixture not found: $fixture"; exit 1 }

# ── worker: one scenario, one CTRL_C_EVENT, machine-readable result ──────────
if ($Scenario -ne "") {
    # A process that ignores Ctrl-C passes the ignore flag on to children
    # created without CREATE_NEW_PROCESS_GROUP, so the aike child would never
    # see the event. Restore normal handling *before* spawning it, and ignore
    # again only when injecting the event.
    $null = [AikeSig.K32]::SetConsoleCtrlHandler([IntPtr]::Zero, $false)

    $tmp = Join-Path $env:TEMP ("aike-sig-" + [guid]::NewGuid().ToString("N") + ".txt")
    New-Item -ItemType File -Path $tmp | Out-Null
    $proc = Start-Process -FilePath $AikeExe -ArgumentList "-m", $fixture, $Scenario `
        -PassThru -NoNewWindow -RedirectStandardOutput $tmp

    # Wait for the fixture to finish registering handlers.
    $deadline = (Get-Date).AddSeconds(30)
    while (-not ((Get-Content $tmp -Raw -ErrorAction SilentlyContinue) -match "READY")) {
        if ($proc.HasExited) { break }
        if ((Get-Date) -gt $deadline) { break }
        Start-Sleep -Milliseconds 100
    }

    $exitedBySignal = $false
    if (-not $proc.HasExited) {
        $null = [AikeSig.K32]::FreeConsole()
        $null = [AikeSig.K32]::AttachConsole([uint32]$proc.Id)
        $null = [AikeSig.K32]::SetConsoleCtrlHandler([IntPtr]::Zero, $true)
        $null = [AikeSig.K32]::GenerateConsoleCtrlEvent(0, 0)
        for ($i = 0; $i -lt 50; $i++) {
            Start-Sleep -Milliseconds 100
            if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) { $exitedBySignal = $true; break }
        }
        $null = [AikeSig.K32]::FreeConsole()
        if (-not $exitedBySignal) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    }

    $output = ""
    if (Test-Path $tmp) { $output = Get-Content $tmp -Raw -ErrorAction SilentlyContinue }
    Remove-Item $tmp -ErrorAction SilentlyContinue

    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([string]$output))
    Write-Output "__EXITED__$exitedBySignal"
    Write-Output "__OUTPUT__$b64"
    exit 0
}

# ── driver: fresh PowerShell per scenario ────────────────────────────────────
$cases = @(
    @{ Name = "control";    Exited = $true;  Has = @();                HasNot = @("HANDLER-") }
    @{ Name = "simple";     Exited = $true;  Has = @("HANDLER-fired"); HasNot = @() }
    @{ Name = "gc";         Exited = $true;  Has = @("HANDLER-fired"); HasNot = @() }
    @{ Name = "overwrite";  Exited = $true;  Has = @("HANDLER-second"); HasNot = @("HANDLER-first") }
    @{ Name = "unregister"; Exited = $true;  Has = @();                HasNot = @("HANDLER-") }
    @{ Name = "reregister"; Exited = $true;  Has = @("HANDLER-second"); HasNot = @("HANDLER-first") }
)

$failed = $false
foreach ($c in $cases) {
    $raw = & powershell -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath `
        -AikeExe $AikeExe -Scenario $c.Name
    $text = ($raw -join "`n")
    $exited = $text -match "__EXITED__True"
    $b64 = ""
    if ($text -match "__OUTPUT__([A-Za-z0-9+/=]*)") { $b64 = $Matches[1] }
    $output = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($b64))

    $ok = $true
    $why = ""
    if ($c.Exited -and -not $exited) { $ok = $false; $why = "process did not exit on signal" }
    foreach ($needle in $c.Has) {
        if ($output -notmatch [regex]::Escape($needle)) { $ok = $false; $why = "missing '$needle'" }
    }
    foreach ($needle in $c.HasNot) {
        if ($output -match [regex]::Escape($needle)) { $ok = $false; $why = "unexpected '$needle'" }
    }

    if ($ok) {
        Write-Host ("  [ok]   {0}" -f $c.Name)
    } else {
        $failed = $true
        Write-Host ("  [FAIL] {0} - {1}" -f $c.Name, $why)
        Write-Host ("         output: {0}" -f ($output -replace "\s+", " ").Trim())
    }
}

Write-Host ""
if ($failed) { Write-Host "FAIL"; exit 1 } else { Write-Host "all pass"; exit 0 }
