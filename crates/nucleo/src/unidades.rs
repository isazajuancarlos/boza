// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Listar las unidades MONTADAS del sistema, para que la interfaz ofrezca elegir
//! entre ellas en vez de teclear una ruta. Se apoya en `lsblk -J`; el parseo es
//! puro y se prueba con una muestra fija (la ejecución de `lsblk` va aparte).

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;

/// Una unidad montada que el usuario puede elegir como origen o destino.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unidad {
    pub nombre: String,
    pub punto_montaje: PathBuf,
    pub tamano: String,
    pub sistema_archivos: Option<String>,
    pub etiqueta: Option<String>,
    /// Extraíble (USB, tarjeta…): lo típico al copiar "entre unidades".
    pub extraible: bool,
}

// --- Espejo de la parte de `lsblk -J` que nos interesa ---
#[derive(Deserialize)]
struct SalidaLsblk {
    blockdevices: Vec<Dispositivo>,
}

#[derive(Deserialize)]
struct Dispositivo {
    name: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    rm: bool,
    #[serde(default)]
    children: Vec<Dispositivo>,
}

/// Un punto de montaje "real" es una ruta absoluta: se excluyen `null` (sin
/// montar) y los pseudo como `[SWAP]`, que no son sitios donde copiar.
fn es_montaje_real(m: &Option<String>) -> Option<String> {
    match m {
        Some(s) if s.starts_with('/') => Some(s.clone()),
        _ => None,
    }
}

fn recolectar(disp: &Dispositivo, salida: &mut Vec<Unidad>) {
    if let Some(punto) = es_montaje_real(&disp.mountpoint) {
        salida.push(Unidad {
            nombre: disp.name.clone(),
            punto_montaje: PathBuf::from(punto),
            tamano: disp.size.clone().unwrap_or_default(),
            sistema_archivos: disp.fstype.clone(),
            etiqueta: disp.label.clone(),
            extraible: disp.rm,
        });
    }
    for hijo in &disp.children {
        recolectar(hijo, salida);
    }
}

/// Parsea la salida JSON de `lsblk -J` y devuelve las unidades montadas. Parte
/// pura: se prueba con una muestra fija, sin depender del disco de la máquina.
pub fn parsear_lsblk(json: &str) -> Result<Vec<Unidad>> {
    use anyhow::Context;
    let salida: SalidaLsblk =
        serde_json::from_str(json).context("no se pudo parsear la salida de lsblk")?;
    let mut unidades = Vec::new();
    for disp in &salida.blockdevices {
        recolectar(disp, &mut unidades);
    }
    Ok(unidades)
}

/// Ejecuta `lsblk -J` y devuelve las unidades montadas. La parte impura; la
/// lógica de qué cuenta como unidad vive en `parsear_lsblk`, que sí se prueba.
#[cfg(unix)]
pub fn unidades_montadas() -> Result<Vec<Unidad>> {
    use anyhow::Context;
    let salida = Command::new("lsblk")
        .args(["-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,LABEL,RM"])
        .output()
        .context("no se pudo ejecutar lsblk (¿está instalado?)")?;
    if !salida.status.success() {
        anyhow::bail!(
            "lsblk devolvió error: {}",
            String::from_utf8_lossy(&salida.stderr)
        );
    }
    parsear_lsblk(&String::from_utf8_lossy(&salida.stdout))
}

/// Enumera las unidades con letra en Windows (la parte impura). Recorre el mapa
/// de letras de `GetLogicalDrives` y por cada una consulta etiqueta y sistema de
/// archivos (`GetVolumeInformationW`), tamaño (`GetDiskFreeSpaceExW`) y si es
/// extraíble (`GetDriveTypeW` == `DRIVE_REMOVABLE`). Es el equivalente de `lsblk`.
#[cfg(windows)]
pub fn unidades_montadas() -> Result<Vec<Unidad>> {
    Ok(windows_impl::enumerar_unidades())
}

