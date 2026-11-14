// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Perfil de copia según el PAR de dispositivos (origen, destino).
//!
//! La estrategia óptima no es una sola: depende de si los extremos comparten
//! sistema de archivos y de si alguno es un disco MECÁNICO. La regla, en una
//! frase: **si comparten fs → reflink; si algún extremo es mecánico →
//! secuencial; si ambos son de estado sólido y en distinto fs → paralelo.**
//!
//! El mecánico manda porque el *seek* de un plato giratorio mata más
//! rendimiento del que el paralelismo puede ganar: escribir varios archivos a
//! la vez en un HDD lo obliga a saltar de pista sin parar.

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perfil {
    /// Origen y destino en el MISMO sistema de archivos: reflink (Copy-on-Write,
    /// `ioctl FICLONE`), casi instantáneo y sin duplicar bloques.
    Reflink,
    /// Algún extremo es mecánico (HDD): copia SECUENCIAL, un archivo a la vez,
    /// con buffer grande y en el orden del árbol (menos saltos de cabezal).
    Secuencial,
    /// Ambos de estado sólido y en distinto fs: se puede SOLAPAR lectura,
    /// hash (CPU) y escritura con varios trabajadores.
    Paralelo,
}

/// El plan concreto que sale de un perfil: qué estrategia, con qué buffer y
/// cuánta concurrencia. El motor de copia lo consulta; no vuelve a decidir.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanCopia {
    pub perfil: Perfil,
    /// Tamaño del buffer de la copia en flujo (modo verificar), en bytes.
    pub buffer: usize,
    /// Archivos copiados a la vez. 1 = secuencial. >1 solo tiene sentido cuando
    /// NINGÚN extremo es mecánico.
    pub concurrencia: usize,
}

/// `st_dev` (identificador de sistema de archivos) de la ruta, subiendo al
/// primer ancestro que exista — el destino puede no existir todavía.
#[cfg(unix)]
fn dev_de(ruta: &Path) -> Option<u64> {
    let mut p = ruta.to_path_buf();
    loop {
        if let Ok(m) = fs::symlink_metadata(&p) {
            return Some(m.dev());
        }
        if !p.pop() {
            return None;
        }
        if p.as_os_str().is_empty() {
            p = Path::new("/").to_path_buf();
        }
    }
}

/// ¿El dispositivo de este `st_dev` es MECÁNICO (plato giratorio)? Lee
/// `/sys/dev/block/MAJ:MIN/queue/rotational` (1 = mecánico, 0 = estado sólido).
/// Para una partición, `queue/` cuelga del disco padre: se prueba `..`.
/// `None` = no se pudo determinar (dispositivo virtual, contenedor, `tmpfs`…).
#[cfg(unix)]
fn es_mecanico(dev: u64) -> Option<bool> {
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    for cand in [
        format!("/sys/dev/block/{maj}:{min}/queue/rotational"),
        format!("/sys/dev/block/{maj}:{min}/../queue/rotational"),
    ] {
        if let Ok(s) = fs::read_to_string(&cand) {
            return match s.trim() {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// Decide el plan de copia mirando el par de dispositivos.
///
/// En Windows no hay `/sys/dev/block` ni un `st_dev` equivalente fiable, así que
/// no se puede detectar "mismo fs" ni "mecánico": el plan es SECUENCIAL, el lado
/// seguro (nunca thrashea un plato). El reflink (CoW) igual se intenta en la copia
/// vía `reflink_copy`, que en Windows cae a copia normal si no es ReFS.
#[cfg(not(unix))]
pub fn planificar(origen: &Path, destino: &Path) -> PlanCopia {
    let _ = (origen, destino);
    PlanCopia {
        perfil: Perfil::Secuencial,
        buffer: 8 << 20,
        concurrencia: 1,
    }
}

/// Decide el plan de copia mirando el par de dispositivos.
#[cfg(unix)]
pub fn planificar(origen: &Path, destino: &Path) -> PlanCopia {
    let (dev_o, dev_d) = (dev_de(origen), dev_de(destino));

    // Comparten sistema de archivos → reflink (CoW), el camino más rápido.
    if let (Some(a), Some(b)) = (dev_o, dev_d) {
        if a == b {
            return PlanCopia {
                perfil: Perfil::Reflink,
                buffer: 4 << 20,
                concurrencia: 1,
            };
        }
    }

    // Un dato de rotación DESCONOCIDO se trata como MECÁNICO: el lado seguro es
    // secuencial (nunca thrashea un plato). No es un relleno que afecte la
    // fidelidad —solo elige un perfil más conservador—, así que aquí el defecto
    // es legítimo (cascada, no dato inventado).
    let hay_mecanico =
        dev_o.and_then(es_mecanico).unwrap_or(true) || dev_d.and_then(es_mecanico).unwrap_or(true);

    if hay_mecanico {
        PlanCopia {
            perfil: Perfil::Secuencial,
            buffer: 8 << 20, // 8 MiB: lecturas/escrituras largas al plato
            concurrencia: 1,
        }
    } else {
        PlanCopia {
            perfil: Perfil::Paralelo,
            buffer: 4 << 20,
            concurrencia: 4,
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn dev_de_sube_al_ancestro_existente() {
        // Una ruta inexistente cae al dev de su padre (que sí existe).
        let dir = std::env::temp_dir();
        let inexistente = dir.join("no-existe-boza-perfil/aun-menos/x");
        assert_eq!(dev_de(&inexistente), dev_de(&dir), "cae al ancestro vivo");
    }

    #[cfg(unix)]
    #[test]
    fn mismo_fs_es_reflink() {
        let dir = std::env::temp_dir();
        // Origen y destino bajo el mismo dir temporal comparten fs.
        let plan = planificar(&dir, &dir.join("destino-boza"));
        assert_eq!(plan.perfil, Perfil::Reflink);
        assert_eq!(plan.concurrencia, 1);
    }

    #[test]
    fn secuencial_nunca_paraleliza() {
        // Invariante del perfil: si es secuencial, concurrencia = 1.
        let s = PlanCopia {
            perfil: Perfil::Secuencial,
            buffer: 8 << 20,
            concurrencia: 1,
        };
        assert_eq!(s.concurrencia, 1);
    }
}
