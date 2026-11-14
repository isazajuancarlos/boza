// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Orquesta el uso de discos Linux (ext4/btrfs/xfs) desde boza en Windows: los
//! monta por **cuchavira** (que los expone como una letra de unidad), copia con
//! **boza-nucleo** sobre esa letra, y los desmonta. El motor de copia no cambia:
//! una vez montado, el disco es una letra Windows como cualquier otra.
//!
//! **RW efímero por construcción** (la regla de oro para backups, D1): el montaje
//! es un [`Montaje`] con `Drop` — al salir de su scope se desmonta, también si la
//! copia entró en pánico o devolvió error. No depende de acordarse de desmontar;
//! lo garantiza el scope. La ventana de escritura dura lo que dura la copia.

use anyhow::Result;
use boza_nucleo::{Opciones, Progreso, Resultado, copiar_arbol};
use cuchavira_cliente::Cuchavira;
use std::path::{Path, PathBuf};

/// Cómo se monta el disco: RO no puede dañar; RW es para crear/actualizar backups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modo {
    SoloLectura,
    Escritura,
}

/// Un disco montado por cuchavira, vivo mientras exista este valor. Al soltarse
/// (fin del scope, o un error que corta la función) se desmonta: así el RW no
/// queda colgado aunque la copia falle. Para capturar un error de desmonte (que
/// con RW importa: es el `sync`), llama [`Montaje::desmontar`] explícitamente.
pub struct Montaje<'c> {
    cliente: &'c Cuchavira,
    label: String,
    letra: String,
    activo: bool,
}

impl<'c> Montaje<'c> {
    /// Monta la partición `label` (RO/RW según `modo`) y queda lista en una letra.
    /// En RW, cuchavira exige que la app haya confirmado (ver `cuchavira-cliente`).
    pub fn montar(cliente: &'c Cuchavira, label: &str, modo: Modo) -> Result<Self> {
        let letra = cliente.montar(label, modo == Modo::Escritura)?;
        Ok(Self {
            cliente,
            label: label.to_string(),
            letra,
            activo: true,
        })
    }

    /// Letra de unidad asignada, p. ej. `"Y:"`.
    #[must_use]
    pub fn letra(&self) -> &str {
        &self.letra
    }

    /// Raíz del disco como ruta Windows (`Y:\`).
    #[must_use]
    pub fn ruta(&self) -> PathBuf {
        destino_en(&self.letra, None)
    }

    /// Desmonta explícitamente (umount con sync + libera la letra) y propaga el
    /// error si lo hay. Consume el montaje: tras esto el `Drop` no vuelve a actuar.
    pub fn desmontar(mut self) -> Result<()> {
        self.activo = false;
        self.cliente.desmontar(&self.label)
    }
}

impl Drop for Montaje<'_> {
    fn drop(&mut self) {
        if self.activo {
            // Red del RW efímero: si no se desmontó explícito (un pánico o un `?`
            // a mitad de copia), desmontar best-effort para no dejar el disco RW
            // colgado. El error no se puede propagar desde Drop; el camino que SÍ
            // reporta es `desmontar()`.
            let _ = self.cliente.desmontar(&self.label);
        }
    }
}

/// Copia un árbol de Windows HACIA una partición Linux (crear/actualizar un
/// backup). Monta RW efímero, copia con el motor, y desmonta con sync. Si la copia
/// falla, desmonta igual (best-effort) y devuelve el error de la copia.
pub fn copiar_a_particion(
    cliente: &Cuchavira,
    label: &str,
    origen: &Path,
    subdir_destino: Option<&Path>,
    opts: &Opciones,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<Resultado> {
    let m = Montaje::montar(cliente, label, Modo::Escritura)?;
    let destino = destino_en(m.letra(), subdir_destino);
    match copiar_arbol(origen, &destino, opts, progreso) {
        Ok(res) => {
            // La copia fue bien: el desmonte (con sync) debe ir bien; su error SÍ
            // importa (datos sin sincronizar) y se propaga.
            m.desmontar()?;
            Ok(res)
        }
        Err(e) => {
            // La copia falló: desmontar best-effort y reportar el fallo de copia.
            let _ = m.desmontar();
            Err(e)
        }
    }
}

/// Copia DESDE una partición Linux hacia Windows (restaurar un backup). Monta
/// SoloLectura: restaurar nunca escribe en el backup.
pub fn copiar_desde_particion(
    cliente: &Cuchavira,
    label: &str,
    subdir_origen: Option<&Path>,
    destino: &Path,
    opts: &Opciones,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<Resultado> {
    let m = Montaje::montar(cliente, label, Modo::SoloLectura)?;
    let origen = destino_en(m.letra(), subdir_origen);
    let r = copiar_arbol(&origen, destino, opts, progreso);
    // Origen RO: el desmonte no sincroniza nada crítico, pero se reporta su error
    // solo si la copia fue bien (el fallo de copia manda).
    match r {
        Ok(res) => {
            m.desmontar()?;
            Ok(res)
        }
        Err(e) => {
            let _ = m.desmontar();
            Err(e)
        }
    }
}

/// Construye la ruta destino/origen dentro de una letra. Pura y testeable.
fn destino_en(letra: &str, subdir: Option<&Path>) -> PathBuf {
    let raiz = PathBuf::from(format!("{letra}\\"));
    match subdir {
        Some(s) => raiz.join(s),
        None => raiz,
    }
}

// El único test es solo-Windows (`destino_en` con rutas tipo `Y:\`): el módulo se
// gatea a windows para que en el verde de referencia (Linux) no quede un import sin
// usar. montaje es, además, un concern exclusivo de Windows (puente cuchavira).
#[cfg(all(test, windows))]
mod pruebas {
    use super::*;

    #[test]
    fn ruta_raiz_y_subdir() {
        assert_eq!(destino_en("Y:", None), PathBuf::from("Y:\\"));
        assert_eq!(
            destino_en("Y:", Some(Path::new("respaldos\\z"))),
            PathBuf::from("Y:\\respaldos\\z")
        );
    }
}
