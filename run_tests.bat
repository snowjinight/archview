@echo off
rem Archview smoke pipeline: (1) setup -> (2) cargo test -> (3) VarTidy CLI smoke.
rem Stage 1 checks deps and (re)generates fixtures under smoke\ (gitignored).
rem Missing required dep (ffmpeg / libmpv-2.dll) aborts in stage 1, never in tests.
rem Missing fixture at test time panics with guidance -- never a silent skip.
setlocal
cd /d "%~dp0"

echo [1/3] smoke setup: check deps, generate fixtures
cargo run --bin smoke_setup
if errorlevel 1 goto :fail

echo.
echo [2/3] cargo test (missing fixture panics, never silent skip)
cargo test -- --nocapture
if errorlevel 1 goto :fail

echo.
echo [3/3] VarTidy CLI smoke: dry-run, real tidy, idempotent rerun
cargo build
if errorlevel 1 goto :fail
set PW1=smoke-pw-123
set PW2=sm0ke!@#2026
set PACKS=smoke\vartidy\pack1.zip smoke\vartidy\pack2.zip smoke\vartidy\pack3_enc.zip smoke\vartidy\pack4.7z smoke\vartidy\pack5_enc.7z smoke\vartidy\pack6_sp.zip
target\debug\archview.exe tidy %PACKS% -o smoke\vartidy\var_repo -n -p %PW1% -p "%PW2%" -q
if errorlevel 1 goto :fail
target\debug\archview.exe tidy %PACKS% -o smoke\vartidy\var_repo -p %PW1% -p "%PW2%" -q
if errorlevel 1 goto :fail
target\debug\archview.exe tidy %PACKS% -o smoke\vartidy\var_repo -p %PW1% -p "%PW2%" -q
if errorlevel 1 goto :fail

echo.
echo === ALL SMOKE PASS ===
exit /b 0

:fail
echo.
echo === SMOKE FAIL ===
exit /b 1
