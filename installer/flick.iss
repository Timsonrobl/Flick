; Flick's installer (Inno Setup 6). Build it with tools\installer.ps1, which
; builds flick.exe and the licence notices first and passes the version.
;
; Per user, no admin rights: Flick goes to %LOCALAPPDATA%\Programs\Flick and
; registers its file types under HKEY_CURRENT_USER (flick.exe --register,
; register.rs). A running Flick is asked to quit first (flick.exe --quit).
; Settings, stars and the close history (%LOCALAPPDATA%\Flick) outlive an
; uninstall, so a reinstall picks them up.

#ifndef Version
  #error Build with tools\installer.ps1 (it passes /DVersion and /DVersionNumbers)
#endif
; Version: Cargo's, "1.1.0" or a local build's "1.1.0-dev". VersionNumbers:
; its numeric part, as setup.exe's own version resource must be.

[Setup]
; Never change: Windows knows the installed Flick by it.
AppId={{67D47C8A-C813-4E72-9C48-BA9EDC68B2A1}
AppName=Flick
AppVersion={#Version}
AppVerName=Flick {#Version}
AppPublisher=Timsonrobl
AppPublisherURL=https://github.com/Timsonrobl/Flick
AppSupportURL=https://github.com/Timsonrobl/Flick
VersionInfoVersion={#VersionNumbers}
VersionInfoTextVersion={#Version}
VersionInfoProductTextVersion={#Version}
PrivilegesRequired=lowest
DefaultDirName={autopf}\Flick
DisableDirPage=auto
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
SetupIconFile=..\assets\icon\flick.ico
UninstallDisplayIcon={app}\flick.exe
UninstallDisplayName=Flick
OutputDir=..\target\installer
OutputBaseFilename=Flick-{#Version}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes
; A Flick that ignored --quit (a standalone window): Windows' Restart
; Manager offers to close it.
CloseApplications=yes
RestartApplications=no

[Files]
Source: "..\target\release\flick.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\target\installer\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Run]
Filename: "{app}\flick.exe"; Parameters: "--register"; StatusMsg: "Registering Flick's file types..."; Flags: runhidden waituntilterminated
Filename: "ms-settings:defaultapps?registeredAppUser=Flick"; Description: "Choose Flick as the default image viewer (opens Settings)"; Flags: postinstall shellexec nowait skipifsilent

[UninstallRun]
Filename: "{app}\flick.exe"; Parameters: "--quit"; RunOnceId: "Quit"; Flags: runhidden waituntilterminated
Filename: "{app}\flick.exe"; Parameters: "--unregister"; RunOnceId: "Unregister"; Flags: runhidden waituntilterminated

[UninstallDelete]
; Whatever Flick left beside itself (a replaced exe, an update waiting).
Type: filesandordirs; Name: "{app}"

[Code]
// The installed Flick, if any, quits before its exe is replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\flick.exe')) then
    Exec(ExpandConstant('{app}\flick.exe'), '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;
