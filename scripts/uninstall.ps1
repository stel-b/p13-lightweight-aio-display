<#
.SYNOPSIS
    Removes the aio-daemon Windows service.

.DESCRIPTION
    Stops and deletes the service, closes aio-ui, removes its shortcuts and
    "$env:ProgramFiles\aio-ui".
    For installs made with install.ps1; installer installs are removed from
    Settings > Apps instead.
    Keeps "$env:ProgramData\aio-ui" (settings, cache, logs);
    delete that folder yourself if you want everything gone.

    Run from an Administrator PowerShell.
#>
#Requires -RunAsAdministrator
$ErrorActionPreference = "Stop"

$ServiceName = "aio-daemon"
$InstallDir  = Join-Path $env:ProgramFiles "aio-ui"

$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($service) {
    if ($service.Status -ne "Stopped") {
        Write-Host "Stopping $ServiceName..."
        Stop-Service -Name $ServiceName
    }
    # Remove-Service needs PowerShell 6+; sc.exe works everywhere.
    sc.exe delete $ServiceName | Out-Null
    Write-Host "Deleted service $ServiceName"
} else {
    Write-Host "Service $ServiceName is not installed"
}

Get-Process -Name aio-ui, aio-loop -ErrorAction SilentlyContinue | Stop-Process -Force
foreach ($link in @(
        (Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs\AIO Display.lnk"),
        (Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs\AIO Loop Finder.lnk"),
        (Join-Path ([Environment]::GetFolderPath("CommonStartup")) "AIO Display.lnk"),
        (Join-Path ([Environment]::GetFolderPath("Startup")) "AIO Display.lnk"))) {
    if (Test-Path $link) {
        Remove-Item -Force $link
        Write-Host "Removed $link"
    }
}

if (Test-Path $InstallDir) {
    Start-Sleep -Milliseconds 500   # let the process exit and release the exe
    Remove-Item -Recurse -Force $InstallDir
    Write-Host "Removed $InstallDir"
}
Write-Host "Kept settings in $(Join-Path $env:ProgramData 'aio-ui')"
