@echo off
cd /d "%~dp0"
if not exist logs mkdir logs

REM Refresh from latest Rust release build, then run from project root so
REM `current_exe().parent()` resolves to the project root and finds the
REM existing accounts/, accounts_meta.json, cb_consumer.json, etc.
copy /Y rust\target\release\ClaudeMonitor.exe ClaudeMonitor.exe >nul

REM Rust binary is windows_subsystem="windows" — stderr only reaches a file
REM when redirected from this cmd.exe parent.
ClaudeMonitor.exe 2>> logs\rust.stderr.log
