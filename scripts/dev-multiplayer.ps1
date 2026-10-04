# Dev multiplayer launcher: one Go session server (`go run`) plus N game clients
# (`cargo run`). No prior build and no `dist/` staging are required; the first
# invocation compiles the debug profile and later ones reuse it.
#
# Every process opens in its OWN titled console, which shows that process's logs
# live; the launcher terminal only prints status. Output is also written to
# logs/ so the launcher can detect readiness. With -Tui the server console runs
# the interactive dashboard instead and mirrors its logs to logs/ itself.
#
# Every process is placed in a Windows Job Object with KILL_ON_JOB_CLOSE, so
# closing this terminal (or Ctrl+C, or a crash) tears down the server and all
# clients automatically.
#
# Usage (from skate-3-rust-engine/):
#   powershell -File scripts/dev-multiplayer.ps1                    # server + 2 clients
#   powershell -File scripts/dev-multiplayer.ps1 -Instances 3
#   powershell -File scripts/dev-multiplayer.ps1 -TestWorld
#   powershell -File scripts/dev-multiplayer.ps1 -Map maps/University.skate
#   powershell -File scripts/dev-multiplayer.ps1 -NoServer          # server already running
#   powershell -File scripts/dev-multiplayer.ps1 -UseBuiltServer    # prefer server-go/bin/skated.exe
#   powershell -File scripts/dev-multiplayer.ps1 -Tui               # server console shows the TUI dashboard
#   powershell -File scripts/dev-multiplayer.ps1 -DryRun            # print commands only
param(
    [ValidateRange(1, 10)][int]$Instances = 2,
    [string]$Server = '127.0.0.1:31030',
    [switch]$NoServer,
    [string]$Assets,
    [string]$Map,
    [switch]$TestWorld,
    [switch]$UseBuiltServer,
    [switch]$Tui,
    [int]$StartupTimeoutMinutes = 15,
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'

$workspace = Split-Path -Parent $PSScriptRoot
$repo = Split-Path -Parent $workspace
$serverGo = Join-Path $repo 'server-go'

if (-not (Test-Path -LiteralPath (Join-Path $serverGo 'go.mod'))) {
    throw "server-go not found at $serverGo (expected the skated server beside skate-3-rust-engine)."
}

# skate-proto's build.rs needs protoc, which is not always on PATH in a fresh
# shell (winget install).
if (-not $env:PROTOC) {
    $candidate = 'C:\Users\goust\AppData\Local\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin\protoc.exe'
    if (Test-Path $candidate) { $env:PROTOC = $candidate }
}

if (-not $Assets) {
    $Assets = if ($env:SKATE3_ASSETS) { $env:SKATE3_ASSETS } else { Join-Path $workspace 'assets' }
}
if (-not (Test-Path -LiteralPath $Assets)) {
    throw "Assets not found: $Assets. Pass -Assets or set SKATE3_ASSETS."
}
$Assets = (Resolve-Path -LiteralPath $Assets).Path
$env:SKATE3_ASSETS = $Assets

if ($Map -and $TestWorld) { throw 'Choose -Map or -TestWorld, not both.' }
if ($Map) {
    $Map = (Resolve-Path -LiteralPath $Map).Path
    if ([IO.Path]::GetExtension($Map) -ne '.skate') { throw 'Expected an existing .skate map.' }
}

$cargo = (Get-Command cargo -ErrorAction Stop).Source
$go = (Get-Command go -ErrorAction Stop).Source
$powershellExe = (Get-Command powershell.exe -ErrorAction Stop).Source

# Builds a Windows-quoted argument for `cmd`/CreateProcess.
function Quote-Argument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
}
# Builds a single-quoted PowerShell literal.
function PS-Literal([string]$Value) {
    "'" + ($Value -replace "'", "''") + "'"
}

# Reads a log file while another process is writing it.
function Get-LogText([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        try {
            $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
            try { return [System.IO.StreamReader]::new($stream).ReadToEnd() }
            finally { $stream.Dispose() }
        } catch { Start-Sleep -Milliseconds 100 }
    }
    return ''
}

