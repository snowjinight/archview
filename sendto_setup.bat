@echo off
rem Create "Send to Archview" shortcut for VarTidy (tidy command).
rem Usage: edit EXE_PATH below if archview.exe lives elsewhere, then double-click this bat.
setlocal

rem ==== EDIT ME: full path to archview.exe ====
set "EXE_PATH=%~dp0target\debug\archview.exe"
rem ==========================================

if not exist "%EXE_PATH%" (
    echo [ERROR] archview.exe not found: "%EXE_PATH%"
    echo Edit EXE_PATH at the top of this bat, then run again.
    pause
    exit /b 1
)

set "LNK=%APPDATA%\Microsoft\Windows\SendTo\Archview tidy.lnk"
powershell -NoProfile -Command "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('%LNK%'); $s.TargetPath = '%EXE_PATH%'; $s.Arguments = 'tidy'; $s.WorkingDirectory = (Split-Path '%EXE_PATH%'); $s.Description = 'VarTidy: incremental var merge into repo'; $s.Save()"

if exist "%LNK%" (
    echo [OK] Shortcut created:
    echo   %LNK%
    echo   Target: "%EXE_PATH%" tidy
    echo.
    echo Right-click any archive - Send to - "Archview tidy"
) else (
    echo [ERROR] Shortcut creation failed.
)
pause
