@echo off
REM rd virtual display driver - one-click installer (virtual-display-rs master, MIT).
REM Needs administrator rights ONCE. Double-click and confirm the UAC prompt,
REM or run this file as administrator. No reboot required.
REM ASCII only on purpose: a UTF-8 .bat/.cmd with non-ASCII text fails silently
REM under a GBK console (phase-0 pitfall).
setlocal EnableExtensions
set "PKG=%~dp0"
set "NEFCON=%PKG%nefconw.exe"
set "INF=%PKG%VirtualDisplayDriver.inf"
set "CER=%PKG%DriverCertificateMaster.cer"
set "HWID=Root\VirtualDisplayDriver"
set "CLASSGUID=4D36E968-E325-11CE-BFC1-08002BE10318"

REM ---- self-elevate ---------------------------------------------------------
net session >nul 2>&1
if not errorlevel 1 goto :elevated
echo Requesting administrator rights (one UAC prompt) ...
powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
if errorlevel 1 (
  echo [ERROR] Elevation was cancelled. Right-click this file and choose
  echo         "Run as administrator".
  pause
  exit /b 1
)
exit /b 0

:elevated
set "LOG=%ProgramData%\rd\install-driver.log"
if not exist "%ProgramData%\rd" mkdir "%ProgramData%\rd" >nul 2>&1
echo. >> "%LOG%"
echo [%date% %time%] rd virtual display driver install, package: %PKG% >> "%LOG%"

if not exist "%INF" goto :missing
if not exist "%PKG%VirtualDisplayDriver.dll" goto :missing
if not exist "%CER" goto :missing
if not exist "%NEFCON" goto :missing

REM An existing "Root\VirtualDisplayDriver" node may belong to another app
REM (official virtual-display-rs v0.3.1 / SnugWin). rd upgrades that node to
REM its own build; rd's uninstaller only removes what its marker proves rd
REM installed. Just tell the user what happens.
"%NEFCON%" --find-hwid --hardware-id "Root\VirtualDisplayDriver" > "%TEMP%\rd-vdisplay-devices.txt" 2>&1
findstr /i /c:"VirtualDisplayDriver" "%TEMP%\rd-vdisplay-devices.txt" >nul 2>&1
if not errorlevel 1 (
  echo [notice] An existing Virtual Display Driver node was found. It will be
  echo          upgraded to rd's build. This keeps third-party virtual screens
  echo          usable; uninstalling rd will not delete a driver rd did not
  echo          install.
  echo [notice] existing node detected, upgrading >> "%LOG%"
)
del "%TEMP%\rd-vdisplay-devices.txt" >nul 2>&1

REM 1) trust the (self-signed, development) driver certificate - public part only
certutil -addstore -f root "%CER%" >> "%LOG%" 2>&1 || goto :fail
certutil -addstore -f TrustedPublisher "%CER%" >> "%LOG%" 2>&1 || goto :fail

REM 2) (re)create the root-enumerated device node
REM    --class-name Display is mandatory: without it nefcon fails with
REM    "Device Class Name missing" (phase-0 pitfall 2).
"%NEFCON%" --remove-device-node --hardware-id "%HWID%" --class-guid "%CLASSGUID%" >> "%LOG%" 2>&1
"%NEFCON%" --create-device-node --class-name Display --class-guid "%CLASSGUID%" --hardware-id "%HWID%" >> "%LOG%" 2>&1 || goto :fail

REM 3) install the driver package itself (Authenticode via the trusted cert)
"%NEFCON%" --install-driver --inf-path "%INF%" >> "%LOG%" 2>&1 || goto :fail

REM 4) ownership marker: rd's uninstaller removes the driver ONLY when this
REM    marker exists; a driver the user installed themselves stays untouched.
powershell -NoProfile -ExecutionPolicy Bypass -Command "$p='%ProgramData%\rd\driver-ownership.json'; $m=[ordered]@{owner='rd';hardware_id='Root\VirtualDisplayDriver';inf='VirtualDisplayDriver.inf';provider='virtual-display-rs (MIT)';driver_version='0.4.0.0';installed_at=(Get-Date).ToString('o')}; $m | ConvertTo-Json | Set-Content -Encoding UTF8 $p" >> "%LOG%" 2>&1
if not exist "%ProgramData%\rd\driver-ownership.json" (
  echo [WARN] ownership marker was not written; rd will refuse to uninstall
  echo        this driver automatically. See "%LOG%".
  echo [WARN] ownership marker write failed >> "%LOG%"
)

echo [rd] virtual display driver installed. No reboot required.
echo [rd] log: "%LOG%"
pause
exit /b 0

:missing
echo [ERROR] driver package is incomplete - re-extract the rd portable zip.
echo [ERROR] package incomplete >> "%LOG%"
pause
exit /b 1

:fail
echo [ERROR] driver installation failed. See "%LOG%" for details.
echo [ERROR] install failed >> "%LOG%"
pause
exit /b 1
