@echo off
rem Create "Send to Archview" shortcuts (player + tidy).
rem Place this bat NEXT TO archview.exe (package.bat does this automatically),
rem then double-click. Exe path is auto-detected from the bat's own directory.
setlocal

rem ==== Optional override: uncomment and edit if exe lives elsewhere ====
rem set "EXE_PATH=C:\tools\archview\archview.exe"
rem =====================================================================

if not defined EXE_PATH set "EXE_PATH=%~dp0archview.exe"

if not exist "%EXE_PATH%" (
    echo [ERROR] archview.exe not found: "%EXE_PATH%"
    echo Put this bat in the same folder as archview.exe, or set EXE_PATH inside this bat.
    pause
    exit /b 1
)

powershell -NoProfile -Command ^
  "$w = New-Object -ComObject WScript.Shell;" ^
  "$wd = (Split-Path '%EXE_PATH%');" ^
  "$p = $w.CreateShortcut($env:APPDATA+'\Microsoft\Windows\SendTo\Archview player.lnk');" ^
  "$p.TargetPath = '%EXE_PATH%'; $p.WorkingDirectory = $wd; $p.Description = 'Open archive in Archview player'; $p.Save();" ^
  "$t = $w.CreateShortcut($env:APPDATA+'\Microsoft\Windows\SendTo\Archview tidy.lnk');" ^
  "$t.TargetPath = '%EXE_PATH%'; $t.Arguments = 'tidy'; $t.WorkingDirectory = $wd; $t.Description = 'VarTidy: incremental var merge into repo'; $t.Save()"

set "SENDTO_DIR=%APPDATA%\Microsoft\Windows\SendTo"
if exist "%SENDTO_DIR%\Archview player.lnk" if exist "%SENDTO_DIR%\Archview tidy.lnk" (
    echo [OK] Shortcuts created in SendTo menu:
    echo   Archview player  -^> "%EXE_PATH%"
    echo   Archview tidy    -^> "%EXE_PATH%" tidy
    echo.
    echo Right-click any archive - Send to - "Archview player" or "Archview tidy"
) else (
    echo [ERROR] Shortcut creation failed.
)
pause
