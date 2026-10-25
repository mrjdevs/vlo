@echo off
setlocal enabledelayedexpansion
title VLO Hostnames Setup
color 0B

:: ============================================
:: VLO Development Hostnames Setup
:: ============================================

:: Check for administrator privileges
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo Requesting administrator privileges...
    powershell -Command "Start-Process '%~f0' -Verb RunAs"
    exit /b
)

echo ============================================
echo   VLO Development Hostnames Setup
echo ============================================
echo.

:: Define paths
set "HOSTS_FILE=C:\Windows\System32\drivers\etc\hosts"
set "BACKUP_FILE=C:\Windows\System32\drivers\etc\hosts.backup.%date:~10,4%%date:~4,2%%date:~7,2%"

:: Check if hosts file exists
if not exist "%HOSTS_FILE%" (
    echo [ERROR] Hosts file not found at: %HOSTS_FILE%
    echo Press any key to exit...
    pause >nul
    exit /b 1
)

:: Create backup
echo [1/5] Creating backup of hosts file...
copy "%HOSTS_FILE%" "%BACKUP_FILE%" >nul 2>&1
if %errorLevel% equ 0 (
    echo       Backup saved to: %BACKUP_FILE%
) else (
    echo       [WARNING] Could not create backup. Continuing anyway...
)
echo.

:: Check if entries already exist
set "ENTRIES_EXIST=0"
findstr /i "dev.local" "%HOSTS_FILE%" >nul 2>&1
if %errorLevel% equ 0 set "ENTRIES_EXIST=1"

if "%ENTRIES_EXIST%"=="1" (
    echo [2/5] Checking existing entries...
    echo       VLO hostnames already exist in hosts file.
    echo.
    set /p "OVERWRITE=      Overwrite existing entries? (Y/N): "
    if /i "!OVERWRITE!" neq "Y" (
        echo       Skipping modification.
        goto :flush_dns
    )
    echo       Removing existing entries...
    powershell -Command "(Get-Content '%HOSTS_FILE%') | Where-Object { $_ -notmatch 'dev\.local|prod\.local|staging\.local|VLO Development' } | Set-Content '%HOSTS_FILE%'"
    echo       Done.
    echo.
)

:: Add new entries
echo [3/5] Adding VLO hostnames to hosts file...
(
    echo.
    echo # ============================================
    echo # VLO Development Hostnames
    echo # Added on: %date% %time%
    echo # ============================================
    echo 127.0.0.1    dev.local
    echo 127.0.0.1    prod.local
    echo 127.0.0.1    staging.local
    echo # ============================================
) >> "%HOSTS_FILE%"

if %errorLevel% equ 0 (
    echo       Successfully added:
    echo         - dev.local    (use with: http://dev.local:3000)
    echo         - prod.local   (use with: http://prod.local:4000)
    echo         - staging.local (use with: http://staging.local:5000)
) else (
    echo [ERROR] Failed to write to hosts file.
    echo         Try running this script as Administrator.
    goto :error_exit
)
echo.

:: Flush DNS cache
:flush_dns
echo [4/5] Flushing DNS cache...
ipconfig /flushdns >nul 2>&1
echo       DNS cache flushed successfully.
echo.

:: Verify entries
echo [5/5] Verifying hostnames...
echo.
set "VERIFY_SUCCESS=1"

for %%h in (dev.local prod.local staging.local) do (
    ping -n 1 %%h >nul 2>&1
    if !errorLevel! equ 0 (
        echo       [OK] %%h resolves correctly
    ) else (
        echo       [FAIL] %%h does not resolve
        set "VERIFY_SUCCESS=0"
    )
)

echo.
echo ============================================
if "%VERIFY_SUCCESS%"=="1" (
    echo   Setup Complete!
    echo ============================================
    echo.
    echo   You can now access your VLO apps at:
    echo     http://dev.local:3000      (Development)
    echo     http://prod.local:4000     (Production)
    echo     http://staging.local:5000  (Staging)
    echo.
    echo   Each hostname has its own cookie scope,
    echo   so login sessions are isolated!
    echo.
) else (
    echo   Setup completed with warnings.
    echo   Some hostnames may not resolve correctly.
    echo   Try restarting your computer if issues persist.
    echo.
)

echo ============================================
echo   Hosts file location:
echo   %HOSTS_FILE%
echo ============================================
echo.

:: Optional: Open hosts file to verify
set /p "OPEN_HOSTS=Open hosts file to verify? (Y/N): "
if /i "%OPEN_HOSTS%" equ "Y" (
    notepad "%HOSTS_FILE%"
)

echo.
echo Press any key to exit...
pause >nul
exit /b 0

:error_exit
echo.
echo ============================================
echo   Setup failed. Please check the errors above.
echo ============================================
echo.
echo Press any key to exit...
pause >nul
exit /b 1