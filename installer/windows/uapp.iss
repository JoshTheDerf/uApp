; Inno Setup script for uapp (open-source installer, https://jrsoftware.org/isinfo.php).
; Per-user install (no admin / UAC), registers the .uapp file association so
; double-clicking a .uapp opens it. Build:  iscc uapp.iss
; The build workflow stages uapp.exe (the GUI desktop app), the icons and LICENSE.txt next to this file.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
; Display/brand name shown to users. The executable and ProgID identifiers
; stay lowercase/stable so upgrades and the association don't break.
#define AppName "UApp"
#define AppPublisher "Joshua Bemenderfer"
#define AppExe "uapp.exe"

[Setup]
; Keep this GUID stable across releases so upgrades replace in place.
AppId={{7F3A6E2C-9B4D-4E1A-A6F2-8C1D5B0E9A44}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppSupportURL=https://github.com/JoshTheDerf/uApp
DefaultDirName={localappdata}\Programs\uapp
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
OutputBaseFilename=uapp-setup
OutputDir=.
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes
SetupIconFile=uapp.ico
UninstallDisplayIcon={app}\{#AppExe}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "launchapp"; Description: "Open uapp after installing"; Flags: unchecked

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "uapp.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "uapp-file.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; IconFilename: "{app}\uapp.ico"

[Registry]
; Per-user .uapp -> uapp.File association (HKCU, no admin needed).
Root: HKCU; Subkey: "Software\Classes\.uapp"; ValueType: string; ValueName: ""; ValueData: "uapp.File"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.uapp"; ValueType: string; ValueName: "Content Type"; ValueData: "application/x-uapp"; Flags: uninsdeletevalue
; Modern OpenWithProgids advertisement (drives "Open with" + Default Apps).
Root: HKCU; Subkey: "Software\Classes\.uapp\OpenWithProgids"; ValueType: string; ValueName: "uapp.File"; ValueData: ""; Flags: uninsdeletevalue
; The ProgID: description, icon, and the command that opens a .uapp.
Root: HKCU; Subkey: "Software\Classes\uapp.File"; ValueType: string; ValueName: ""; ValueData: "UApp application"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\uapp.File\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\uapp-file.ico"
Root: HKCU; Subkey: "Software\Classes\uapp.File\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""
; Advertise the app so "Open with" and Default Apps list it.
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "UApp"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\uapp.ico"
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}\SupportedTypes"; ValueType: string; ValueName: ".uapp"; ValueData: ""

[Run]
Filename: "{app}\{#AppExe}"; Description: "Open uapp"; Flags: nowait postinstall skipifsilent; Tasks: launchapp
