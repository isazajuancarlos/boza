// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Selector multi-origen: copiar archivos sueltos y/o carpetas a un destino.
//! Va en `tests/` (test de integración, solo la API pública) para que compile en
//! Windows — el módulo unit de `src/copia.rs` usa symlinks unix y no compila aquí.

use boza_nucleo::{copiar_items, copiar_un_archivo, Opciones, Progreso};
use std::fs;

fn nada(_: &Progreso) {}

#[test]
fn un_archivo_suelto_se_copia_y_verifica() {
    let o = tempfile::tempdir().unwrap();
    let d = tempfile::tempdir().unwrap();
    fs::write(o.path().join("doc.txt"), b"contenido suelto").unwrap();
    let dst = d.path().join("doc.txt");
    let r = copiar_un_archivo(&o.path().join("doc.txt"), &dst, &Opciones::default(), &mut nada).unwrap();
    assert_eq!(r.copiados, 1);
    assert_eq!(r.verificados, 1, "las opciones por defecto verifican por hash");
    assert!(r.fallos.is_empty(), "{:?}", r.fallos);
    assert_eq!(fs::read(&dst).unwrap(), b"contenido suelto");
}

#[test]
fn copiar_un_archivo_rechaza_un_directorio() {
    // D33 (caso rojo): un directorio NO es un archivo suelto.
    let o = tempfile::tempdir().unwrap();
    fs::create_dir(o.path().join("carpeta")).unwrap();
    let r = copiar_un_archivo(
        &o.path().join("carpeta"),
        &o.path().join("x"),
        &Opciones::default(),
        &mut nada,
    );
    assert!(r.is_err());
}

#[test]
fn copiar_items_mezcla_archivos_y_una_carpeta() {
    let o = tempfile::tempdir().unwrap();
    let d = tempfile::tempdir().unwrap();
    fs::write(o.path().join("a.txt"), b"AAA").unwrap();
    fs::write(o.path().join("b.txt"), b"BBBB").unwrap();
    fs::create_dir_all(o.path().join("carpeta/sub")).unwrap();
    fs::write(o.path().join("carpeta/x.txt"), b"X").unwrap();
    fs::write(o.path().join("carpeta/sub/y.txt"), b"Y").unwrap();
    let destino = d.path().join("backup");
    let items = vec![
        o.path().join("a.txt"),
        o.path().join("b.txt"),
        o.path().join("carpeta"),
    ];
    let r = copiar_items(&items, &destino, &Opciones::default(), &mut nada).unwrap();
    assert!(r.fallos.is_empty(), "{:?}", r.fallos);
    // cada archivo suelto, por su nombre, en la raíz del destino
    assert_eq!(fs::read(destino.join("a.txt")).unwrap(), b"AAA");
    assert_eq!(fs::read(destino.join("b.txt")).unwrap(), b"BBBB");
    // la carpeta recreada por su nombre, con su árbol
    assert!(destino.join("carpeta/x.txt").exists());
    assert!(destino.join("carpeta/sub/y.txt").exists());
    assert_eq!(r.copiados, 4, "2 sueltos + 2 dentro de la carpeta");
}

#[test]
fn copiar_items_un_item_que_falta_no_aborta_el_resto() {
    // D20: un item inexistente se registra como fallo; el resto se copia igual.
    let o = tempfile::tempdir().unwrap();
    let d = tempfile::tempdir().unwrap();
    fs::write(o.path().join("ok.txt"), b"ok").unwrap();
    let items = vec![o.path().join("ok.txt"), o.path().join("no-existe.txt")];
    let destino = d.path().join("dst");
    let r = copiar_items(&items, &destino, &Opciones::default(), &mut nada).unwrap();
    assert_eq!(r.copiados, 1, "el que existe sí se copió");
    assert!(!r.fallos.is_empty(), "el inexistente se registró como fallo");
    assert!(destino.join("ok.txt").exists());
}
