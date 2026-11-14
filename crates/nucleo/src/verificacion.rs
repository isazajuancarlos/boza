// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Verificación: ¿el destino es de verdad una copia fiel del origen? Se compara
//! por HASH (blake3), no por fecha ni tamaño — un archivo del mismo tamaño con un
//! byte cambiado tiene que salir como diferencia, y eso solo lo ve el contenido.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Una discrepancia entre origen y destino. Cada variante nombra la falla: un
/// "no coincide" a secas no dejaría al usuario saber qué revisar.
#[derive(Debug, PartialEq, Eq)]
pub enum Diferencia {
    /// El archivo existe en el origen pero no en el destino.
    Falta(PathBuf),
    /// Existe en ambos pero el contenido difiere (hash distinto).
    Contenido(PathBuf),
    /// Existe en ambos pero uno es archivo y el otro no (p. ej. directorio).
    Tipo(PathBuf),
    /// No se pudo leer para comparar (se reporta, no se silencia — directiva 20).
    Ilegible(PathBuf, String),
}

/// Hash blake3 del contenido de un archivo, leyendo por trozos (no carga el
/// archivo entero en memoria).
pub fn hash_archivo(ruta: &Path) -> io::Result<blake3::Hash> {
    let mut f = File::open(ruta)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Compara el árbol de `origen` contra `destino` y devuelve TODAS las diferencias
/// (lista vacía = copia fiel). Recorre el origen: comprueba que cada archivo suyo
/// esté en el destino y con el mismo contenido. No mira lo que sobre en destino.
pub fn verificar_arbol(origen: &Path, destino: &Path) -> Vec<Diferencia> {
    verificar_arbol_con(origen, destino, &[])
}

/// Igual que [`verificar_arbol`] pero podando los mismos `exclusiones` que usó la
/// copia: si no, los subárboles excluidos (p. ej. `target/`) se reportarían como
/// `Falta` en el destino, un falso positivo.
pub fn verificar_arbol_con(
    origen: &Path,
    destino: &Path,
    exclusiones: &[String],
) -> Vec<Diferencia> {
    let mut difs = Vec::new();
    for entrada in crate::copia::recorrer(origen, exclusiones) {
        let entrada = match entrada {
            Ok(e) => e,
            Err(e) => {
                difs.push(Diferencia::Ilegible(
                    e.path().unwrap_or(origen).to_path_buf(),
                    e.to_string(),
                ));
                continue;
            }
        };
        let rel = match entrada.path().strip_prefix(origen) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue; // la raíz misma
        }
        let tipo = entrada.file_type();
        if tipo.is_dir() {
            continue; // los directorios los cubre la presencia de sus archivos
        }
        let destino_ruta = destino.join(rel);
        if !destino_ruta.exists() {
            difs.push(Diferencia::Falta(rel.to_path_buf()));
            continue;
        }
        if tipo.is_file() {
            if !destino_ruta.is_file() {
                difs.push(Diferencia::Tipo(rel.to_path_buf()));
                continue;
            }
            match (hash_archivo(entrada.path()), hash_archivo(&destino_ruta)) {
                (Ok(a), Ok(b)) if a == b => {}
                (Ok(_), Ok(_)) => difs.push(Diferencia::Contenido(rel.to_path_buf())),
                (Err(e), _) | (_, Err(e)) => {
                    difs.push(Diferencia::Ilegible(rel.to_path_buf(), e.to_string()))
                }
            }
        }
        // enlaces simbólicos: su fidelidad la comprueba la copia al recrearlos;
        // aquí no se siguen (follow_links(false)) para no salir del árbol.
    }
    difs
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use std::fs;

    fn arbol(dir: &Path) {
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"contenido A").unwrap();
        fs::write(dir.join("sub/b.txt"), b"contenido B mas largo").unwrap();
    }

    // AGUJA: dos árboles idénticos no tienen diferencias.
    #[test]
    fn arboles_identicos_no_difieren() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        arbol(d.path());
        assert_eq!(verificar_arbol(o.path(), d.path()), vec![]);
    }

    // PAREJA roja 1 — un byte cambiado (mismo tamaño): se caza por contenido, que
    // es la razón de hashear en vez de comparar tamaño/fecha.
    #[test]
    fn un_byte_distinto_se_caza_por_contenido() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        arbol(d.path());
        fs::write(d.path().join("a.txt"), b"contenido X").unwrap(); // mismo largo, 1 byte distinto
        assert_eq!(
            verificar_arbol(o.path(), d.path()),
            vec![Diferencia::Contenido(PathBuf::from("a.txt"))]
        );
    }

    // PAREJA roja 2 — falta un archivo en el destino.
    #[test]
    fn archivo_ausente_se_reporta() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        arbol(d.path());
        fs::remove_file(d.path().join("sub/b.txt")).unwrap();
        assert_eq!(
            verificar_arbol(o.path(), d.path()),
            vec![Diferencia::Falta(PathBuf::from("sub/b.txt"))]
        );
    }
}
