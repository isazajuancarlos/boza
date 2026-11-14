// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Capa por plataforma: las operaciones de bajo nivel que difieren entre Linux
//! y Windows. El resto del motor es idéntico en las dos.
//!
//! **La diferencia de fondo es el modelo de DURABILIDAD.** En Linux boza usa
//! `syncfs`: un solo `sync` por LOTE hace durable todo lo renombrado, que es lo
//! que iguala a `rsync` en árboles de muchos archivos. Windows no tiene `syncfs`
//! (el equivalente por volumen exige privilegios), así que ahí la durabilidad va
//! por ARCHIVO: `FlushFileBuffers` (vía [`std::fs::File::sync_all`]) sobre cada
//! temporal ANTES del `rename`. Es más lento por archivo pero igual de fiel; el
//! `sync` por lote pasa a ser no-op en Windows.
//!
//! La otra diferencia es la lectura DEL PLATO. En Linux se descarta la caché con
//! `posix_fadvise(FADV_DONTNEED)` para que el hash de verificación no compare la
//! RAM consigo misma. Windows no tiene un equivalente sin buffering alineado, así
//! que ahí es un no-op DOCUMENTADO: la verificación sigue siendo por hash y
//! correcta, pero pierde la garantía de leer sin pasar por caché (la degradación
//! se dice, no se calla — directiva 20/30).

use anyhow::Result;
use std::fs::File;
use std::io;
use std::path::Path;

/// Sincronizador del destino. Encapsula el modelo de durabilidad por lotes: en
/// Linux guarda un descriptor del directorio destino para `syncfs`; en Windows no
/// guarda nada, porque ahí la durabilidad la asegura [`durabilizar_ruta`] por
/// archivo y el `sync` de lote es no-op.
pub struct Sincronizador {
    #[cfg(unix)]
    dir: File,
}

impl Sincronizador {
    /// Abre lo que haga falta para poder sincronizar `destino` (un directorio).
    ///
    /// En Windows NO se abre el directorio: `File::open` sobre una carpeta falla
    /// sin `FILE_FLAG_BACKUP_SEMANTICS`, y no hace falta porque la durabilidad va
    /// por archivo.
    pub fn nuevo(destino: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use anyhow::Context;
            let dir = File::open(destino).with_context(|| {
                format!("no se pudo abrir el destino {destino:?} para sincronizar")
            })?;
            Ok(Self { dir })
        }
        #[cfg(not(unix))]
        {
            let _ = destino;
            Ok(Self {})
        }
    }

    /// Hace durable, de una vez, todo lo escrito en el sistema de archivos del
    /// destino. En Linux es `syncfs` (un `-1` es un fallo de durabilidad REAL y se
    /// propaga). En Windows es no-op: cada archivo ya se hizo durable en
    /// [`durabilizar_ruta`] antes de su `rename`.
    pub fn sincronizar(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use anyhow::Context;
            use std::os::unix::io::AsRawFd;
            // SAFETY: `syncfs` solo recibe un descriptor válido y vivo (`self.dir`);
            // no toca memoria de Rust.
            let r = unsafe { libc::syncfs(self.dir.as_raw_fd()) };
            if r != 0 {
                return Err(std::io::Error::last_os_error()).context("syncfs falló");
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }
}

/// Hace durable en disco el archivo `p` (recién escrito), ANTES de renombrarlo.
///
/// En Linux es no-op: la durabilidad va por LOTE (`Sincronizador::sincronizar`).
/// En Windows abre el archivo y hace `sync_all` (`FlushFileBuffers`), porque no
/// hay `syncfs`: cada temporal se hace durable antes de publicar su nombre.
pub fn durabilizar_ruta(p: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = p;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // `sync_all` sobre un handle de escritura hace `FlushFileBuffers` de ese
        // archivo (los datos que este proceso escribió en el temporal).
        std::fs::OpenOptions::new().write(true).open(p)?.sync_all()
    }
}

/// Descarta la caché de páginas del archivo `ruta` (mejor esfuerzo) para que un
/// hash posterior lea DEL PLATO y no compare la RAM consigo misma.
///
/// En Linux es `posix_fadvise(FADV_DONTNEED)`. En Windows es no-op documentado:
/// la lectura puede venir de caché (el hash sigue siendo correcto, solo pierde la
/// garantía de "desde el plato").
pub fn descartar_cache(ruta: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if let Ok(f) = File::open(ruta) {
            // SAFETY: descriptor válido; offset/len 0 = archivo entero; consejo puro.
            unsafe {
                libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ruta;
    }
}

/// ¿`a` y `b` son EL MISMO archivo en disco? Caza el bind mount / doble montaje
/// (Linux) o el mismo archivo por dos rutas (Windows) que `canonicalize` no ve.
///
/// Linux: mismo `st_dev` + `st_ino`. Windows: mismo número de serie de volumen +
/// índice de archivo (`nFileIndex`), el equivalente exacto.
pub fn mismo_archivo(a: &Path, b: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
        Ok(ma.dev() == mb.dev() && ma.ino() == mb.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        // Identidad de archivo por número de serie de volumen + índice de archivo
        // (`nFileIndexHigh:Low`) — el equivalente exacto de (st_dev, st_ino) en
        // Windows, por la API ESTABLE (la de `std` está tras un feature inestable).
        fn info(p: &Path) -> io::Result<(u32, u64)> {
            let f = File::open(p)?;
            // SAFETY: se rellena una estructura POD; el handle es válido y vivo
            // mientras dure `f`.
            let mut fi: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as HANDLE, &mut fi) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let index = ((fi.nFileIndexHigh as u64) << 32) | fi.nFileIndexLow as u64;
            Ok((fi.dwVolumeSerialNumber, index))
        }
        let (va, ia) = info(a)?;
        let (vb, ib) = info(b)?;
        Ok(va == vb && ia == ib)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a, b);
        Ok(false)
    }
}

/// Recrea un enlace simbólico `enlace` que apunta a `objetivo`.
///
/// Linux: `symlink` (sirve para archivo y directorio). Windows: hay que elegir
/// entre `symlink_file` y `symlink_dir`; se decide resolviendo `objetivo`
/// relativo al directorio del enlace (si apunta a un directorio existente →
/// `symlink_dir`, si no → `symlink_file`). En Windows crear symlinks exige
/// privilegio (Modo desarrollador o admin); si falla, el llamante lo registra
/// como fallo del archivo (no aborta la copia).
pub fn crear_symlink(objetivo: &Path, enlace: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(objetivo, enlace)
    }
    #[cfg(windows)]
    {
        // Resolver el objetivo relativo al directorio del enlace para saber si es
        // directorio. Si no se puede determinar, se asume archivo.
        let base = enlace.parent().unwrap_or_else(|| Path::new("."));
        let resuelto = if objetivo.is_absolute() {
            objetivo.to_path_buf()
        } else {
            base.join(objetivo)
        };
        if resuelto.is_dir() {
            std::os::windows::fs::symlink_dir(objetivo, enlace)
        } else {
            std::os::windows::fs::symlink_file(objetivo, enlace)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (objetivo, enlace);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "enlaces simbólicos no soportados en esta plataforma",
        ))
    }
}
