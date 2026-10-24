; drive-sync Windows 安装包（Inno Setup 6）
; CI 中由 release.yml 替换 __VERSION__ 后编译，产物 dist\DriveSync-Setup-<ver>.exe
; 安装后从开始菜单/桌面双击 DriveSync 即打开 GUI；GUI 内含首次运行向导与“打开即同步”。

#define MyAppName "drive-sync"
#define MyAppVersion "__VERSION__"
#define MyAppPublisher "go-enols"
#define MyAppURL "https://github.com/go-enols/drive-sync"
#define MyAppExeName "DriveSync.exe"

[Setup]
AppId={{B6E7B4A2-4B8C-4E1D-9D5A-0A0F4E2C9E71}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\drive-sync
DefaultGroupName=drive-sync
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\{#MyAppExeName}
OutputDir=dist
OutputBaseFilename=DriveSync-Setup-{#MyAppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
PrivilegesRequired=lowest
SourceDir=..\..
SetupIconFile=

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务："; Flags: unchecked
Name: "autostart"; Description: "登录 Windows 后自动启动 drive-sync"; GroupDescription: "附加任务："; Flags: checkedonce

[Files]
Source: "dist\package\DriveSync.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\package\drive-sync.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\package\drive-relay.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\package\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\package\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\package\DEPLOYMENT.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\drive-sync"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\卸载 drive-sync"; Filename: "{uninstallexe}"
Name: "{autodesktop}\drive-sync"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "DriveSyncGui"; ValueData: """{app}\{#MyAppExeName}"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "立即启动 drive-sync"; Flags: nowait postinstall skipifsilent
