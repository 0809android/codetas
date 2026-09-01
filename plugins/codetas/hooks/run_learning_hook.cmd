@echo off
REM Thin Windows wrapper: locate Python 3.10+ and run learning_hook.py.
setlocal EnableExtensions
set "ROOT=%~dp0.."
set "HOOK=%ROOT%\hooks\learning_hook.py"
set "PROBE=import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 1)"

where python >nul 2>nul
if errorlevel 1 goto try_python3
python -c "%PROBE%" >nul 2>nul
if errorlevel 1 goto try_python3
python "%HOOK%" %*
exit /b %ERRORLEVEL%

:try_python3
where python3 >nul 2>nul
if errorlevel 1 goto try_py
python3 -c "%PROBE%" >nul 2>nul
if errorlevel 1 goto try_py
python3 "%HOOK%" %*
exit /b %ERRORLEVEL%

:try_py
where py >nul 2>nul
if errorlevel 1 goto missing
py -3 -c "%PROBE%" >nul 2>nul
if errorlevel 1 goto missing
py -3 "%HOOK%" %*
exit /b %ERRORLEVEL%

:missing
echo CODETAS: Python 3.10+ not found 1>&2
exit /b 127
