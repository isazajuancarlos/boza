<!-- SPDX-License-Identifier: LicenseRef-Xiliux-Propietario -->
# boza

Aplicación de escritorio que clona/copia datos entre **dos unidades**, de forma
**rápida** y **segura**. Corre en **Linux** (frontal GTK4 nativo, `crates/gtk`) y
en **Windows** (frontal egui portable, `crates/egui`, un solo `.exe`); los dos
comparten el mismo motor (`crates/nucleo`).

- **Rápida**: elige el **perfil de copia** según el par de dispositivos —
  reflink (Copy-on-Write, `ioctl FICLONE`) si comparten sistema de archivos;
  secuencial con buffer grande si algún extremo es un disco mecánico; paralelo si
  ambos son de estado sólido. La durabilidad va por **lotes** (un `syncfs`, no un
  `fsync` por archivo), y en modo verificar el hash del origen se calcula **al
  vuelo** durante la copia — así iguala la velocidad de `rsync` sin renunciar a la
  copia fiel. Salto rápido incremental (tamaño+fecha) para re-copias, como rsync.
- **Segura**: verifica por hash (blake3) que cada archivo del destino es idéntico
  al del origen, **leyéndolo del PLATO** (`FADV_DONTNEED` descarta la caché para
  no comparar la RAM consigo misma; en Windows este descarte es no-op documentado
  y el hash sigue siendo correcto); **nunca borra ni toca el origen**; copia a
  temporal + `rename` atómico; rechaza copiar un árbol dentro de sí mismo;
  preserva permisos y fecha.
- **Sin basura**: por defecto poda cachés regenerables (`target`, `node_modules`,
  `.venv`, `__pycache__`, `.cache`) — no inflan el respaldo con lo que
  las herramientas reconstruyen. Se puede desactivar.
- **Progreso real, por proceso**: la barra va por BYTES (no por nº de archivos),
  avanza DENTRO de un archivo grande, y dice en qué fase está (midiendo /
  copiando / sincronizando / verificando) para que nunca parezca colgada.

`boza` es «dos» en la numeración muisca (dos unidades). Nombre provisional.

## Estructura

Un motor y **dos frontales, uno por plataforma** (ambos producen un binario `boza`):

- `crates/nucleo` — el motor de copia y verificación. Rust puro, sin UI; se prueba
  con directorios temporales. **Verde.** Incluye `boza-scrub` (detector de bit-rot
  por manifiesto; `sellar`/`verificar`, ambos **reanudables** con `--reanudar` para
  no reempezar de cero un barrido largo interrumpido) y `subcarpeta_destino` (regla
  «copiar la carpeta dentro del destino», compartida por los dos frontales y probada
  en ambas plataformas).
- `crates/gtk` — el frontal de **Linux** (GTK4 nativo, binario `boza`). Necesita
  `libgtk-4-dev`:

  ```
  sudo apt install libgtk-4-dev build-essential
  cargo build --release -p boza-gtk          # boza (GUI)
  cargo build --release -p boza-nucleo --bin boza-scrub   # boza-scrub
  ```

- `crates/egui` — el frontal de **Windows** (eframe + tema `huitaca`, portable, un
  solo `.exe` sin runtime de sistema). Además monta discos Linux (ext4/btrfs/xfs)
  por **cuchavira** (`crates/montaje`) para hacer backups cruzados. En Linux no se
  usa (los discos ya están montados); se excluye del empaquetado `.deb`.
- `crates/montaje` — el pegamento con cuchavira (solo lo usa el frontal de Windows).
- `crates/nucleo/src/plataforma.rs` — la capa por plataforma (durabilidad, lectura
  de plato, identidad de archivo, enlaces, listar unidades). Linux queda idéntico;
  Windows usa el equivalente (`FlushFileBuffers`, Win32).

## Empaquetar

- **Debian (.deb)** — desde Linux. Construye ANTES los dos binarios (el `.deb` lleva
  `boza` de `boza-gtk` y `boza-scrub` de `boza-nucleo`) y empaqueta sin recompilar:

  ```
  cargo build --release -p boza-gtk -p boza-nucleo --bin boza-scrub --bin boza
  cargo deb -p boza-gtk --no-build          # → target/debian/boza_*.deb
  ```

  Trae `boza` + `boza-scrub`, icono y entrada de menú; depende del GTK4 del sistema.
- **openSUSE / Fedora (.rpm)** — desde Linux, gemelo del `.deb`. Mismos dos binarios;
  se corre **desde `crates/gtk/`** (las rutas de los assets son relativas a su
  `Cargo.toml`), con `cargo-generate-rpm` (`cargo install cargo-generate-rpm`):

  ```
  cargo build --release -p boza-gtk -p boza-nucleo --bin boza-scrub --bin boza
  (cd crates/gtk && cargo rpm-firmado)   # = generate-rpm + firma GPG + rpm -K
  #  → crates/gtk/target/generate-rpm/boza-<ver>-1.x86_64.rpm (el ./target de crates/gtk)
  sudo zypper install ./crates/gtk/target/generate-rpm/boza-*.x86_64.rpm   # o dnf en Fedora
  ```

  El `.rpm` auto-detecta sus dependencias (`libgtk-4.so.1`, `libglib-2.0`, `libnettle`…)
  del ELF, y `zypper`/`dnf install ./…` las resuelve; `rpm -Uvh` NO. **Esas sonames son
  de la DISTRO donde se construyó**: el 0.1.0 hecho en Fedora 44 pedía
  `libhogweed.so.6` (nettle 3) y no instala en Tumbleweed (`libhogweed.so.7`, nettle 4)
  — se reconstruye en la distro de destino, no se copia. Sale **firmado** con la clave de
  paquetes Xiliux (`182A…D65E`, `conocimiento/rpm-firmado`); cada máquina la importa una
  vez con `sudo rpm --import xiliux-paquetes.asc`. Verificado en Tumbleweed el
  2026-11-06 (boza 0.1.1, `Signature, key ID d50ed65e: OK`).
- **Windows (.exe)** — se construye en Windows: `cargo build --release -p boza-egui`
  da un `.exe` portable (sin runtime que empaquetar). El instalador va con Inno Setup
  (`dist/windows/boza.iss`); junto al `.exe` se despliega `cuchavira.exe` si se quiere
  el montaje de discos Linux.

## Origen

Protocolo solo-local: `origin` = `/home/juan/espejos/boza.git`. No se publica.
