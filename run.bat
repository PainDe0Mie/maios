@echo off
title MaiOS Build & Run
echo.
echo ============================================
echo   MaiOS - Build ^& Run
echo ============================================
echo.

:: Check args
if "%1"=="run" goto :run_only
if "%1"=="build" goto :build_only

:: Default: build + run
:build_and_run
call :do_build
if errorlevel 1 goto :error
call :do_run
goto :end

:build_only
call :do_build
goto :end

:run_only
call :do_run
goto :end

:: ── Build ────────────────────────────────────────────────────
:do_build
echo [*] Building MaiOS in WSL...
wsl bash -c "command -v nasm >/dev/null 2>&1 || sudo apt-get install -y nasm"
wsl bash -c "cd /mnt/c/Users/zulbe/Documents/Projet/MaiOs && make iso 2>&1"
if errorlevel 1 (
    echo.
    echo [!] BUILD FAILED
    exit /b 1
)
echo.
echo [OK] Build successful
exit /b 0

:: ── Run QEMU ─────────────────────────────────────────────────
:do_run
set ISO=build\MaiOS.iso
if not exist "%ISO%" (
    echo [!] ISO not found: %ISO%
    echo     Run 'run.bat' without args to build first.
    exit /b 1
)
echo [*] Launching QEMU...

set QEMU="C:\Program Files\qemu\qemu-system-x86_64.exe"
set ARGS=-cdrom %ISO% -boot d -cpu Broadwell -m 512M -smp 4 -no-reboot -no-shutdown -serial mon:stdio -s

:: Add disk image if present
if exist fat32.img set ARGS=%ARGS% -drive format=raw,file=fat32.img,if=ide

%QEMU% %ARGS%
exit /b 0

:error
echo.
echo Build failed. Fix errors above and retry.
pause
exit /b 1

:end
echo.
echo Done.
pause
