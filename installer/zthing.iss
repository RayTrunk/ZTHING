; Inno Setup script for ZTHING. Built by installer\package.ps1, which passes
; the version and the folder with the release files:
;   iscc /DVersion=0.1.0 /DSource=..\dist\ZTHING-0.1.0 installer\zthing.iss

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Source
  #error Pass /DSource=<folder with the release files>
#endif

[Setup]
; Keeps upgrades and the uninstaller entry together across versions. Never change it.
AppId={{6E2B6C51-8F0D-4C2B-9C59-5A3E7D0F9B21}
AppName=ZTHING
AppVersion={#Version}
AppVerName=ZTHING {#Version}
AppPublisher=R.Trunk
AppCopyright=(C) 2026 R.Trunk
; The GPL asks for the license to reach everyone who receives the program.
LicenseFile={#Source}\LICENSE
DefaultDirName={autopf}\ZTHING
DefaultGroupName=ZTHING
DisableProgramGroupPage=yes
; The service runs as LocalSystem and has to be registered by an administrator.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=ZTHING-{#Version}-setup
SetupIconFile={#Source}\zthing.ico
UninstallDisplayIcon={app}\zthing-gui.exe
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
; Closes a running search window through the Restart Manager before files are replaced.
CloseApplications=yes
ChangesEnvironment=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "de"; MessagesFile: "compiler:Languages\German.isl"

[CustomMessages]
en.AutoStart=Start ZTHING when I sign in to Windows
de.AutoStart=ZTHING bei der Anmeldung starten
en.AddToPath=Add the command line tool zt to PATH
de.AddToPath=Kommandozeilenwerkzeug zt zum PATH hinzufügen
en.InstallService=Starting the ZTHING service ...
de.InstallService=ZTHING-Dienst wird gestartet …

[Tasks]
Name: "autostart"; Description: "{cm:AutoStart}"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked
Name: "addtopath"; Description: "{cm:AddToPath}"; Flags: unchecked

[Files]
Source: "{#Source}\zthing-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\zthing-service.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\zt.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\zthing.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\sdk\*"; DestDir: "{app}\sdk"; Flags: ignoreversion recursesubdirs
Source: "{#Source}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\README.de.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\ZTHING"; Filename: "{app}\zthing-gui.exe"
Name: "{autodesktop}\ZTHING"; Filename: "{app}\zthing-gui.exe"; Tasks: desktopicon
Name: "{commonstartup}\ZTHING"; Filename: "{app}\zthing-gui.exe"; Tasks: autostart

[Registry]
; URL protocol zthing:<query>, see docs/API.md.
Root: HKA; Subkey: "Software\Classes\zthing"; ValueType: string; ValueName: ""; ValueData: "URL:ZTHING search"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\zthing"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\zthing\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\zthing-gui.exe"",0"
Root: HKA; Subkey: "Software\Classes\zthing\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\zthing-gui.exe"" ""%1"""
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NotInPath

[Run]
; Registers the service or updates an existing registration, then starts it.
Filename: "{app}\zthing-service.exe"; Parameters: "install"; StatusMsg: "{cm:InstallService}"; Flags: runhidden waituntilterminated
Filename: "{app}\zthing-gui.exe"; Description: "{cm:LaunchProgram,ZTHING}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/IM zthing-gui.exe /F"; Flags: runhidden; RunOnceId: "StopWindow"
; Stops the service and waits until it has, so its executable can be deleted.
Filename: "{app}\zthing-service.exe"; Parameters: "uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"

[UninstallDelete]
; The service's saved index.
Type: filesandordirs; Name: "{sys}\config\systemprofile\AppData\Local\ZTHING"

[Code]
function NotInPath(): Boolean;
var
  Path: string;
begin
  if not RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Path) then
    Path := '';
  Result := Pos(';' + Uppercase(ExpandConstant('{app}')) + ';', ';' + Uppercase(Path) + ';') = 0;
end;

{ The running service holds zthing-service.exe open. "net stop" returns once it has stopped. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\net.exe'), 'stop zthing', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

{ Takes the folder out of PATH again if the installer put it there. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, Entry: string;
  At: Integer;
begin
  if CurUninstallStep <> usPostUninstall then
    exit;
  if not RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Path) then
    exit;
  Entry := ';' + ExpandConstant('{app}');
  At := Pos(Uppercase(Entry), Uppercase(Path));
  if At > 0 then
  begin
    Delete(Path, At, Length(Entry));
    RegWriteExpandStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Path);
  end;
end;
