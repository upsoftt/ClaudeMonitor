@echo off
cd /d "%~dp0"
if not exist logs mkdir logs

REM Refresh from latest Rust release build, then run from project root so
REM `current_exe().parent()` resolves to the project root and finds the
REM existing accounts/, accounts_meta.json, cb_consumer.json, etc.
copy /Y rust\target\release\ClaudeMonitor.exe ClaudeMonitor.exe >nul

REM Force Slint to use the Skia renderer — femtovg-gl on Windows ignores the
REM transparent Window background, leaving the OS surface opaque black around
REM our rounded inner Rectangle. Skia honours per-pixel alpha out of the box.
set SLINT_BACKEND=winit-skia

REM Rust binary is windows_subsystem="windows" — stderr only reaches a file
REM when redirected from this cmd.exe parent.
REM Use the fully-qualified path (%~dp0 = this script's dir): under TrayConsole's
REM restart environment cmd does not search the current directory for a bare
REM `ClaudeMonitor.exe`, so it failed with "is not recognized as a command".
start "" /wait "%~dp0ClaudeMonitor.exe" 2>> logs\rust.stderr.log
