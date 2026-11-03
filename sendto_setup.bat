@echo off
rem Create "Send to Archview" shortcuts (player + tidy + encode).
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
  "$t.TargetPath = '%EXE_PATH%'; $t.Arguments = 'tidy'; $t.WorkingDirectory = $wd; $t.Description = 'VarTidy: incremental var merge into repo'; $t.Save();" ^
  "$e = $w.CreateShortcut($env:APPDATA+'\Microsoft\Windows\SendTo\Archview encode.lnk');" ^
  "$e.TargetPath = '%EXE_PATH%'; $e.Arguments = 'encode'; $e.WorkingDirectory = $wd; $e.Description = 'Encode raw frame dump sequence to video'; $e.Save()"

set "SENDTO_DIR=%APPDATA%\Microsoft\Windows\SendTo"
if exist "%SENDTO_DIR%\Archview player.lnk" if exist "%SENDTO_DIR%\Archview tidy.lnk" if exist "%SENDTO_DIR%\Archview encode.lnk" (
    echo [OK] Shortcuts created in SendTo menu:
    echo   Archview player  -^> "%EXE_PATH%"
    echo   Archview tidy    -^> "%EXE_PATH%" tidy
    echo   Archview encode  -^> "%EXE_PATH%" encode
    echo.
    echo Right-click any archive - Send to - "Archview player" / "Archview tidy" / "Archview encode"
) else (
    echo [ERROR] Shortcut creation failed.
)
pause
