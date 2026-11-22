@echo off
rem ============================================================
rem  Archview install (per-user, no admin required):
rem    1. SendTo shortcuts  : player / tidy / encode
rem    2. File associations : register ProgIDs + OpenWithProgids
rem       (right-click - Open with list; NOT stealing defaults)
rem  Place this bat NEXT TO archview.exe (package.bat does this
rem  automatically), then double-click. Uninstall: uninstall.bat.
rem  Keep this file ASCII-only (cmd code-page safe).
rem ============================================================
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

set "SENDTO_DIR=%APPDATA%\Microsoft\Windows\SendTo"

rem ==== 1. SendTo shortcuts =============================================
powershell -NoProfile -Command ^
  "$w = New-Object -ComObject WScript.Shell;" ^
  "$wd = (Split-Path '%EXE_PATH%');" ^
  "$p = $w.CreateShortcut($env:APPDATA+'\Microsoft\Windows\SendTo\Archview player.lnk');" ^
  "$p.TargetPath = '%EXE_PATH%'; $p.WorkingDirectory = $wd; $p.Description = 'Open archive in Archview player'; $p.Save();" ^
  "$t = $w.CreateShortcut($env:APPDATA+'\Microsoft\Windows\SendTo\Archview tidy.lnk');" ^
  "$t.TargetPath = '%EXE_PATH%'; $t.Arguments = 'tidy'; $t.WorkingDirectory = $wd; $t.Description = 'VarTidy: incremental var merge into repo'; $t.Save();" ^
  "$e = $w.CreateShortcut($env:APPDATA+'\Microsoft\Windows\SendTo\Archview encode.lnk');" ^
  "$e.TargetPath = '%EXE_PATH%'; $e.Arguments = 'encode'; $e.WorkingDirectory = $wd; $e.Description = 'Encode raw frame dump sequence to video'; $e.Save()"

rem ==== 2. File associations ============================================
rem ProgID: who we are + how to open. OpenWithProgids: add us to each
rem extension's "Open with" list without touching the default app.
rem DefaultIcon "exe",N: N is a ZERO-BASED index over the exe's icon
rem resources in resource-ID order (build.rs ids 1..4):
rem   index 0 = app, 1 = image, 2 = video, 3 = archive
rem (verified by ExtractIconEx on the built exe; ",N" inside quotes is
rem legacy notation, "exe",N outside quotes is the canonical form)

reg add "HKCU\Software\Classes\Archview.Archive" /ve /d "Archview Archive" /f >nul
reg add "HKCU\Software\Classes\Archview.Archive\DefaultIcon" /ve /d "\"%EXE_PATH%\",3" /f >nul
reg add "HKCU\Software\Classes\Archview.Archive\shell\open\command" /ve /d "\"%EXE_PATH%\" \"%%1\"" /f >nul

reg add "HKCU\Software\Classes\Archview.Video" /ve /d "Archview Video" /f >nul
reg add "HKCU\Software\Classes\Archview.Video\DefaultIcon" /ve /d "\"%EXE_PATH%\",2" /f >nul
reg add "HKCU\Software\Classes\Archview.Video\shell\open\command" /ve /d "\"%EXE_PATH%\" \"%%1\"" /f >nul

reg add "HKCU\Software\Classes\Archview.Image" /ve /d "Archview Image" /f >nul
reg add "HKCU\Software\Classes\Archview.Image\DefaultIcon" /ve /d "\"%EXE_PATH%\",1" /f >nul
reg add "HKCU\Software\Classes\Archview.Image\shell\open\command" /ve /d "\"%EXE_PATH%\" \"%%1\"" /f >nul

rem Archives: zip / 7z / rar (VFS supported set)
for %%e in (zip 7z rar) do call :assocext ".%%e" "Archview.Archive"
rem Videos: standard containers + raw dump presets ([[raw.demux]] defaults:
rem nv12 = NV12 8bit, yuv400 = gray 8bit, raw = bayer_auto16le dump)
for %%e in (mp4 mkv avi mov flv ts webm wmv m4v nv12 yuv400 raw) do call :assocext ".%%e" "Archview.Video"
rem Images: standard formats + camera RAW (raw-files embedded JPEG preview)
for %%e in (jpg jpeg png gif webp bmp tif tiff arw nef nrw srf sr2 arq) do call :assocext ".%%e" "Archview.Image"

reg query "HKCU\Software\Classes\Archview.Archive\shell\open\command" >nul 2>&1
if errorlevel 1 goto :fail

rem Tell the shell that association icons changed so Explorer re-reads them
rem without a manual icon-cache rebuild / reboot.
powershell -NoProfile -Command ^
  "Add-Type -Namespace Win32 -Name Shell -MemberDefinition '[DllImport(\"shell32.dll\")] public static extern void SHChangeNotify(int wEventId, int uFlags, IntPtr dwItem1, IntPtr dwItem2);';" ^
  "[Win32.Shell]::SHChangeNotify(0x08000000, 0x0000, [IntPtr]::Zero, [IntPtr]::Zero)"

echo [OK] Install finished for: "%EXE_PATH%"
echo.
echo SendTo menu:
echo   Archview player  -^> "%EXE_PATH%"
echo   Archview tidy    -^> "%EXE_PATH%" tidy
echo   Archview encode  -^> "%EXE_PATH%" encode
echo.
echo Registered in "Open with" (right-click a file - Open with):
echo   Archview Archive : zip 7z rar
echo   Archview Video   : mp4 mkv avi mov flv ts webm wmv m4v + raw dump (nv12 yuv400 raw)
echo   Archview Image   : jpg jpeg png gif webp bmp tif tiff + camera RAW (arw nef nrw srf sr2 arq)
echo.
echo To make Archview the default app for a type, use:
echo   right-click the file - Open with - Choose another app - Always
echo Uninstall anytime with uninstall.bat.
echo.
goto :end

:assocext
rem %~1 = ".ext", %~2 = ProgID. reg add creates the key if missing and
rem only adds our value, so existing associations are untouched.
reg add "HKCU\Software\Classes\%~1\OpenWithProgids" /v %~2 /t REG_SZ /d "" /f >nul
goto :eof

:fail
echo [ERROR] Association registration failed.
goto :end

:end
pause
