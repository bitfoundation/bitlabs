@echo off
setlocal

rem Folder of this script, without the trailing backslash
set "DEVDECK_DIR=%~dp0"
if "%DEVDECK_DIR:~-1%"=="\" set "DEVDECK_DIR=%DEVDECK_DIR:~0,-1%"

echo Removing "%DEVDECK_DIR%" from the user PATH...
rem Reads the raw user PATH (keeps %%VAR%% references), drops every entry for this folder,
rem writes it back as REG_EXPAND_SZ and broadcasts the change to running programs.
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $d=$env:DEVDECK_DIR; $k=[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); $p=[string]$k.GetValue('Path','',[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); $parts=@($p -split ';' | Where-Object { $_ }); $keep=@($parts | Where-Object { $_.TrimEnd('\') -ine $d }); if ($keep.Count -eq $parts.Count) { Write-Host '  Not in PATH.' } else { $k.SetValue('Path', ($keep -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString); [Environment]::SetEnvironmentVariable('DEVDECK_PATH_REFRESH','1','User'); [Environment]::SetEnvironmentVariable('DEVDECK_PATH_REFRESH',$null,'User'); Write-Host '  Done.' }; $k.Close()"
if errorlevel 1 goto fail

echo Removing "Open with DevDeck" from the Explorer context menu...
call :delkey "HKCU\Software\Classes\Directory\shell\DevDeck" || goto fail
call :delkey "HKCU\Software\Classes\Directory\Background\shell\DevDeck" || goto fail
rem Entries of the app's old name, GitGui
call :delkey "HKCU\Software\Classes\Directory\shell\GitGui" || goto fail
call :delkey "HKCU\Software\Classes\Directory\Background\shell\GitGui" || goto fail
echo   Done.

echo Refreshing Explorer icons...
call :refreshicons

if exist "%APPDATA%\DevDeck" (
    echo.
    choice /c YN /n /m "Also delete DevDeck settings in %APPDATA%\DevDeck? [Y/N] "
    if not errorlevel 2 (
        rmdir /s /q "%APPDATA%\DevDeck" && echo   Settings deleted.
    )
)

echo.
echo DevDeck is uninstalled. You can now delete this folder.
echo.
pause
exit /b 0

:fail
echo.
echo Uninstall failed.
pause
exit /b 1

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
