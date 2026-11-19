@echo off
setlocal

rem Folder of this script, without the trailing backslash
set "DEVDECK_DIR=%~dp0"
if "%DEVDECK_DIR:~-1%"=="\" set "DEVDECK_DIR=%DEVDECK_DIR:~0,-1%"
set "DEVDECK_EXE=%DEVDECK_DIR%\devdeck.exe"

if not exist "%DEVDECK_EXE%" (
    echo devdeck.exe was not found next to this script.
    goto fail
)

echo Adding "%DEVDECK_DIR%" to the user PATH...
rem Reads the raw user PATH (keeps %%VAR%% references), appends the folder once,
rem writes it back as REG_EXPAND_SZ and broadcasts the change to running programs.
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $d=$env:DEVDECK_DIR; $k=[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); $p=[string]$k.GetValue('Path','',[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); $parts=@($p -split ';' | Where-Object { $_ }); if ($parts | Where-Object { $_.TrimEnd('\') -ieq $d }) { Write-Host '  Already in PATH.' } else { $k.SetValue('Path', (($parts + $d) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString); [Environment]::SetEnvironmentVariable('DEVDECK_PATH_REFRESH','1','User'); [Environment]::SetEnvironmentVariable('DEVDECK_PATH_REFRESH',$null,'User'); Write-Host '  Done.' }; $k.Close()"
if errorlevel 1 goto fail

echo Adding "Open with DevDeck" to the Explorer context menu...
rem Same keys the app uses, so "devdeck --uninstall-context-menu" removes them.
rem Replaces the entry of the app's old name, GitGui, if it is there.
call :delkey "HKCU\Software\Classes\Directory\shell\GitGui" || goto fail
call :delkey "HKCU\Software\Classes\Directory\Background\shell\GitGui" || goto fail
call :addmenu "HKCU\Software\Classes\Directory\shell\DevDeck" || goto fail
call :addmenu "HKCU\Software\Classes\Directory\Background\shell\DevDeck" || goto fail
echo   Done.

echo Refreshing Explorer icons...
call :refreshicons

echo.
echo DevDeck is installed. Open a new terminal and run: devdeck .
echo Right-click a folder (or empty space inside one) in Explorer and pick "Open with DevDeck".
echo On Windows 11 the entry is under "Show more options" (Shift+F10).
echo.
pause
exit /b 0

:fail
echo.
echo Installation failed.
pause
exit /b 1

rem Kept outside any ( ) block: a ")" in the install path, such as "Program Files (x86)", would end the block early.
:addmenu
reg add "%~1" /ve /d "Open with DevDeck" /f >nul || exit /b 1
reg add "%~1" /v Icon /d "%DEVDECK_EXE%" /f >nul || exit /b 1
reg add "%~1\command" /ve /d "\"%DEVDECK_EXE%\" \"%%V\"" /f >nul || exit /b 1
exit /b 0

rem Deletes a registry key if it exists; a missing key is not an error.
:delkey
reg query "%~1" >nul 2>&1 || exit /b 0
reg delete "%~1" /f >nul || exit /b 1
exit /b 0

rem Explorer caches icons by file path, so an updated devdeck.exe in the same folder keeps
rem showing its old icon. ie4uinit rebuilds the icon cache (-show on Windows 10/11,
rem -ClearIconCache on older versions); failures are ignored.
:refreshicons
ie4uinit.exe -show >nul 2>&1 || ie4uinit.exe -ClearIconCache >nul 2>&1
exit /b 0
