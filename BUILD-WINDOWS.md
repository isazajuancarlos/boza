# Construir boza para Windows (.exe + instalador)

> El **motor** (`boza-nucleo` + `boza-scrub`) ya compila para Windows —
> verificado con `cargo check --target x86_64-pc-windows-gnu` en Linux—. Lo que
> NO se puede hacer desde Linux es construir la **GUI GTK4** ni el instalador:
> GTK4 arrastra un stack de librerías C de Windows que solo está empaquetado
> **en Windows** (MSYS2). Estos pasos se corren en la máquina Windows.

## Qué cambió para que funcione en Windows

El núcleo usa una **capa por plataforma** (`crates/nucleo/src/plataforma.rs`).
Linux queda EXACTAMENTE igual; Windows usa el equivalente honesto:

| Pieza | Linux | Windows |
|---|---|---|
| Durabilidad | `syncfs` por LOTE | `FlushFileBuffers` (`sync_all`) por ARCHIVO, antes del `rename` |
| Leer del plato | `posix_fadvise(FADV_DONTNEED)` | no-op **documentado** (el hash sigue siendo correcto; pierde la garantía de saltarse la caché) |
| Mismo archivo | `st_dev` + `st_ino` | `GetFileInformationByHandle` (serie de volumen + índice) |
| Enlaces | `symlink` | `symlink_file`/`symlink_dir` (exige Modo desarrollador o admin; si falla, se registra, no aborta) |
| Perfil de copia | `/sys/.../rotational` | **Secuencial** (lado seguro; el reflink igual se intenta y cae a copia normal si no es ReFS) |
| Listar unidades | `lsblk -J` | `GetLogicalDrives` + `GetDriveTypeW` + `GetVolumeInformationW` |

**Nota de durabilidad**: en Windows la durabilidad del RENAME en sí (metadatos
del directorio) no se fuerza — no hay equivalente sencillo sin handle de volumen
con privilegios. Se fuerzan los DATOS de cada archivo (`FlushFileBuffers`), que
es lo importante para una copia entre unidades. Si algún día se quiere durabilidad
transaccional del rename en Windows, es trabajo aparte.

## Opción A — GUI NATIVA de Windows (`crates/win`, RECOMENDADA)

Desde 2026-09-18 hay una segunda cáscara, **`crates/win`**, con GUI **nativa Win32**
(`native-windows-gui`) sobre el mismo `boza-nucleo`. **No usa GTK ni MSYS2**: un
`.exe` autocontenido. Es la vía más simple para Windows.

Requisitos en la VM (todo con scoop, sin Visual Studio):
```
scoop install rustup gcc          # toolchain gnu + linker MinGW
rustup default stable-x86_64-pc-windows-gnu
```
Verificado que compila para `x86_64-pc-windows-gnu` (lib + GUI). Construir:
```
# .exe autocontenido, sin DLLs de MinGW colgando:
set RUSTFLAGS=-C target-feature=+crt-static      # (cmd)   —o en PowerShell:
$env:RUSTFLAGS="-C target-feature=+crt-static"
cargo build --release -p boza-win
# binario: target/release/boza.exe  (GUI nativa)
```
La GUI nativa dibuja la misma pantalla que la de GTK (elegir dos unidades,
opciones, barra por fase, parte final). En Linux este crate compila a un binario
vacío (por el `cfg(windows)`), así que no estorba al build del host ni al `.deb`.

`boza-scrub` (la herramienta CLI) sale del mismo build: `cargo build --release -p boza-nucleo --bins`.

> **Opción B — GUI GTK4 en Windows** (abajo): solo si quieres la MISMA pantalla
> GTK que en Linux. Necesita MSYS2 + el stack GTK + empaquetar ~30 DLLs. Más
> pesado; documentado por completitud.

## 1. Entorno de compilación (MSYS2 — para la Opción B / GTK)

