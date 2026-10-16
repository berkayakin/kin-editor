; Windows installer (Inno Setup 6), built by .github/workflows/release.yml:
;   ISCC.exe /DVersion=<version> scripts\kuana.iss
; Installs per user (no admin prompt) so the updater can replace kuana.exe in place.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{8E4C1D2B-6F0A-4B57-9C3E-2A7D5F1B9E64}
AppName=Kuana
AppVersion={#Version}
AppVerName=Kuana {#Version}
AppPublisher=Kuana
AppPublisherURL=https://github.com/berkayakin/kuana-editor
DefaultDirName={autopf}\Kuana
DefaultGroupName=Kuana
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=..\assets\app-icon\windows\kuana.ico
UninstallDisplayIcon={app}\kuana.exe
OutputDir=..\dist
OutputBaseFilename=Kuana-{#Version}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\kuana.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Kuana"; Filename: "{app}\kuana.exe"
Name: "{autodesktop}\Kuana"; Filename: "{app}\kuana.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\kuana.exe"; Description: "{cm:LaunchProgram,Kuana}"; Flags: nowait postinstall skipifsilent

[InstallDelete]
; Left by installs from when Kuana was called Kin (same AppId, so same folder).
Type: files; Name: "{app}\kin.exe"
Type: files; Name: "{autoprograms}\Kin.lnk"
Type: files; Name: "{autodesktop}\Kin.lnk"

[UninstallDelete]
; What the updater leaves beside the executable.
Type: files; Name: "{app}\kuana.exe.old"
Type: filesandordirs; Name: "{app}\.kuana-update"
