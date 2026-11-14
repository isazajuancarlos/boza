; SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
; Instalador de boza para Windows (Inno Setup 6).
; Empaqueta la carpeta autocontenida dist/windows/boza-portable/ (creada por
; empaquetar.sh) en un unico boza-instalador-<version>.exe.
;
; Compilar:  iscc dist\windows\boza.iss     (o abrir en Inno Setup y Build)

#define MiNombre "Boza"
#define MiVersion "0.1.0"
#define MiEditor "Isaza Arenas"
#define MiExe "boza.exe"

[Setup]
AppId={{B0ZA0001-0000-0000-0000-000000000001}
AppName={#MiNombre}
AppVersion={#MiVersion}
AppPublisher={#MiEditor}
DefaultDirName={autopf}\{#MiNombre}
DefaultGroupName={#MiNombre}
UninstallDisplayIcon={app}\{#MiExe}
Compression=lzma2
SolidCompression=yes
OutputDir=salida
OutputBaseFilename=boza-instalador-{#MiVersion}
; Instala para el usuario actual sin pedir admin; cambiar a "admin" para todos.
PrivilegesRequired=lowest
WizardStyle=modern

[Languages]
Name: "es"; MessagesFile: "compiler:Languages\Spanish.isl"

[Files]
; Toda la carpeta portable (boza.exe + DLL de GTK + share/ + lib/).
Source: "boza-portable\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{group}\{#MiNombre}"; Filename: "{app}\{#MiExe}"
Name: "{group}\Desinstalar {#MiNombre}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MiNombre}"; Filename: "{app}\{#MiExe}"; Tasks: escritorio

[Tasks]
Name: "escritorio"; Description: "Crear un acceso en el escritorio"; GroupDescription: "Accesos:"

[Run]
Filename: "{app}\{#MiExe}"; Description: "Abrir {#MiNombre}"; Flags: nowait postinstall skipifsilent
