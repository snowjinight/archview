@echo off
rem ============================================================
rem  Archview uninstall (per-user): removes the file associations
rem  and SendTo shortcuts created by install.bat. ASCII-only.
rem ============================================================
setlocal

set "SENDTO_DIR=%APPDATA%\Microsoft\Windows\SendTo"

rem ---- ProgIDs ----
for %%p in (Archview.Archive Archview.Iso Archview.Video Archview.Image) do (
    reg delete "HKCU\Software\Classes\%%p" /f >nul 2>&1
)

rem ---- OpenWithProgids values (value-level delete only) ----
for %%e in (zip 7z rar) do reg delete "HKCU\Software\Classes\.%%e\OpenWithProgids" /v Archview.Archive /f >nul 2>&1
for %%e in (iso) do reg delete "HKCU\Software\Classes\.%%e\OpenWithProgids" /v Archview.Iso /f >nul 2>&1
for %%e in (mp4 mkv avi mov flv ts webm wmv m4v nv12 yuv400 raw) do reg delete "HKCU\Software\Classes\.%%e\OpenWithProgids" /v Archview.Video /f >nul 2>&1
for %%e in (jpg jpeg png gif webp bmp tif tiff arw nef nrw srf sr2 arq) do reg delete "HKCU\Software\Classes\.%%e\OpenWithProgids" /v Archview.Image /f >nul 2>&1

rem ---- SendTo shortcuts ----
del "%SENDTO_DIR%\Archview player.lnk" >nul 2>&1
del "%SENDTO_DIR%\Archview tidy.lnk" >nul 2>&1
del "%SENDTO_DIR%\Archview encode.lnk" >nul 2>&1

echo [OK] Removed per-user associations and SendTo shortcuts.
echo If you set Archview as the DEFAULT app in Explorer, clear it under
echo Windows Settings - Apps - Default apps (UserChoice is user-owned).
echo.
pause
