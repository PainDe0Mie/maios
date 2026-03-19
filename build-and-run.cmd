@echo off
REM Launch the PowerShell build script with proper elevation and wait

echo Building MaiOS...
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-and-run.ps1"

if errorlevel 1 (
    echo.
    echo BUILD FAILED - Press any key to exit
    pause >nul
    exit /b 1
)

echo.
echo Press any key to exit
pause >nul
