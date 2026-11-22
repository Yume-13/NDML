@rem MIT License
@rem
@rem Copyright © 2026 Yume-13
@rem
@rem Permission is hereby granted, free of charge, to any person obtaining a copy
@rem of this software and associated documentation files (the "Software"), to deal
@rem in the Software without restriction, including without limitation the rights
@rem to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
@rem copies of the Software, and to permit persons to whom the Software is
@rem furnished to do so, subject to the following conditions:
@rem
@rem The above copyright notice and this permission notice shall be included in all
@rem copies or substantial portions of the Software.
@rem
@rem THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
@rem IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
@rem FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
@rem AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
@rem LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
@rem OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
@rem SOFTWARE.

@echo off
setlocal EnableExtensions
rem ===================================================================
rem  NDML launcher. Double-click to rebuild (only what changed) and open
rem  the manager. From a terminal:
rem    ndml.cmd            build + open the retro UI
rem    ndml.cmd play       build + deploy to the game + launch Nova Drift
rem    ndml.cmd status     build + run any other ndml command
rem ===================================================================
cd /d "%~dp0"

rem Double-clicked from Explorer inside Windows Terminal: reopen at 120x36.
if /i not "%~1"=="--sized" (
  if defined WT_SESSION (
    echo %cmdcmdline% | find /i "%~nx0" >nul && (
      where wt >nul 2>&1 && (
        start "" wt --size 120,36 -d "%~dp0." cmd /c ndml.cmd --sized %*
        exit /b 0
      )
    )
  )
)
if /i "%~1"=="--sized" shift /1

title NDML :: Nova Drift Mod Loader
mode con: cols=120 lines=36 >nul 2>&1

where cargo >nul 2>&1 || (
  echo cargo not found on PATH. Install Rust from https://rustup.rs
  pause
  exit /b 1
)
echo Building NDML (release)...
cargo build --release --quiet
if errorlevel 1 (
  echo.
  echo Build failed. If the NDML manager is already open, close it and retry.
  pause
  exit /b 1
)

if /i "%~1"=="play" (
  "target\release\ndml.exe" install || (pause & exit /b 1)
  "target\release\ndml.exe" launch
  exit /b
)
"target\release\ndml.exe" %1 %2 %3 %4 %5 %6 %7 %8 %9
if errorlevel 1 pause
