#ifndef SourceRoot
#define SourceRoot "..\.."
#endif

#ifndef BinaryPath
#define BinaryPath SourceRoot + "\target\release\elowen-edge.exe"
#endif

#ifndef OutputDir
#define OutputDir SourceRoot + "\dist"
#endif

#ifndef AppVersion
#define AppVersion "0.1.0"
#endif

#define AppName "Elowen Edge"
#define AppPublisher "Elowen"
#define AppExeName "elowen-edge.exe"
#define AppId "{{E3CF8B51-605C-4E80-B48D-7C520742BC0F}"

[Setup]
AppId={#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\Elowen\Edge
DefaultGroupName=Elowen
DisableProgramGroupPage=yes
OutputDir={#OutputDir}
OutputBaseFilename=ElowenEdgeSetup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
UninstallDisplayIcon={app}\{#AppExeName}
SetupLogging=yes

[Tasks]
Name: "registertask"; Description: "Install Elowen Edge as a background scheduled task"; GroupDescription: "Background service:"; Flags: checkedonce
Name: "startedge"; Description: "Start Elowen Edge after installation"; GroupDescription: "Background service:"; Flags: checkedonce
Name: "desktopshortcut"; Description: "Create a desktop shortcut for the TUI"; GroupDescription: "Shortcuts:"; Flags: checkedonce
Name: "startmenushortcut"; Description: "Create a Start Menu shortcut for the TUI"; GroupDescription: "Shortcuts:"; Flags: checkedonce

[Files]
Source: "{#BinaryPath}"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion
Source: "{#SourceRoot}\edge.toml.example"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceRoot}\scripts\windows\Start-ElowenEdge.ps1"; DestDir: "{app}\scripts\windows"; Flags: ignoreversion
Source: "{#SourceRoot}\scripts\windows\Register-ElowenEdgeTask.ps1"; DestDir: "{app}\scripts\windows"; Flags: ignoreversion
Source: "{#SourceRoot}\scripts\windows\Install-ElowenEdgeTuiShortcut.ps1"; DestDir: "{app}\scripts\windows"; Flags: ignoreversion; AfterInstall: InstallConfigAndSecrets

[Run]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\windows\Register-ElowenEdgeTask.ps1"" -TaskName ""ElowenEdge"" -ConfigFile ""{app}\edge.toml"" -BinaryPath ""{app}\{#AppExeName}"" -SkipTunnel"; StatusMsg: "Registering Elowen Edge scheduled task..."; Flags: runhidden waituntilterminated; Tasks: registertask
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Start-ScheduledTask -TaskName 'ElowenEdge'"""; StatusMsg: "Starting Elowen Edge..."; Flags: runhidden waituntilterminated; Tasks: startedge; Check: ShouldInstallTask
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\windows\Install-ElowenEdgeTuiShortcut.ps1"" -ConfigFile ""{app}\edge.toml"" -BinaryPath ""{app}\{#AppExeName}"" -Location Desktop"; StatusMsg: "Creating desktop TUI shortcut..."; Flags: runhidden waituntilterminated; Tasks: desktopshortcut
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\windows\Install-ElowenEdgeTuiShortcut.ps1"" -ConfigFile ""{app}\edge.toml"" -BinaryPath ""{app}\{#AppExeName}"" -Location StartMenu"; StatusMsg: "Creating Start Menu TUI shortcut..."; Flags: runhidden waituntilterminated; Tasks: startmenushortcut

[UninstallRun]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Stop-ScheduledTask -TaskName 'ElowenEdge' -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName 'ElowenEdge' -Confirm:$false -ErrorAction SilentlyContinue"""; Flags: runhidden waituntilterminated; RunOnceId: "RemoveElowenEdgeTask"

[Code]
function ShouldInstallTask: Boolean;
begin
  Result := WizardIsTaskSelected('registertask');
end;

procedure CopyDirectoryRecursive(SourceDir: String; DestDir: String);
var
  FindRec: TFindRec;
  SourcePath: String;
  DestPath: String;
begin
  if not DirExists(SourceDir) then begin
    RaiseException('Directory not found: ' + SourceDir);
  end;

  ForceDirectories(DestDir);

  if FindFirst(AddBackslash(SourceDir) + '*', FindRec) then begin
    try
      repeat
        if (FindRec.Name <> '.') and (FindRec.Name <> '..') then begin
          SourcePath := AddBackslash(SourceDir) + FindRec.Name;
          DestPath := AddBackslash(DestDir) + FindRec.Name;
          if (FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then begin
            CopyDirectoryRecursive(SourcePath, DestPath);
          end else begin
            if not CopyFile(SourcePath, DestPath, False) then begin
              RaiseException('Failed to copy ' + SourcePath + ' to ' + DestPath);
            end;
          end;
        end;
      until not FindNext(FindRec);
    finally
      FindClose(FindRec);
    end;
  end;
end;

procedure InstallConfigAndSecrets;
var
  ConfigSource: String;
  SecretSourceDir: String;
  TargetConfig: String;
  DefaultConfig: String;
begin
  ConfigSource := ExpandConstant('{param:CONFIGSOURCE|}');
  SecretSourceDir := ExpandConstant('{param:SECRETSOURCEDIR|}');
  TargetConfig := ExpandConstant('{app}\edge.toml');
  DefaultConfig := ExpandConstant('{app}\edge.toml.example');

  if ConfigSource <> '' then begin
    if not FileExists(ConfigSource) then begin
      RaiseException('Config source file not found: ' + ConfigSource);
    end;
    if not CopyFile(ConfigSource, TargetConfig, False) then begin
      RaiseException('Failed to copy config source to ' + TargetConfig);
    end;
  end else if not FileExists(TargetConfig) then begin
    if not CopyFile(DefaultConfig, TargetConfig, False) then begin
      RaiseException('Failed to create default config at ' + TargetConfig);
    end;
  end;

  if SecretSourceDir <> '' then begin
    CopyDirectoryRecursive(SecretSourceDir, ExpandConstant('{app}\secrets'));
  end;
end;
