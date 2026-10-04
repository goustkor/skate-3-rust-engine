@echo off
REM ---------------------------------------------------------------------------
REM Dev multiplayer launcher: one Go session server (`go run`) + N clients
REM (`cargo run`). No prior build / dist staging needed.
REM
REM Usage:
REM   multiplayer-dev.bat                 server + two clients (server console runs the TUI)
REM   multiplayer-dev.bat --instances 3   server + three clients
REM   multiplayer-dev.bat --test-world    use the lightweight multiplayer test scene
REM   multiplayer-dev.bat --no-tui        plain server console instead of the TUI dashboard
REM   multiplayer-dev.bat --no-server     clients only (server already running)
REM   multiplayer-dev.bat --dry-run       show commands without starting anything
REM
REM For a specific map: powershell -File scripts/dev-multiplayer.ps1 -Map <path>
REM ---------------------------------------------------------------------------
setlocal
set "HERE=%~dp0"

set "INSTANCES=2"
set "NO_SERVER="
set "DRY_RUN="
set "TEST_WORLD="
set "TUI=-Tui"

:parse
if "%~1"=="" goto afterparse
if /I "%~1"=="--no-server" (
    set "NO_SERVER=-NoServer"
    shift
    goto parse
)
if /I "%~1"=="--dry-run" (
    set "DRY_RUN=-DryRun"
    shift
    goto parse
)
if /I "%~1"=="--tui" (
    set "TUI=-Tui"
    shift
    goto parse
)
if /I "%~1"=="--no-tui" (
    set "TUI="
    shift
    goto parse
)
if /I "%~1"=="--test-world" (
    set "TEST_WORLD=-TestWorld"
    shift
    goto parse
)
if /I "%~1"=="--instances" (
    if "%~2"=="" (
        echo --instances requires a number.
        exit /b 2
    )
    set "INSTANCES=%~2"
    shift
    shift
    goto parse
)
echo Unknown argument: %~1
echo Usage: multiplayer-dev.bat [--instances N] [--test-world] [--tui] [--no-tui] [--no-server] [--dry-run]
exit /b 2
:afterparse
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%HERE%dev-multiplayer.ps1" -Instances "%INSTANCES%" %TEST_WORLD% %NO_SERVER% %TUI% %DRY_RUN%
if errorlevel 1 (
    if not defined DRY_RUN pause
    exit /b 1
)
exit /b 0
