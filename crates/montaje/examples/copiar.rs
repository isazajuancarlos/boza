// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Ejemplo/CLI headless: copia un árbol de Windows a una partición Linux montada
//! por cuchavira (RW efímero), con el motor de boza. Sirve de referencia de cómo
//! la GUI usará `boza-montaje`, y de banco e2e del backend.
//!
//! Uso:  copiar <label> <origen> [subdir_destino]
//! El binario de cuchavira se toma de la variable `CUCHAVIRA_EXE` si está, o del
//! PATH. La escritura la "confirma" este ejemplo por ser el llamante (en la GUI
//! real, el diálogo de boza es quien confirma antes de montar RW).

use boza_montaje::copiar_a_particion;
use boza_nucleo::{Deteccion, Opciones, Sobrescritura};
use cuchavira_cliente::Cuchavira;
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("uso: copiar <label> <origen> [subdir_destino]");
        std::process::exit(2);
    }
    let label = &args[1];
    let origen = Path::new(&args[2]);
    let subdir = args.get(3).map(Path::new);

    let mut cli = Cuchavira::nuevo("boza");
    if let Ok(exe) = std::env::var("CUCHAVIRA_EXE") {
        cli = cli.con_exe(exe);
    }

    let opts = Opciones {
        verificar: true,
        preservar_tiempo: true,
        preservar_permisos: true,
        sobrescribir: Sobrescritura::SiDistinto,
        deteccion: Deteccion::TamanoFecha,
        exclusiones: Vec::new(),
    };

    let mut ultima = String::new();
    let r = copiar_a_particion(&cli, label, origen, subdir, &opts, &mut |p| {
        // Progreso simple por fase (la GUI dibuja una barra; aquí una línea).
        let fase = format!("{:?}", p.fase);
        if fase != ultima {
            eprintln!("[{fase}] {}/{} archivos", p.archivos_hechos, p.archivos_totales);
            ultima = fase;
        }
    })?;
    println!(
        "copiados={} verificados={} omitidos={} excluidos={} bytes={} fallos={}",
        r.copiados,
        r.verificados,
        r.omitidos,
        r.excluidos,
        r.bytes,
        r.fallos.len()
    );
    if !r.fallos.is_empty() {
        for (ruta, motivo) in &r.fallos {
            eprintln!("FALLO {ruta:?}: {motivo}");
        }
        std::process::exit(1);
    }
    Ok(())
}
