@echo off
setlocal
cd /d "%~dp0"
echo Open http://127.0.0.1:9999 in your browser.
echo Keep this window open while using pace-forge. Press Ctrl+C to stop.
"%~dp0pace-forge.exe" web 127.0.0.1:9999
set "pace_forge_exit=%errorlevel%"
if not "%pace_forge_exit%"=="0" (
    echo.
    echo pace-forge exited with error %pace_forge_exit%.
    pause
)
exit /b %pace_forge_exit%
