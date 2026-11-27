#ifndef AppVersion
  #error Pass the widget version with /DAppVersion=X.Y.Z.
#endif

#define AppName "LAMZU Battery Widget"
#define AppId "VozdyxDev.LamzuBatteryWidget"
#define AppExe "lamzu-widget.exe"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
AppId={#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=VozdyxDev
AppPublisherURL=https://github.com/VozdyxDev/lamzu-battery-widget
AppUpdatesURL=https://github.com/VozdyxDev/lamzu-battery-widget/releases
DefaultDirName={localappdata}\Programs\{#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
AppMutex=Local\LamzuWallpaperMonitor
RestartApplications=no
UsePreviousTasks=no
UninstallDisplayIcon={app}\{#AppExe}
OutputDir=..\target
OutputBaseFilename=LAMZU-Battery-Widget-windows-x64-setup
WizardStyle=modern

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[CustomMessages]
en.RunAtLogon=Start LAMZU Battery Widget when I sign in to Windows
ru.RunAtLogon=Запускать LAMZU Battery Widget при входе в Windows
en.AutostartPathTooLong=The installation path is too long for Windows startup. Choose a shorter folder or clear the startup option.
ru.AutostartPathTooLong=Путь установки слишком длинный для автозапуска Windows. Выберите более короткий путь или снимите флажок автозапуска.
en.RemoveAutostartFailed=Could not remove LAMZU Battery Widget from Windows startup.
ru.RemoveAutostartFailed=Не удалось убрать LAMZU Battery Widget из автозапуска Windows.

[Tasks]
Name: "autostart"; Description: "{cm:RunAtLogon}"

[Files]
Source: "..\target\x86_64-pc-windows-msvc\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; ValueData: "{code:InstalledStartupCommand}"; Tasks: autostart
Root: HKCU; Subkey: "{#RunKey}"; ValueType: none; ValueName: "{#AppName}"; Flags: deletevalue dontcreatekey; Tasks: not autostart; Check: HasInstalledAutostart

[Run]
Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent unchecked

[Code]
const
  StartupKey = '{#RunKey}';
  StartupValue = '{#AppName}';
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppId}_is1';

var
  AutostartDefaultLoaded: Boolean;

function StartupCommand(const Directory: String): String;
begin
  Result := '"' + AddBackslash(Directory) + '{#AppExe}"';
end;

function InstalledStartupCommand(Param: String): String;
begin
  Result := StartupCommand(ExpandConstant('{app}'));
end;

function HasAutostartFor(const Directory: String): Boolean;
var
  Command: String;
begin
  Result := RegQueryStringValue(HKCU, StartupKey, StartupValue, Command) and
    (CompareText(Command, StartupCommand(Directory)) = 0);
end;

function HasInstalledAutostart: Boolean;
begin
  Result := HasAutostartFor(ExpandConstant('{app}'));
end;

procedure LoadAutostartDefault;
var
  PreviousDirectory: String;
begin
  if AutostartDefaultLoaded then
    Exit;
  AutostartDefaultLoaded := True;

  { Command-line task selection takes precedence over the upgrade default. }
  if (ExpandConstant('{param:tasks|default}') <> 'default') or
    (ExpandConstant('{param:mergetasks|default}') <> 'default') then
    Exit;

  if RegQueryStringValue(HKCU, UninstallKey, 'InstallLocation', PreviousDirectory) and
    not HasAutostartFor(PreviousDirectory) then
    WizardSelectTasks('!autostart');
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpSelectTasks then
    LoadAutostartDefault;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  LoadAutostartDefault;
  Result := '';
  if WizardIsTaskSelected('autostart') and (Length(StartupCommand(WizardDirValue)) > 260) then
    Result := CustomMessage('AutostartPathTooLong');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usUninstall) and HasInstalledAutostart and
    not RegDeleteValue(HKCU, StartupKey, StartupValue) then
  begin
    Log(CustomMessage('RemoveAutostartFailed'));
    SuppressibleMsgBox(CustomMessage('RemoveAutostartFailed'), mbError, MB_OK, IDOK);
  end;
end;
