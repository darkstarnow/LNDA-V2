@echo off
setlocal
pushd "%~dp0"
cargo run --quiet --release --locked --package leonida-tasks -- setup %*
set status=%errorlevel%
popd
if not "%status%"=="0" (
    echo.
    echo Setup stopped. See the message above.
    pause
    exit /b 1
)
