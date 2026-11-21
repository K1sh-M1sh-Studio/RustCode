@echo off
echo =======================================
echo   Building RustCode
echo =======================================
set /p mode="Enter mode (release/debug) [release]: "

if /I "%mode%"=="debug" (
    echo Running debug build...
    cargo build
) else (
    echo Running release build...
    cargo build --release
)

echo.
echo Build finished successfully!
pause