#[cfg(windows)]
mod windows_impl {
    use super::Unidad;
    use std::path::PathBuf;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };

    /// `GetDriveTypeW` == 2 → unidad extraíble (USB, tarjeta). El valor es estable
    /// en la API de Windows; se define aquí para no depender de dónde lo reexporte
    /// la binding.
    const DRIVE_REMOVABLE: u32 = 2;

    /// Convierte `"C:\"` a un buffer UTF-16 terminado en NUL para las API `*W`.
    fn a_utf16(raiz: &str) -> Vec<u16> {
        raiz.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Etiqueta de volumen y nombre del sistema de archivos de una raíz `X:\`.
    fn etiqueta_y_fs(raiz_w: &[u16]) -> (Option<String>, Option<String>) {
        let mut label = [0u16; 256];
        let mut fs = [0u16; 256];
        // SAFETY: buffers propios con su longitud correcta; punteros no nulos; la
        // API solo escribe hasta la longitud dada. Los `null`/0 son parámetros de
        // salida opcionales que no se piden.
        let ok = unsafe {
            GetVolumeInformationW(
                raiz_w.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            return (None, None); // sin medio (lector vacío), sin permiso, etc.
        }
        (Some(desde_utf16(&label)), Some(desde_utf16(&fs))).into_no_vacio()
    }

    /// Tamaño total de la unidad, formateado legible (p. ej. `57,3G`), o vacío.
    fn tamano(raiz_w: &[u16]) -> String {
        let mut total: u64 = 0;
        // SAFETY: puntero de salida válido; los otros dos son opcionales (null).
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                raiz_w.as_ptr(),
                std::ptr::null_mut(),
                &mut total,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || total == 0 {
            return String::new();
        }
        formatear_tamano(total)
    }

    fn formatear_tamano(bytes: u64) -> String {
        const U: [&str; 5] = ["B", "K", "M", "G", "T"];
        let mut v = bytes as f64;
        let mut i = 0;
        while v >= 1024.0 && i < U.len() - 1 {
            v /= 1024.0;
            i += 1;
        }
        format!("{v:.1}{}", U[i])
    }

    fn desde_utf16(buf: &[u16]) -> String {
        let fin = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..fin])
    }

    /// Enumera A:..Z: según el mapa de bits de `GetLogicalDrives`.
    pub fn enumerar_unidades() -> Vec<Unidad> {
        // SAFETY: sin argumentos; devuelve un bitmask.
        let mapa = unsafe { GetLogicalDrives() };
        let mut unidades = Vec::new();
        for i in 0..26u32 {
            if mapa & (1 << i) == 0 {
                continue;
            }
            let letra = (b'A' + i as u8) as char;
            let raiz = format!("{letra}:\\");
            let raiz_w = a_utf16(&raiz);
            // SAFETY: raíz terminada en NUL.
            let tipo = unsafe { GetDriveTypeW(raiz_w.as_ptr()) };
            let (etiqueta, fs) = etiqueta_y_fs(&raiz_w);
            unidades.push(Unidad {
                nombre: format!("{letra}:"),
                punto_montaje: PathBuf::from(&raiz),
                tamano: tamano(&raiz_w),
                sistema_archivos: fs,
                etiqueta,
                extraible: tipo == DRIVE_REMOVABLE,
            });
        }
        unidades
    }

    /// Pequeña ayuda: convierte cadenas vacías de la API en `None`.
    trait NoVacio {
        fn into_no_vacio(self) -> (Option<String>, Option<String>);
    }
    impl NoVacio for (Option<String>, Option<String>) {
        fn into_no_vacio(self) -> (Option<String>, Option<String>) {
            let limpiar = |s: Option<String>| s.filter(|x| !x.is_empty());
            (limpiar(self.0), limpiar(self.1))
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    // Muestra con la forma REAL de lsblk: un disco sin montar con tres
    // particiones (efi montada, raíz montada, swap), y un disco extraíble con una
    // partición montada. La aguja: se listan SOLO los montajes reales.
    const MUESTRA: &str = r#"{
      "blockdevices": [
        {"name":"sda","size":"119,2G","type":"disk","mountpoint":null,"fstype":null,"label":null,"rm":false,
         "children":[
           {"name":"sda1","size":"976M","type":"part","mountpoint":"/boot/efi","fstype":"vfat","label":null,"rm":false},
           {"name":"sda2","size":"112,1G","type":"part","mountpoint":"/","fstype":"ext4","label":"raiz","rm":false},
           {"name":"sda3","size":"6,2G","type":"part","mountpoint":"[SWAP]","fstype":"swap","label":null,"rm":false}
         ]},
        {"name":"sdb","size":"57,3G","type":"disk","mountpoint":null,"fstype":null,"label":null,"rm":true,
         "children":[
           {"name":"sdb1","size":"57,3G","type":"part","mountpoint":"/media/usb","fstype":"exfat","label":"USB","rm":true}
         ]}
      ]
    }"#;

    #[test]
    fn lista_solo_montajes_reales() {
        let u = parsear_lsblk(MUESTRA).unwrap();
        let puntos: Vec<_> = u
            .iter()
            .map(|x| x.punto_montaje.display().to_string())
            .collect();
        // efi, raíz y el USB: SÍ. El SWAP y los discos sin montar: NO.
        assert_eq!(puntos, vec!["/boot/efi", "/", "/media/usb"]);
        // el pseudo-montaje [SWAP] quedó fuera (pareja del de arriba)
        assert!(!puntos.iter().any(|p| p.contains("SWAP")));
    }

    #[test]
    fn marca_extraible_y_etiqueta() {
        let u = parsear_lsblk(MUESTRA).unwrap();
        let usb = u.iter().find(|x| x.nombre == "sdb1").unwrap();
        assert!(usb.extraible, "el USB es extraíble");
        assert_eq!(usb.etiqueta.as_deref(), Some("USB"));
        assert_eq!(usb.sistema_archivos.as_deref(), Some("exfat"));
        let raiz = u.iter().find(|x| x.nombre == "sda2").unwrap();
        assert!(!raiz.extraible, "el disco interno no es extraíble");
    }

    #[test]
    fn json_invalido_falla_no_entra_en_panico() {
        assert!(parsear_lsblk("esto no es json").is_err());
    }
}
