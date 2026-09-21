; Inno Setup script for the esMail Windows installer.
;
; Build (from the repository root, after `cargo build --release`):
;
;     iscc /DAppVersion=1.2.3 installer\esmail.iss
;
; The result is dist\esmail-<version>-setup.exe. CI does exactly that in
; .github/workflows/build.yml.
;
; Design notes
;  * Per-user install by default (no UAC prompt): esMail keeps everything in the
;    user's profile, so it needs nothing outside it. The wizard offers "all
;    users" for whoever wants it.
;  * Everything is compressed as one solid LZMA2 stream at the highest setting.
;  * Uninstalling asks whether to remove the user's data as well. That work is
;    done by `esmail.exe --purge-data` (see crates/esmail/src/uninstall.rs), so
;    the program that knows where its files are is the one that deletes them.
;    A silent uninstall keeps the data unless /PURGE is passed:
;        unins000.exe /VERYSILENT /PURGE

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\esmail.exe"
#endif

#define AppName "esMail"
#define AppExeName "esmail.exe"

[Setup]
; Never change this GUID: it is how upgrades and the uninstaller find the
; existing installation.
AppId={{D40DC2ED-5A3C-4BBC-A51D-3D063427D76B}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=va1erian
AppPublisherURL=https://github.com/va1erian/esmail
AppSupportURL=https://github.com/va1erian/esmail/issues
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Refuse to install or uninstall over a running copy (the name is
; shell::SINGLE_INSTANCE_MUTEX in crates/esmail/src/shell.rs; a unit test keeps
; the two in step). esMail sits in the tray, so the message tells the user to
; quit it from there.
AppMutex=esMail.SingleInstance
Compression=lzma2/ultra64
SolidCompression=yes
InternalCompressLevel=ultra64
OutputDir=..\dist
OutputBaseFilename=esmail-{#AppVersion}-setup
SetupIconFile=..\crates\esmail\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}
WizardStyle=modern
ShowLanguageDialog=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The AppUserModelID ties these shortcuts, the taskbar button and toast
; notifications together (shell::APP_USER_MODEL_ID; also unit-tested).
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; AppUserModelID: "io.github.va1erian.esmail"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; AppUserModelID: "io.github.va1erian.esmail"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
var
  PurgeUserData: Boolean;

function HasSwitch(const Name: string): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
    begin
      Result := True;
      Exit;
    end;
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  if UninstallSilent then
    // Nobody to ask: keep the data unless the caller said otherwise.
    PurgeUserData := HasSwitch('/PURGE')
  else
    PurgeUserData := MsgBox(
      'Do you also want to remove esMail''s data for this Windows user?' + #13#10 + #13#10 +
      'This deletes your account settings, the cached mail and the passwords esMail saved in Windows Credential Manager. ' +
      'Your mail on the server is not affected.' + #13#10 + #13#10 +
      'Choose No to keep it, for example to reinstall later.',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  // usUninstall: about to delete the program files, so esmail.exe is still here.
  if (CurUninstallStep = usUninstall) and PurgeUserData then
    Exec(ExpandConstant('{app}\{#AppExeName}'), '--purge-data', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