En Windows, instalar [MSYS2](https://www.msys2.org/) y abrir la terminal
**MINGW64** (no la MSYS ni la UCRT64: el target `x86_64-pc-windows-gnu` de Rust
enlaza contra el runtime MINGW64):

```bash
pacman -Syu                       # y reabrir la terminal si lo pide
pacman -S --needed \
    mingw-w64-x86_64-gtk4 \
    mingw-w64-x86_64-pkgconf \
    mingw-w64-x86_64-gcc \
    mingw-w64-x86_64-rust          # o instalar rustup aparte (ver abajo)
```

Si se prefiere el `rustup` oficial en vez del rust de MSYS2:

```bash
# en la terminal MINGW64, con rustup ya instalado en Windows:
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu
export PATH="/mingw64/bin:$PATH"   # que pkgconf y las libs de GTK estén delante
```

Comprobar que pkgconf ve GTK4:

```bash
pkg-config --modversion gtk4       # debe imprimir 4.xx
```

## 2. Compilar

Desde la raíz del repo, en la terminal **MINGW64**. Antes de compilar la GUI hay
que añadir el crate `gtk` a los miembros del workspace (está fuera hasta tener
GTK): editar `Cargo.toml` raíz y dejar
`members = ["crates/nucleo", "crates/gtk"]` (ya está así).

```bash
cargo build --release --target x86_64-pc-windows-gnu
# binarios en: target/x86_64-pc-windows-gnu/release/boza.exe y boza-scrub.exe
```

`boza.exe` NO arranca solo: necesita las DLL de GTK a su lado (paso 3).

## 3. Empaquetar el runtime GTK junto al .exe

GTK4 en Windows es un montón de DLL + esquemas + cargadores de imágenes + iconos.
El script `dist/windows/empaquetar.sh` (terminal MINGW64) copia todo a una carpeta
`dist/windows/boza-portable/` autocontenida:

```bash
bash dist/windows/empaquetar.sh
```

Hace, en resumen (está comentado):
- copia `boza.exe`, `boza-scrub.exe`;
- `ldd boza.exe | grep /mingw64` → copia cada DLL de GTK/GLib/Cairo/Pango…;
- copia `lib/gdk-pixbuf-2.0/` (cargadores PNG/SVG), `share/glib-2.0/schemas/`
  (compilados con `glib-compile-schemas`), y el tema de iconos
  `share/icons/Adwaita` + `hicolor` (más `boza.svg`);
- deja un `boza.exe` que arranca con doble clic desde esa carpeta.

Verificar arrancándolo con doble clic. Si se queja de un icono o esquema, falta
una carpeta de `share/` — el script lista lo que copió.

## 4. Instalador

### Opción A — instalador .exe con Inno Setup (recomendada, la más simple)

[Inno Setup](https://jrsoftware.org/isdl.php) empaqueta la carpeta portable en un
único `boza-instalador.exe`. Está la plantilla `dist/windows/boza.iss`: abrir con
Inno Setup y **Compile**, o por línea de comandos:

```bat
iscc dist\windows\boza.iss
```

Produce `dist\windows\salida\boza-instalador-0.1.0.exe`: instala en
`Archivos de programa`, crea acceso en el menú Inicio y desinstalador.

### Opción B — .msi con WiX (si se exige MSI)

`cargo install cargo-wix` y WiX Toolset 3.x. Requiere listar en el `.wxs` todas
las DLL de GTK (Inno lo hace con un `Source: ...\*` recursivo, más cómodo). Solo
merece la pena si el destino exige MSI (despliegue por directiva de grupo, etc.).

## Lo que falta VERIFICAR en Windows (honestidad)

- Que la GUI **dibuje** (directiva 28): abrir `boza.exe`, elegir dos carpetas,
  copiar y ver la barra por fases. El núcleo está probado; la pantalla GTK en
  Windows no se ha podido ejecutar desde aquí.
- Que un archivo con symlink se copie o falle **con aviso** (privilegio de
  symlink en Windows).
- Idealmente, pasar el `concilio` al diff del port antes de dar la versión
  Windows por cerrada.
