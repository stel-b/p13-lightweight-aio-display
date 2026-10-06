; Inno Setup script for AIO Display. Build after `cargo build --release`:
;   ISCC installer\aio-display.iss /DAppVersion=0.1.0
; The output goes to target\installer\.

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#define BinDir "..\target\release"
#define ServiceName "aio-daemon"
#define AppGuid "6B8F3C2A-4D1E-4F7B-9A05-1C62B0E02F13"

[Setup]
AppId={{{#AppGuid}}
AppName=AIO Display for MSI P13
AppVersion={#AppVersion}
AppPublisher=p13-lightweight-aio-display (community project)
AppPublisherURL=https://github.com/stel-b/p13-lightweight-aio-display
AppSupportURL=https://github.com/stel-b/p13-lightweight-aio-display/issues
DefaultDirName={autopf}\aio-ui
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=aio-display-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
#ifexist "..\LICENSE"
LicenseFile=..\LICENSE
#endif
InfoBeforeFile=before-install.txt
InfoAfterFile=after-install.txt
UninstallDisplayName=AIO Display for MSI P13
UninstallDisplayIcon={app}\aio-ui.exe
; The service and our apps are stopped in [Code] instead.
CloseApplications=no

[Tasks]
Name: "videosupport"; Description: "Video support (needs ffmpeg installed: winget install Gyan.FFmpeg)"; Flags: unchecked
Name: "trayicon"; Description: "Show the AIO Display tray icon at login (for all users, about 2 MB)"; Flags: unchecked

[Files]
Source: "{#BinDir}\aio-daemon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\aio-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\aio-ui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\aio-loop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\aio-loop-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
#ifexist "..\LICENSE"
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
#endif

[Icons]
Name: "{autoprograms}\AIO Display"; Filename: "{app}\aio-ui.exe"; Comment: "AIO Display settings"
Name: "{autoprograms}\AIO Loop Finder"; Filename: "{app}\aio-loop.exe"; Comment: "Find a seamless loop in a video"
; All-users Startup folder: setup runs elevated, possibly as another admin account.
Name: "{commonstartup}\AIO Display"; Filename: "{app}\aio-ui.exe"; Parameters: "--tray"; Tasks: trayicon

[Run]
Filename: "{app}\aio-ui.exe"; Description: "Open AIO Display"; Flags: postinstall nowait skipifsilent

[Code]
const
  ServiceKey = 'SYSTEM\CurrentControlSet\Services\{#ServiceName}';
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{{#AppGuid}}_is1';
  FfmpegMissing = 'Video support needs ffmpeg and ffprobe, but they were not found on your PATH.' + #13#10#13#10 +
    'Install ffmpeg (for example: winget install Gyan.FFmpeg), then start this setup again.' + #13#10 +
    'Or untick "Video support" to install without it.';

var
  TasksDefaulted: Boolean;

{ Full path of ffmpeg.exe if both ffmpeg and ffprobe are available, else ''.
  The service runs as LocalSystem, which does not see your PATH, so this
  path is handed to it explicitly. }
function FindFfmpeg(): String;
var
  Ffmpeg: String;
begin
  Result := '';
  Ffmpeg := FileSearch('ffmpeg.exe', GetEnv('PATH'));
  if Ffmpeg = '' then
    Exit;
  if FileExists(ExtractFilePath(Ffmpeg) + 'ffprobe.exe') or (FileSearch('ffprobe.exe', GetEnv('PATH')) <> '') then
    Result := Ffmpeg;
end;

function IsUpgrade(): Boolean;
begin
  Result := RegKeyExists(HKLM, UninstallKey) or RegKeyExists(HKCU, UninstallKey);
end;

{ Fresh installs: pre-tick video support when ffmpeg is already installed.
  Upgrades keep the previous choice. }
procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpSelectTasks) and not TasksDefaulted then
  begin
    TasksDefaulted := True;
    if not IsUpgrade() and (FindFfmpeg() <> '') then
      WizardSelectTasks('videosupport');
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpSelectTasks) and WizardIsTaskSelected('videosupport') and (FindFfmpeg() = '') then
  begin
    MsgBox(FfmpegMissing, mbError, MB_OK);
    Result := False;
  end;
end;

{ Runs a program hidden and returns its exit code (-1 if it could not start). }
function RunHidden(const Exe, Params: String): Integer;
var
  Code: Integer;
begin
  if Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, Code) then
    Result := Code
  else
    Result := -1;
end;

function ServiceExists(): Boolean;
begin
  Result := RunHidden(ExpandConstant('{sys}\sc.exe'), 'query {#ServiceName}') = 0;
end;

{ Stops the service (net stop waits until it has stopped) and closes our apps,
  so their files can be replaced or removed. }
procedure StopEverything();
begin
  if ServiceExists() then
    RunHidden(ExpandConstant('{sys}\net.exe'), 'stop {#ServiceName}');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM aio-ui.exe');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM aio-loop.exe');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  { Also covers silent installs, where the tasks page is skipped. }
  if WizardIsTaskSelected('videosupport') and (FindFfmpeg() = '') then
  begin
    Result := FfmpegMissing;
    Exit;
  end;
  StopEverything();
  Result := '';
end;

{ Registers (or updates) the service, its recovery actions and where ffmpeg
  is, then starts it. }
procedure InstallService();
var
  Sc, BinPath, Ffmpeg: String;
begin
  Sc := ExpandConstant('{sys}\sc.exe');
  BinPath := '"\"' + ExpandConstant('{app}') + '\aio-daemon.exe\" --service"';
  if ServiceExists() then
    RunHidden(Sc, 'config {#ServiceName} binPath= ' + BinPath + ' start= auto')
  else
    RunHidden(Sc, 'create {#ServiceName} binPath= ' + BinPath + ' start= auto DisplayName= "AIO Display (MSI P13 LCD)"');
  RunHidden(Sc, 'description {#ServiceName} "Shows the configured color, image or animation on the MSI MPG CoreLiquid P13 pump display."');
  RunHidden(Sc, 'failure {#ServiceName} reset= 86400 actions= restart/5000/restart/5000/restart/5000');
  RunHidden(Sc, 'failureflag {#ServiceName} 1');

  { Video support: pass ffmpeg's location in the service's own environment. }
  Ffmpeg := '';
  if WizardIsTaskSelected('videosupport') then
    Ffmpeg := FindFfmpeg();
  if Ffmpeg <> '' then
    RegWriteMultiStringValue(HKLM, ServiceKey, 'Environment', 'AIO_FFMPEG=' + Ffmpeg)
  else
    RegDeleteValue(HKLM, ServiceKey, 'Environment');

  RunHidden(ExpandConstant('{sys}\net.exe'), 'start {#ServiceName}');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    InstallService();
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopEverything();
    if ServiceExists() then
      RunHidden(ExpandConstant('{sys}\sc.exe'), 'delete {#ServiceName}');
  end;
end;
