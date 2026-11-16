@echo off
rem Build (if needed) and start the TDM GUI. No console window stays open:
rem tdm.exe is a Windows GUI-subsystem program.
setlocal
set "ROOT=%~dp0"
set "BIN=%ROOT%target-build\x86_64-pc-windows-gnu\release\tdm.exe"
set "CLI=%ROOT%target-build\x86_64-pc-windows-gnu\release\tdm-cli.exe"

if not exist "%BIN%" (
  echo Building TDM, first run takes a few minutes...
  call "%ROOT%build.cmd" build --release || exit /b 1
)

rem keep easy-to-find copies in the project folder (ignore failures if locked)
copy /y "%BIN%" "%ROOT%tdm.exe" >nul 2>&1
copy /y "%CLI%" "%ROOT%tdm-cli.exe" >nul 2>&1

rem launch the fresh binary even when the root copy is locked by antivirus
start "" "%BIN%"
