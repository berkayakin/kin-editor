; Windows installer (Inno Setup 6), built by .github/workflows/release.yml:
;   ISCC.exe /DVersion=<version> scripts\kin.iss
; Installs per user (no admin prompt) so the updater can replace kin.exe in place.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{8E4C1D2B-6F0A-4B57-9C3E-2A7D5F1B9E64}
AppName=Kin
AppVersion={#Version}
AppVerName=Kin {#Version}
AppPublisher=Kin
AppPublisherURL=https://github.com/berkayakin/kin-editor
DefaultDirName={autopf}\Kin
DefaultGroupName=Kin
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=..\assets\app-icon\windows\kin.ico
UninstallDisplayIcon={app}\kin.exe
OutputDir=..\dist
OutputBaseFilename=Kin-{#Version}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\kin.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Kin"; Filename: "{app}\kin.exe"
Name: "{autodesktop}\Kin"; Filename: "{app}\kin.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\kin.exe"; Description: "{cm:LaunchProgram,Kin}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; What the updater leaves beside the executable.
Type: files; Name: "{app}\kin.exe.old"
Type: filesandordirs; Name: "{app}\.kin-update"
