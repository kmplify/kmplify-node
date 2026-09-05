; Inno Setup script for the Windows installer.
;
;   iscc /DAppVersion=0.7.0 /DSourceDir=..\..\target\release packaging\windows\kmplify-node.iss
;
; Installs per user by default (no elevation, HKCU autostart, the user's
; PATH), which is also where the node directory (%APPDATA%\kmplify-node)
; lives; "install for all users" is offered when the installer is run
; elevated. The binary is the whole program: the window, the terminal
; dashboard and every CLI command are the same kmplify-node.exe.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\release"
#endif

[Setup]
AppId={{7D0C3E1A-5B6F-4E2C-9A1D-2F8B7C4E6A31}
AppName=KMPLIFY Node
AppVersion={#AppVersion}
AppVerName=KMPLIFY Node {#AppVersion}
AppPublisher=KMPLIFY
AppPublisherURL=https://kmplify.io
AppSupportURL=https://github.com/kmplify/kmplify-node
AppUpdatesURL=https://github.com/kmplify/kmplify-node/releases
DefaultDirName={autopf}\KMPLIFY Node
DefaultGroupName=KMPLIFY Node
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=.
OutputBaseFilename=kmplify-node-{#AppVersion}-setup
SetupIconFile=..\icons\kmplify-node.ico
UninstallDisplayIcon={app}\kmplify-node.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ChangesEnvironment=yes

[Tasks]
Name: "addtopath"; Description: "Add kmplify-node to my PATH (for the terminal commands)"
Name: "autostart"; Description: "Open KMPLIFY Node when I sign in"; Flags: unchecked
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\kmplify-node.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\icons\kmplify-node.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\NOTICE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\docs\ROUTER.md"; DestDir: "{app}\docs"; Flags: ignoreversion

[Icons]
Name: "{group}\KMPLIFY Node"; Filename: "{app}\kmplify-node.exe"; Parameters: "gui"; IconFilename: "{app}\kmplify-node.ico"; Comment: "The desktop window: this machine's node and the LAN router"
Name: "{group}\KMPLIFY Node terminal dashboard"; Filename: "{app}\kmplify-node.exe"; Parameters: "tui --router"; IconFilename: "{app}\kmplify-node.ico"; Comment: "The same node and router, in a console"
Name: "{autodesktop}\KMPLIFY Node"; Filename: "{app}\kmplify-node.exe"; Parameters: "gui"; IconFilename: "{app}\kmplify-node.ico"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "KMPLIFY Node"; ValueData: """{app}\kmplify-node.exe"" gui"; Flags: uninsdeletevalue; Tasks: autostart
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}'))

[Run]
Filename: "{app}\kmplify-node.exe"; Parameters: "gui"; Description: "Open KMPLIFY Node now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; A window hidden in the tray would otherwise keep the binary locked. Only
; the processes running THIS install's binary: a node built from source
; (cargo install) on the same machine is somebody else's and stays up.
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Get-Process kmplify-node -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{app}\*' }} | Stop-Process -Force"""; Flags: runhidden; RunOnceId: "stopnode"

[Code]
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

// The PATH entry the addtopath task appended is removed again on
// uninstall; Inno does not undo an expandsz append by itself.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, App: string;
  P: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Path) then
    begin
      App := ';' + ExpandConstant('{app}');
      P := Pos(Uppercase(App), Uppercase(Path));
      if P > 0 then
      begin
        Delete(Path, P, Length(App));
        RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Path);
      end;
    end;
  end;
end;
