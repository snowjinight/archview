@echo off
rem ============================================================
rem  Release packaging script for archview
rem  Usage:  package.bat
rem  Optional env overrides (no hard-coded paths):
rem    ARCHVIEW_MPV_DIR  - directory containing libmpv-2.dll (default: deps\mpv-dev)
rem    ARCHVIEW_DIST     - output directory (default: dist)
rem ============================================================
setlocal

if "%ARCHVIEW_MPV_DIR%"=="" set ARCHVIEW_MPV_DIR=deps\mpv-dev
if "%ARCHVIEW_DIST%"=="" set ARCHVIEW_DIST=dist

echo ==^> cargo build --release
cargo build --release
if errorlevel 1 goto :fail

if exist "%ARCHVIEW_DIST%" rmdir /s /q "%ARCHVIEW_DIST%"
mkdir "%ARCHVIEW_DIST%"

copy /y target\release\archview.exe "%ARCHVIEW_DIST%\" >nul
if errorlevel 1 goto :fail

if exist "%ARCHVIEW_MPV_DIR%\libmpv-2.dll" (
    copy /y "%ARCHVIEW_MPV_DIR%\libmpv-2.dll" "%ARCHVIEW_DIST%\" >nul
) else (
    echo WARNING: libmpv-2.dll not found in "%ARCHVIEW_MPV_DIR%" - package will not play video!
)

rem copy config: config.toml preferred; legacy config.json also packed (auto-migrates on first run)
if exist config.toml (
    copy /y config.toml "%ARCHVIEW_DIST%\" >nul
) else (
    if exist config.json copy /y config.json "%ARCHVIEW_DIST%\" >nul
)

if exist "%ARCHVIEW_DIST%.zip" del /q "%ARCHVIEW_DIST%.zip"
powershell -NoProfile -Command "Compress-Archive -Path '%ARCHVIEW_DIST%\*' -DestinationPath '%ARCHVIEW_DIST%.zip'"
if errorlevel 1 goto :fail

echo ==^> Done: %ARCHVIEW_DIST%.zip
dir "%ARCHVIEW_DIST%"
exit /b 0

:fail
echo PACKAGE FAILED
exit /b 1
