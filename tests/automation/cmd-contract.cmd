@echo off
setlocal

if "%~1"=="" (
    echo Command Prompt automation contract failed: the Unclean executable path is required. Pass the release binary path and retry.
    exit /b 2
)

set "binary=%~f1"
if not exist "%binary%" (
    echo Command Prompt automation contract failed: the Unclean executable was not found. Build the release binary and retry.
    exit /b 2
)

set "result=%TEMP%\unclean-automation-%RANDOM%-%RANDOM%.json"
set "missing=%~dp0unclean-contract-missing-preset.toml"

"%binary%" engines --format json > "%result%" 2>&1
if errorlevel 1 (
    echo Command Prompt automation contract failed: engine discovery returned a failure. Check the executable and retry.
    goto failure
)

findstr /R /C:"schema.*1" "%result%" > nul
if errorlevel 1 (
    echo Command Prompt automation contract failed: engine discovery omitted schema 1. Restore the stable contract or publish a new schema.
    goto failure
)

findstr /R /C:"ok.*true" "%result%" > nul
if errorlevel 1 (
    echo Command Prompt automation contract failed: engine discovery omitted its success state. Restore the schema 1 contract.
    goto failure
)

findstr /R /C:"engines.*\[" "%result%" > nul
if errorlevel 1 (
    echo Command Prompt automation contract failed: engine discovery omitted the engine list. Restore the schema 1 contract.
    goto failure
)

"%binary%" preset validate "%missing%" --format json > "%result%" 2>&1
set "failureCode=%ERRORLEVEL%"
if not "%failureCode%"=="4" (
    echo Command Prompt automation contract failed: a missing preset did not return exit code 4. Restore the documented exit code.
    goto failure
)

findstr /R /C:"code.*not_found" "%result%" > nul
if errorlevel 1 (
    echo Command Prompt automation contract failed: the missing-preset result omitted not_found. Restore the schema 1 error contract.
    goto failure
)

findstr /R /C:"exit_code.*4" "%result%" > nul
if errorlevel 1 (
    echo Command Prompt automation contract failed: the missing-preset envelope omitted exit code 4. Restore the schema 1 error contract.
    goto failure
)

del /q "%result%" > nul 2>&1
echo Command Prompt automation contract passed.
exit /b 0

:failure
del /q "%result%" > nul 2>&1
exit /b 1