# --- Kill-on-close job so closing this terminal tears everything down ---------
$script:jobHandle = [IntPtr]::Zero
try {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class LauncherJob {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObject(IntPtr attr, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int infoClass, IntPtr info, uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr handle);

    [StructLayout(LayoutKind.Sequential)]
    struct BasicLimit {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct IoCounters {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ExtendedLimit {
        public BasicLimit BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    public static IntPtr CreateKillOnClose() {
        IntPtr job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) return IntPtr.Zero;
        var info = new ExtendedLimit();
        info.BasicLimitInformation.LimitFlags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        int size = Marshal.SizeOf(info);
        IntPtr buffer = Marshal.AllocHGlobal(size);
        try {
            Marshal.StructureToPtr(info, buffer, false);
            if (!SetInformationJobObject(job, 9 /* ExtendedLimitInformation */, buffer, (uint)size)) {
                CloseHandle(job);
                return IntPtr.Zero;
            }
        } finally {
            Marshal.FreeHGlobal(buffer);
        }
        return job;
    }

    public static bool Assign(IntPtr job, int pid) {
        if (job == IntPtr.Zero) return false;
        // PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION
        IntPtr proc = OpenProcess(0x0100 | 0x0001 | 0x0400, false, pid);
        if (proc == IntPtr.Zero) return false;
        try { return AssignProcessToJobObject(job, proc); }
        finally { CloseHandle(proc); }
    }
}
"@ -ErrorAction Stop
    $script:jobHandle = [LauncherJob]::CreateKillOnClose()
    if ($script:jobHandle -eq [IntPtr]::Zero) {
        Write-Warning 'Kill-on-close job could not be created; closing this terminal may leave games running.'
    }
} catch {
    Write-Warning "Kill-on-close job unavailable ($($_.Exception.Message)); closing this terminal may leave games running."
}

function Add-ToJob([int]$ProcessId) {
    if ($script:jobHandle -eq [IntPtr]::Zero) { return }
    if (-not [LauncherJob]::Assign($script:jobHandle, $ProcessId)) {
        Write-Warning "Could not attach process $ProcessId to the kill-on-close job."
    }
}

# Starts a process in its own titled console that shows its live output and also
# writes it to $LogFile. Returns the console host process (tracked for liveness
# and job membership; its children die with it).
function Start-TitledConsole {
    param(
        [Parameter(Mandatory)][string]$Title,
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$LogFile,
        [Parameter(Mandatory)][string]$WorkDir
    )
    $cmdLine = (Quote-Argument $Exe) + ' ' + (($Arguments | ForEach-Object { Quote-Argument $_ }) -join ' ')
    # `cmd /c` merges the child's stderr into stdout so the log stays clean; the
    # console shows it and Tee mirrors it to disk for readiness detection.
    $script = "`$Host.UI.RawUI.WindowTitle = $(PS-Literal $Title)`r`n" +
        "cmd.exe /c $(PS-Literal ($cmdLine + ' 2>&1')) | Tee-Object -FilePath $(PS-Literal $LogFile)"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
    return Start-Process -FilePath $powershellExe -WorkingDirectory $WorkDir `
        -ArgumentList "-NoProfile -ExecutionPolicy Bypass -EncodedCommand $encoded" -PassThru
}

# Like Start-TitledConsole, but runs the command directly in the console instead
# of piping its output through Tee. The TUI needs a real terminal (a pipe is not
# a TTY), so it cannot run behind the Tee pipeline; the server mirrors its logs
# to a file with -logfile for readiness detection instead.
function Start-TitledConsoleDirect {
    param(
        [Parameter(Mandatory)][string]$Title,
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$WorkDir
    )
    $cmdLine = (Quote-Argument $Exe) + ' ' + (($Arguments | ForEach-Object { Quote-Argument $_ }) -join ' ')
    $script = "`$Host.UI.RawUI.WindowTitle = $(PS-Literal $Title)`r`n" +
        "cmd.exe /c $(PS-Literal $cmdLine)"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
    return Start-Process -FilePath $powershellExe -WorkingDirectory $WorkDir `
        -ArgumentList "-NoProfile -ExecutionPolicy Bypass -EncodedCommand $encoded" -PassThru
}

$run = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$logs = Join-Path $workspace "logs/multiplayer-dev-$run"
Write-Host 'Dev multiplayer: server via `go run`, clients via `cargo run` (first run compiles).' -ForegroundColor Cyan
Write-Host "Clients: $Instances | Server: $Server | Assets: $Assets"
Write-Host "Each process opens its own titled console; full logs: $logs"
if (-not $DryRun) { New-Item -ItemType Directory -Path $logs -Force | Out-Null }

$serverProcess = $null
$clients = @()
try {
    if (-not $NoServer) {
        $built = Join-Path $serverGo 'bin\skated.exe'
        if ($UseBuiltServer -and (Test-Path -LiteralPath $built)) {
            $serverExe = $built
            $serverArgs = @('-bind', $Server, '-timeout', '30s')
        } else {
            $serverExe = $go
            $serverArgs = @('run', './cmd/skated', '-bind', $Server, '-timeout', '30s')
        }
        $serverLog = Join-Path $logs 'server.log'
        if ($Tui) {
            # The dashboard owns the console, so the server cannot have its
            # stdout piped through Tee. It mirrors logs to a file instead, which
            # keeps the readiness check below working.
            $serverArgs += @('-tui', '-logfile', $serverLog)
        }
        if ($DryRun) {
            Write-Host ("SERVER  [Server]: {0} {1}" -f $serverExe, ($serverArgs -join ' '))
        } else {
            if ($Tui) {
                $serverProcess = Start-TitledConsoleDirect -Title 'Server' -Exe $serverExe -Arguments $serverArgs -WorkDir $serverGo
            } else {
                $serverProcess = Start-TitledConsole -Title 'Server' -Exe $serverExe -Arguments $serverArgs -LogFile $serverLog -WorkDir $serverGo
            }
            Add-ToJob $serverProcess.Id
            $deadline = [DateTime]::UtcNow.AddSeconds(30)
            while (-not (Get-LogText $serverLog).Contains('listening')) {
                if ($serverProcess.HasExited) { throw "Server exited. If one is already running, use -NoServer. Log: $serverLog" }
                if ([DateTime]::UtcNow -ge $deadline) { throw "Server did not report 'listening' within 30 s. Log: $serverLog" }
                Start-Sleep -Milliseconds 300
            }
            Write-Host "Server running (PID $($serverProcess.Id))." -ForegroundColor Cyan
            if ($Tui) { Write-Host 'Server console shows the TUI dashboard; press q there (or close the window) to stop it.' -ForegroundColor Cyan }
        }
    }

    for ($index = 1; $index -le $Instances; $index++) {
        # Bound offsets to the engine's existing 20-metre CLI limit.
        $offset = (($index - 1) % 10) * 2
        $clientArgs = @('run', '-p', 'skate-game', '--bin', 'skate3rust', '--',
            '--assets', $Assets, '--net-server', $Server, '--gpu-backend', 'vulkan',
            '--multi-instance', '--player-title', "Player $index",
            '--spawn-offset', "$offset")
        if ($Map) { $clientArgs += @('--map', $Map) }
        if ($TestWorld) { $clientArgs += '--test-world' }

        if ($DryRun) {
            Write-Host ("CLIENT {0} [Player {0}]: cargo {1}" -f $index, ($clientArgs -join ' '))
            continue
        }

        $clientLog = Join-Path $logs "client-$index.log"
        $client = Start-TitledConsole -Title "Player $index" -Exe $cargo -Arguments $clientArgs -LogFile $clientLog -WorkDir $workspace
        $clients += $client
        Add-ToJob $client.Id
        Write-Host "Client $index 'Player $index' starting (PID $($client.Id)); compiling and loading." -ForegroundColor Yellow

        # Loading large maps and compiling pipelines can exceed a fixed delay.
        $deadline = [DateTime]::UtcNow.AddMinutes($StartupTimeoutMinutes)
        while ($true) {
            $tail = Get-LogText $clientLog
            if ($client.HasExited) { throw "Client $index exited during startup. Log: $clientLog" }
            if ($tail -match 'REPORT_PANIC|REPORT_NATIVE|Crash report saved:|memory allocation .* failed') {
                throw "Client $index failed to start. Log: $clientLog"
            }
            if ($tail -match 'REPORT_META state=.*physics_tick:[1-9][0-9]*') { break }
            if ([DateTime]::UtcNow -ge $deadline) {
                throw "Client $index startup timed out after $StartupTimeoutMinutes min. Log: $clientLog"
            }
            Start-Sleep -Milliseconds 500
        }
        Write-Host "Client $index 'Player $index' running." -ForegroundColor Green
        if ($index -lt $Instances) { Start-Sleep -Seconds 2 }
    }

    if (-not $DryRun) {
        Write-Host 'All clients launched. Their consoles show live logs; close the game windows (or this terminal) when done.' -ForegroundColor Green
        while (@($clients | Where-Object { -not $_.HasExited }).Count -gt 0) { Start-Sleep -Seconds 1 }
    }
} finally {
    # Only terminate processes created by this invocation. -NoServer must never
    # stop an existing server or someone else's clients.
    foreach ($client in $clients) {
        if (-not $client.HasExited) { & taskkill.exe /PID $client.Id /T /F 2>&1 | Out-Null }
    }
    if ($serverProcess -and -not $serverProcess.HasExited) {
        & taskkill.exe /PID $serverProcess.Id /T /F 2>&1 | Out-Null
    }
}
