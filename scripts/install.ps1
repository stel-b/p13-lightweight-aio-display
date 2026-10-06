<#
.SYNOPSIS
    Installs or updates AIO Display from a source build or the portable zip.

    Most people should use the installer (aio-display-setup-<version>.exe from
    the GitHub Releases page) instead; it does the same with a wizard.

.DESCRIPTION
    Does exactly this, and nothing else:
      1. Stops the service if it is already installed, and closes aio-ui.
      2. Copies aio-daemon.exe, aio-cli.exe, aio-ui.exe, aio-loop.exe and
         aio-loop-cli.exe to
         "$env:ProgramFiles\aio-ui".
      3. Registers the service on first install: automatic start at boot,
         runs as LocalSystem, command line "<exe>" --service.
      4. With -VideoSupport: sets the service's AIO_FFMPEG environment
         variable to your ffmpeg (needed to import videos). Without it the
         variable is removed and video sources are refused.
      5. Sets recovery: restart after 5 s on failure (also on error exits).
      6. Starts the service.
      7. Adds Start Menu shortcuts "AIO Display" (settings window) and
         "AIO Loop Finder" (makes seamless video loops),
         and with -Autostart a Startup-folder shortcut for the tray icon
         (aio-ui --tray, about 2 MB; it opens the window on demand).

    Settings, cache and logs live in "$env:ProgramData\aio-ui". The device
    key is built in; a device_key.pem there overrides it.

    Run from an Administrator PowerShell. Re-run after rebuilding to update.

.PARAMETER BinDir
    Folder with the binaries. Default: this script's folder if they are there
    (portable zip), else target\release in this repo.

.PARAMETER VideoSupport
    Enable video sources. ffmpeg and ffprobe must be installed (for example
    winget install Gyan.FFmpeg); the script stops before changing anything if
    they are not found.

.PARAMETER FfmpegPath
    With -VideoSupport: full path of ffmpeg.exe. Default: the one on your PATH.

.PARAMETER Autostart
    Also show the tray icon (aio-ui --tray) when you log in.
#>
#Requires -RunAsAdministrator
# No positional arguments: a typo like "--Autostart" must be an error, not a BinDir.
[CmdletBinding(PositionalBinding = $false)]
param(
    [string]$BinDir,
    [switch]$VideoSupport,
    [string]$FfmpegPath,
    [switch]$Autostart
)
$ErrorActionPreference = "Stop"

$ServiceName = "aio-daemon"            # must match SERVICE_NAME in service.rs
$DisplayName = "AIO Display (MSI P13 LCD)"
$Description = "Shows the configured color, image or animation on the MSI MPG CoreLiquid P13 pump display."
$InstallDir  = Join-Path $env:ProgramFiles "aio-ui"
$DataDir     = Join-Path $env:ProgramData "aio-ui"
$Binaries    = "aio-daemon.exe", "aio-cli.exe", "aio-ui.exe", "aio-loop.exe", "aio-loop-cli.exe"

if (-not $BinDir) {
    $BinDir = if (Test-Path (Join-Path $PSScriptRoot "aio-daemon.exe")) { $PSScriptRoot }
              else { Join-Path $PSScriptRoot "..\target\release" }
}
foreach ($name in $Binaries) {
    if (-not (Test-Path (Join-Path $BinDir $name))) {
        throw "$name not found in $BinDir. Build first: cargo build --release"
    }
}

# Video support: find ffmpeg (and ffprobe next to it or on PATH) before
# touching anything, so a missing ffmpeg never leaves a half-done install.
if ($VideoSupport) {
    if (-not $FfmpegPath) {
        $cmd = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
        if ($cmd) { $FfmpegPath = $cmd.Source }
    }
    $probeOk = $FfmpegPath -and ((Test-Path (Join-Path (Split-Path $FfmpegPath) "ffprobe.exe")) -or
                                 (Get-Command ffprobe.exe -ErrorAction SilentlyContinue))
    if (-not $FfmpegPath -or -not (Test-Path $FfmpegPath) -or -not $probeOk) {
        throw "Video support needs ffmpeg and ffprobe, but they were not found. Install ffmpeg (winget install Gyan.FFmpeg), open a new terminal and run this script again, or leave out -VideoSupport."
    }
}

# 1. Stop the existing service so its exe can be replaced.
$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing -and $existing.Status -ne "Stopped") {
    Write-Host "Stopping $ServiceName..."
    Stop-Service -Name $ServiceName
}
Get-Process -Name aio-ui, aio-loop -ErrorAction SilentlyContinue | Stop-Process -Force

# 2. Copy the binaries (retry briefly: the old process may still be exiting).
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($name in $Binaries) {
    $target = Join-Path $InstallDir $name
    for ($try = 1; ; $try++) {
        try {
            Copy-Item -Path (Join-Path $BinDir $name) -Destination $target -Force
            break
        } catch {
            if ($try -ge 20) { throw }
            Start-Sleep -Milliseconds 250
        }
    }
}
Write-Host "Copied binaries to $InstallDir"

# 3. Register the service (first install only; updates just replace the exe).
$exe = Join-Path $InstallDir "aio-daemon.exe"
if (-not $existing) {
    New-Service -Name $ServiceName `
        -BinaryPathName "`"$exe`" --service" `
        -DisplayName $DisplayName `
        -Description $Description `
        -StartupType Automatic | Out-Null
    Write-Host "Registered service $ServiceName (LocalSystem, automatic start)"
}

# 4. Video support: tell the service where ffmpeg is. A per-user install
#     (winget) is not on LocalSystem's PATH, so the path is passed in the
#     service's own environment (registry value "Environment").
$serviceKey = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
if ($VideoSupport) {
    New-ItemProperty -Path $serviceKey -Name Environment -PropertyType MultiString `
        -Value @("AIO_FFMPEG=$FfmpegPath") -Force | Out-Null
    Write-Host "Video support: using $FfmpegPath"
} else {
    Remove-ItemProperty -Path $serviceKey -Name Environment -ErrorAction SilentlyContinue
    Write-Host "Video support off (use -VideoSupport to enable it)"
}

# 5. Recovery: restart after 5 s, reset the failure count after a day.
sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null
sc.exe failureflag $ServiceName 1 | Out-Null

# 6. Start it.
Start-Service -Name $ServiceName
Get-Service -Name $ServiceName | Format-Table -AutoSize Name, Status, StartType
# 7. Shortcuts.
$shell = New-Object -ComObject WScript.Shell
function New-Shortcut($path, $arguments, $exe = "aio-ui.exe", $description = "AIO Display settings") {
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = Join-Path $InstallDir $exe
    $link.Arguments = $arguments
    $link.WorkingDirectory = $InstallDir
    $link.Description = $description
    $link.Save()
}
$startMenu = Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs"
New-Shortcut (Join-Path $startMenu "AIO Display.lnk") ""
New-Shortcut (Join-Path $startMenu "AIO Loop Finder.lnk") "" "aio-loop.exe" "Find a seamless loop in a video"
Write-Host "Start Menu: AIO Display, AIO Loop Finder"
if ($Autostart) {
    New-Shortcut (Join-Path ([Environment]::GetFolderPath("CommonStartup")) "AIO Display.lnk") "--tray"
    Write-Host "The tray icon will start at login"
}

Write-Host "Logs: $DataDir\logs"
Write-Host "Control it with: & `"$InstallDir\aio-cli.exe`" status"
