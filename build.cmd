@echo off
rem Build helper. Requires Rust and MinGW-w64 on PATH.
rem   build.cmd build --release
rem   build.cmd test
setlocal
set "ROOT=%~dp0"
set "CARGO_TARGET_DIR=%ROOT%target-build"
if defined TDM_MINGW if exist "%TDM_MINGW%\bin" set "PATH=%TDM_MINGW%\bin;%PATH%"
where cargo >nul 2>nul || (
  echo Cargo was not found on PATH. Install Rust, then run this again.
  exit /b 1
)
cd /d "%ROOT%"
cargo %*
