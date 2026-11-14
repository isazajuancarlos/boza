// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! `boza-scrub` — detector de *bit-rot* para un árbol de respaldo (p. ej. el HDD
//! `/mnt/datos`). ext4 no verifica los DATOS por checksum, así que una pudrición
//! silenciosa en el plato no se nota hasta que lees el archivo y ya es tarde.
//!
//! Modelo de MANIFIESTO (no comparar contra el origen vivo): se SELLA el estado
//! actual del respaldo (blake3 de cada archivo) y más tarde se VERIFICA
//! re-hasheando **desde el plato** (`FADV_DONTNEED` descarta la caché para no
//! comparar la RAM consigo misma) contra ese sello. Así una discrepancia es
//! corrupción del disco, INDEPENDIENTE de que el origen haya cambiado.
//!
//!   boza-scrub sellar    <dir> <manifiesto> [--reanudar]
//!   boza-scrub verificar <dir> <manifiesto> [--reanudar]
//!
//! **Reanudable** (`--reanudar`): hashear del plato un árbol de respaldo grande
//! tarda horas; un barrido cancelado o cuyo proceso muere no debe reempezar de
//! cero. El checkpoint se persiste POR LOTES (ante una muerte se pierde a lo sumo
//! el último lote, que se re-hashea al reanudar):
//!   - al SELLAR, el propio manifiesto parcial ES el checkpoint: `--reanudar`
//!     retoma lo ya sellado y continúa. Sin el flag, sobrescribe (hoy).
//!   - al VERIFICAR, un `<manif>.verif-progreso` efímero guarda el veredicto de
//!     cada archivo ya comprobado; `--reanudar` lo retoma. El progreso se BORRA al
//!     terminar el barrido (éxito o rojo): solo sobrevive si el proceso muere.
//!
//! **El detector no se ciega** (puerta de seguridad, 2026-10-31): un checkpoint es
//! un artefacto en disco que un barrido anterior pudo dejar a medias, y un árbol de
//! respaldo puede contener nombres hostiles. Por eso (a) al retomar, solo se confía
//! en registros BIEN FORMADOS (hash de 64 hex / estado ∈ ok|rot|nuevo; un estado
//! desconocido NO es «limpio», se re-verifica — D20) y el checkpoint se reescribe
//! limpio antes de seguir (una línea torcida no contamina por `append`); (b) un
//! nombre con `\n`/`\t`/`\r` no se puede representar en el formato por línea y se
//! REPORTA como problema en vez de romper el archivo o inyectar un registro falso;
//! (c) el `.verif-progreso` se crea sin seguir symlinks (`O_NOFOLLOW`), que si no se
//! truncaría un archivo arbitrario apuntado por un enlace plantado en esa ruta.
//!
//! Reusa el hash y las exclusiones de `boza-nucleo` (no duplica lógica).

use boza_nucleo::{
    exclusiones_por_defecto, plataforma::descartar_cache, verificacion::hash_archivo,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use walkdir::WalkDir;

/// Cada cuántos registros nuevos se vuelca el checkpoint al SO. Un lote perdido
/// por una muerte del proceso se re-hashea al reanudar — barato frente a hashear.
const LOTE_PROGRESO: u64 = 256;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (modo, dir, manif, reanudar) = match args.as_slice() {
        [_, m, d, f] if m == "sellar" || m == "verificar" => {
            (m.clone(), PathBuf::from(d), PathBuf::from(f), false)
        }
        [_, m, d, f, r] if (m == "sellar" || m == "verificar") && r == "--reanudar" => {
            (m.clone(), PathBuf::from(d), PathBuf::from(f), true)
        }
        _ => {
            eprintln!(
                "uso:\n  boza-scrub sellar    <dir> <manifiesto> [--reanudar]\n  \
                 boza-scrub verificar <dir> <manifiesto> [--reanudar]"
            );
            return ExitCode::from(2);
        }
    };

    let resultado = if modo == "sellar" {
        sellar(&dir, &manif, reanudar)
    } else {
        verificar(&dir, &manif, reanudar)
    };
    match resultado {
        Ok(codigo) => codigo,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// ¿Algún componente de la ruta relativa casa un nombre de caché excluido? (Misma
/// regla que boza, para sellar/verificar exactamente lo que se respalda.)
fn excluido(rel: &Path, exclusiones: &[String]) -> bool {
    rel.components().any(|c| {
        let n = c.as_os_str();
        exclusiones.iter().any(|x| n == x.as_str())
    })
}

/// Un hash blake3 en hex: exactamente 64 dígitos hex. Un registro de checkpoint/
/// manifiesto cuyo hash no lo cumpla está corrupto (línea torcida, manipulación) y
/// no se confía en él al retomar.
fn es_hash_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Estados válidos del checkpoint de verificación. Cualquier otro (incluida una línea
/// torcida o manipulada) NO se interpreta como «limpio»: se descarta y la ruta se
/// re-verifica (D20 — un dato ausente/dudoso falla ruidosamente, no se rellena).
fn estado_valido(s: &str) -> bool {
    matches!(s, "ok" | "rot" | "nuevo")
}

/// ¿La ruta lleva un carácter que rompe el formato por línea tab-separado? Un nombre
/// con `\n`/`\t`/`\r` no se puede sellar/registrar sin corromper el archivo o inyectar
/// un registro falso, así que se reporta como problema en vez de ingerirlo.
fn tiene_control(s: &str) -> bool {
    s.contains(['\n', '\t', '\r'])
}

/// Hash blake3 leyendo DEL PLATO: descarta la caché del archivo antes de hashear,
/// para que una pudrición viva solo en RAM no pase inadvertida.
fn hash_del_plato(ruta: &Path) -> std::io::Result<String> {
    descartar_cache(ruta); // Linux: FADV_DONTNEED; Windows: no-op documentado
    Ok(hash_archivo(ruta)?.to_hex().to_string())
}

/// Ruta del checkpoint efímero de `verificar`, junto al manifiesto. Se AÑADE el
/// sufijo (no se reemplaza la extensión): `manif.txt` → `manif.txt.verif-progreso`.
fn ruta_progreso(manif: &Path) -> PathBuf {
    let mut s: OsString = manif.as_os_str().to_owned();
    s.push(".verif-progreso");
    PathBuf::from(s)
}

/// Crea (truncando) un archivo que el código genera él mismo, SIN seguir un symlink
/// plantado en esa ruta (`O_NOFOLLOW` en unix): un `.verif-progreso` que fuera un
/// enlace a otro archivo se truncaría al abrirlo de la forma normal. En no-unix,
/// `File::create` corriente (el vector es el modelo de permisos POSIX).
fn crear_sin_symlink(ruta: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    o.open(ruta)
}

/// Carga un archivo tabulado `<valor>\t<ruta>` a un mapa `ruta -> valor`, conservando
/// solo los registros BIEN FORMADOS (`valido(valor)` y ruta sin carácter de control).
/// Devuelve además las líneas RECHAZADAS (no vacías que no produjeron un registro
/// válido). Un archivo ausente da un mapa vacío (retomar sin checkpoint = desde cero).
///
/// El llamante decide qué hacer con lo rechazado: en el CHECKPOINT efímero o al
/// reanudar un SELLADO se ignora (se reconstruye / se re-sella), pero en el MANIFIESTO
/// de una verificación una línea corrupta es un PROBLEMA —el sello autoritativo no se
/// descarta en silencio— (D20/D23: no rellenar ni callar un dato que no cuadra).
fn cargar_validado(
    ruta: &Path,
    valido: impl Fn(&str) -> bool,
) -> std::io::Result<(HashMap<String, String>, Vec<String>)> {
    let f = match File::open(ruta) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((HashMap::new(), Vec::new()))
        }
        Err(e) => return Err(e),
    };
    let mut m = HashMap::new();
    let mut rechazadas = Vec::new();
    for linea in BufReader::new(f).lines() {
        let linea = linea?;
        if linea.trim().is_empty() {
            continue; // línea en blanco (p. ej. el salto final): no es corrupción
        }
        match linea.split_once('\t') {
            Some((val, r)) if valido(val) && !tiene_control(r) => {
                m.insert(r.to_string(), val.to_string());
            }
            _ => rechazadas.push(linea),
        }
    }
    Ok((m, rechazadas))
}

/// Recorre `dir` podando las exclusiones y llama a `f(rel, ruta_absoluta)` por
/// cada ARCHIVO regular. Los subárboles ilegibles se saltan (se informan aparte).
fn por_cada_archivo(
    dir: &Path,
    exclusiones: &[String],
    mut f: impl FnMut(&Path, &Path),
) -> (u64, Vec<(PathBuf, String)>) {
    let mut ilegibles = Vec::new();
    let mut it = WalkDir::new(dir).follow_links(false).into_iter();
    let mut n = 0u64;
    while let Some(entrada) = it.next() {
        let entrada = match entrada {
            Ok(e) => e,
            Err(err) => {
                let ruta = err
                    .path()
                    .and_then(|p| p.strip_prefix(dir).ok())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();
                ilegibles.push((ruta, err.to_string()));
                continue;
            }
        };
        let rel = match entrada.path().strip_prefix(dir) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if excluido(rel, exclusiones) {
            if entrada.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        if entrada.file_type().is_file() {
            n += 1;
            f(rel, entrada.path());
        }
    }
    (n, ilegibles)
}

fn sellar(dir: &Path, manif: &Path, reanudar: bool) -> std::io::Result<ExitCode> {
    let excl = exclusiones_por_defecto();
    // Reanudar: lo ya sellado se retoma, pero solo los registros BIEN FORMADOS (hash
    // de 64 hex, ruta sin control); una línea torcida por una muerte previa se
    // descarta y ese archivo se re-sella. Sin el flag, se empieza de cero.
    let ya: HashMap<String, String> = if reanudar {
        // Al reanudar un SELLADO, una línea corrupta del manifiesto parcial se descarta
        // sin ruido: ese archivo se re-sella y el sello final queda correcto (auto-sana).
        cargar_validado(manif, es_hash_hex)?.0
    } else {
        HashMap::new()
    };
    // Se reescribe el manifiesto LIMPIO (lo válido ya sellado), y se continúa: así una
    // línea torcida nunca sobrevive pegada al `append` de la nueva corrida.
    let salida = File::create(manif)?;
    let manif_canon = manif.canonicalize().ok();
    let mut w = BufWriter::new(salida);
    for (r, h) in &ya {
        let _ = writeln!(w, "{h}\t{r}");
    }
    let _ = w.flush();

    let mut fallos: Vec<(PathBuf, String)> = Vec::new();
    let mut no_representables: Vec<PathBuf> = Vec::new();
    let mut lineas = 0u64;
    let mut saltados = 0u64;
    let mut desde_flush = 0u64;
    let (_, ilegibles) = por_cada_archivo(dir, &excl, |rel, abs| {
        if abs.canonicalize().ok() == manif_canon {
            return;
        }
        let clave = rel.display().to_string();
        if tiene_control(&clave) {
            // nombre que rompería el formato por línea: se reporta, no se ingiere.
            no_representables.push(rel.to_path_buf());
            return;
        }
        if ya.contains_key(&clave) {
            saltados += 1; // ya sellado en una corrida previa: se retoma.
            return;
        }
        match hash_del_plato(abs) {
            Ok(h) => {
                // formato: <hash>\t<ruta relativa>
                if writeln!(w, "{h}\t{clave}").is_ok() {
                    lineas += 1;
                    desde_flush += 1;
                    if desde_flush >= LOTE_PROGRESO {
                        let _ = w.flush(); // checkpoint best-effort; el final sí propaga
                        desde_flush = 0;
                    }
                }
            }
            Err(e) => fallos.push((rel.to_path_buf(), e.to_string())),
        }
    });
    w.flush()?;
    if reanudar {
        println!(
            "Sellado: {lineas} archivos nuevos ({saltados} ya estaban) en {}",
            manif.display()
        );
    } else {
        println!("Sellado: {lineas} archivos en {}", manif.display());
    }
    informar_fallos("no se pudieron hashear", &fallos);
    informar_fallos(
        "nombres no representables (NO sellados: llevan \\n/\\t/\\r)",
        &no_representables
            .iter()
            .map(|r| (r.clone(), "se omitió del sello".to_string()))
            .collect::<Vec<_>>(),
    );
    informar_fallos("subárboles ilegibles", &ilegibles);
    // Sellar es de solo lectura: informa fallos pero no es un veredicto rojo.
    Ok(ExitCode::SUCCESS)
}

/// El resultado de un barrido de verificación, separado de su presentación: así el
/// veredicto (ExitCode) y el stdout se DERIVAN de él, y las pruebas miden el barrido
/// de verdad —vistos, nuevos espurios— y no solo el código de salida (D16/D33).
struct Parte {
    total_sellado: usize,
    vistos: u64,
    cambiados: Vec<String>,
    faltan: Vec<String>,
    nuevos: Vec<String>,
    ilegibles_hash: Vec<(PathBuf, String)>,
    ilegibles: Vec<(PathBuf, String)>,
    no_representables: Vec<PathBuf>,
    manifiesto_corrupto: Vec<String>,
}

impl Parte {
    /// ROJO si hay corrupción/pérdida, algo ilegible / no representable, o el SELLO
    /// mismo está corrupto (D20: el silencio no vale). `nuevos` NO es rojo: el respaldo
    /// creció desde el sello.
    fn hay_problema(&self) -> bool {
        !self.cambiados.is_empty()
            || !self.faltan.is_empty()
            || !self.ilegibles_hash.is_empty()
            || !self.ilegibles.is_empty()
            || !self.no_representables.is_empty()
            || !self.manifiesto_corrupto.is_empty()
    }
}

/// Núcleo de `verificar`: re-hashea del plato y compara contra el sello, retomando el
/// checkpoint si `reanudar`. No imprime ni decide el ExitCode (eso es `verificar`).
fn calcular_verificacion(dir: &Path, manif: &Path, reanudar: bool) -> std::io::Result<Parte> {
    let excl = exclusiones_por_defecto();
    // Cargar el manifiesto: ruta relativa -> hash esperado. Solo registros bien
    // formados; una línea corrupta del sello autoritativo NO se descarta en silencio
    // —es un PROBLEMA (D20)—, a diferencia del checkpoint efímero de más abajo.
    let (mut esperados, manifiesto_corrupto) = cargar_validado(manif, es_hash_hex)?;
    let total_sellado = esperados.len();
    let manif_canon = manif.canonicalize().ok();
    let prog_ruta = ruta_progreso(manif);

    let mut cambiados: Vec<String> = Vec::new();
    let mut nuevos: Vec<String> = Vec::new();
    let mut ilegibles_hash: Vec<(PathBuf, String)> = Vec::new();
    let mut no_representables: Vec<PathBuf> = Vec::new();
    let mut vistos = 0u64;

    // Reanudar: retomar el checkpoint, pero solo registros con estado VÁLIDO (un
    // estado desconocido no es «limpio» — se descarta y el archivo se re-verifica).
    let ya: HashMap<String, String> = if reanudar {
        // El checkpoint es efímero y reconstruible: una línea torcida se descarta en
        // silencio (ese archivo se re-verifica). No es el sello autoritativo.
        let previos = cargar_validado(&prog_ruta, estado_valido)?.0;
        for (r, estado) in &previos {
            esperados.remove(r); // ya procesado: ni se re-verifica ni cuenta como "falta"
            match estado.as_str() {
                "rot" => cambiados.push(r.clone()),
                "nuevo" => nuevos.push(r.clone()),
                _ => {} // "ok": íntegro, no va a ninguna lista
            }
            vistos += 1;
        }
        previos
    } else {
        HashMap::new()
    };

    // El progreso se reescribe LIMPIO (solo lo válido retomado) y sin seguir symlinks,
    // y de ahí se continúa. Se ESCRIBE siempre (para que ESTA corrida sea reanudable
    // si muere); solo se LEE con --reanudar.
    let prog_salida = crear_sin_symlink(&prog_ruta)?;
    let prog_canon = prog_ruta.canonicalize().ok();
    let mut wp = BufWriter::new(prog_salida);
    for (r, e) in &ya {
        let _ = writeln!(wp, "{e}\t{r}");
    }
    let _ = wp.flush();
    let mut desde_flush = 0u64;

    let (_, ilegibles) = por_cada_archivo(dir, &excl, |rel, abs| {
        let abs_canon = abs.canonicalize().ok();
        if abs_canon == manif_canon || (abs_canon.is_some() && abs_canon == prog_canon) {
            return; // el manifiesto / su checkpoint no se verifican contra sí mismos
        }
        let clave = rel.display().to_string();
        if tiene_control(&clave) {
            // nombre que rompería el checkpoint: se reporta como problema, no se ingiere.
            no_representables.push(rel.to_path_buf());
            return;
        }
        if ya.contains_key(&clave) {
            return; // comprobado en una corrida previa: se retoma del checkpoint
        }
        match hash_del_plato(abs) {
            Ok(h) => {
                let estado = match esperados.remove(&clave) {
                    Some(esperado) if esperado == h => "ok", // íntegro
                    Some(_) => {
                        cambiados.push(clave.clone());
                        "rot" // ROT / editado
                    }
                    None => {
                        nuevos.push(clave.clone());
                        "nuevo" // nuevo desde el sello
                    }
                };
                if writeln!(wp, "{estado}\t{clave}").is_ok() {
                    desde_flush += 1;
                    if desde_flush >= LOTE_PROGRESO {
                        let _ = wp.flush(); // checkpoint best-effort
                        desde_flush = 0;
                    }
                }
            }
            Err(e) => ilegibles_hash.push((rel.to_path_buf(), e.to_string())),
        }
        vistos += 1;
    });
    let _ = wp.flush();
    // Lo que queda en `esperados` estaba en el manifiesto y NO se encontró.
    let faltan: Vec<String> = esperados.into_keys().collect();

    // El barrido TERMINÓ: el checkpoint efímero ya no sirve y se borra. Solo sobrevive
    // si el proceso muere ANTES de llegar aquí, que es su razón de ser. `remove_file`
    // sobre un symlink borra el ENLACE, no su objetivo.
    drop(wp);
    let _ = std::fs::remove_file(&prog_ruta);

    Ok(Parte {
        total_sellado,
        vistos,
        cambiados,
        faltan,
        nuevos,
        ilegibles_hash,
        ilegibles,
        no_representables,
        manifiesto_corrupto,
    })
}

fn verificar(dir: &Path, manif: &Path, reanudar: bool) -> std::io::Result<ExitCode> {
    let p = calcular_verificacion(dir, manif, reanudar)?;
    println!(
        "Verificados {} archivos contra {} sellados.\n  \
         cambiados(ROT): {}  faltan: {}  nuevos: {}",
        p.vistos,
        p.total_sellado,
        p.cambiados.len(),
        p.faltan.len(),
        p.nuevos.len()
    );
    listar("CAMBIADOS (posible bit-rot o edición)", &p.cambiados);
    listar("FALTAN (estaban en el sello, no en el disco)", &p.faltan);
    informar_fallos("no se pudieron re-hashear", &p.ilegibles_hash);
    informar_fallos(
        "nombres no representables (\\n/\\t/\\r): no se pueden verificar por el sello",
        &p.no_representables
            .iter()
            .map(|r| (r.clone(), "nombre con carácter de control".to_string()))
            .collect::<Vec<_>>(),
    );
    informar_fallos("subárboles ilegibles", &p.ilegibles);
    if !p.manifiesto_corrupto.is_empty() {
        println!(
            "  MANIFIESTO CORRUPTO ({} líneas mal formadas — el sello no es fiable):",
            p.manifiesto_corrupto.len()
        );
        for l in p.manifiesto_corrupto.iter().take(10) {
            println!("    · {l}");
        }
    }
    if !p.nuevos.is_empty() {
        println!("  ({} nuevos desde el sello — informativo)", p.nuevos.len());
    }
    Ok(if p.hay_problema() {
        ExitCode::from(1)
    } else {
        println!("ÍNTEGRO: cada archivo del sello coincide en el plato.");
        ExitCode::SUCCESS
    })
}

fn listar(titulo: &str, xs: &[String]) {
    if xs.is_empty() {
        return;
    }
    println!("  {titulo}:");
    for x in xs.iter().take(50) {
        println!("    · {x}");
    }
    if xs.len() > 50 {
        println!("    … y {} más", xs.len() - 50);
    }
}

fn informar_fallos(titulo: &str, xs: &[(PathBuf, String)]) {
    if xs.is_empty() {
        return;
    }
    println!("  {titulo} ({}):", xs.len());
    for (r, m) in xs.iter().take(20) {
        println!("    · {}: {m}", r.display());
    }
    if xs.len() > 20 {
        println!("    … y {} más", xs.len() - 20);
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use std::fs;

    fn arbol(dir: &Path) {
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"AAA").unwrap();
        fs::write(dir.join("sub/b.txt"), b"BBB").unwrap();
        // caché que debe excluirse (no entra al sello)
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("target/basura.o"), b"junk").unwrap();
    }

    #[test]
    fn sella_y_verifica_integro() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        assert_eq!(sellar(d.path(), &m, false).unwrap(), ExitCode::SUCCESS);
        // el manifiesto NO contiene el caché excluido
        let cont = fs::read_to_string(&m).unwrap();
        assert!(cont.contains("a.txt") && cont.contains("sub/b.txt"));
        assert!(!cont.contains("target"), "target excluido del sello");
        // verificar sin cambios = íntegro
        assert_eq!(verificar(d.path(), &m, false).unwrap(), ExitCode::SUCCESS);
    }

    #[test]
    fn caza_un_byte_podrido() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        // "pudrir" un archivo tras el sello (mismo tamaño, 1 byte distinto)
        fs::write(d.path().join("a.txt"), b"AAX").unwrap();
        assert_eq!(
            verificar(d.path(), &m, false).unwrap(),
            ExitCode::from(1),
            "un cambio de contenido debe salir ROJO"
        );
    }

    #[test]
    fn caza_un_archivo_perdido() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        fs::remove_file(d.path().join("sub/b.txt")).unwrap();
        assert_eq!(
            verificar(d.path(), &m, false).unwrap(),
            ExitCode::from(1),
            "falta => ROJO"
        );
    }

    // AGUJA (D33): sellar --reanudar RETOMA lo ya sellado y NO lo re-hashea. Sonda
    // observable: se pre-siembra a.txt con un hash FALSO (64 hex, pasa la validación);
    // si reanudar funciona, lo conserva (lo retomó). MUTANTE = quitar `if ya...`: a.txt
    // se re-hashea → su hash FALSO se corrige al real. Por la vía real (sellar).
    #[test]
    fn sellar_reanuda_retoma_lo_ya_sellado() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        let falso = "f".repeat(64);
        fs::write(&m, format!("{falso}\ta.txt\n")).unwrap();
        assert_eq!(sellar(d.path(), &m, true).unwrap(), ExitCode::SUCCESS);
        let cont = fs::read_to_string(&m).unwrap();
        assert!(
            cont.contains(&format!("{falso}\ta.txt")),
            "a.txt debió retomarse (no re-hashear lo sellado): {cont}"
        );
        assert_eq!(
            cont.matches("\ta.txt").count(),
            1,
            "a.txt no se duplica al reanudar: {cont}"
        );
        assert!(cont.contains("sub/b.txt"), "lo pendiente sí se sella: {cont}");
    }

    // AGUJA (D33): verificar --reanudar CONFÍA en el checkpoint y no re-verifica lo ya
    // comprobado. Checkpoint marca a.txt="rot" aunque está íntegro → cuenta como
    // cambiado y sale ROJO. MUTANTE = no cargar/reconstruir el checkpoint: a.txt se
    // re-hashea íntegro → exit 0. Y se comprueba que el checkpoint se BORRA.
    #[test]
    fn verificar_reanuda_confia_en_el_checkpoint() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        let prog = ruta_progreso(&m);
        fs::write(&prog, "rot\ta.txt\n").unwrap();
        assert_eq!(
            verificar(d.path(), &m, true).unwrap(),
            ExitCode::from(1),
            "reanudar confía en el checkpoint (a.txt=rot) y sale rojo"
        );
        assert!(!prog.exists(), "el checkpoint efímero se borra al completar");
    }

    // CONTROL (D33): SIN --reanudar el checkpoint viejo de una corrida muerta se
    // IGNORA, así que un checkpoint envenenado no contamina un barrido limpio.
    #[test]
    fn verificar_sin_reanudar_ignora_checkpoint_viejo() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        let prog = ruta_progreso(&m);
        fs::write(&prog, "rot\ta.txt\n").unwrap();
        assert_eq!(
            verificar(d.path(), &m, false).unwrap(),
            ExitCode::SUCCESS,
            "sin --reanudar NO se lee el checkpoint viejo"
        );
        assert!(!prog.exists(), "y se limpia al terminar");
    }

    // AGUJA de EFICIENCIA (D33/D16): reanudar NO re-hashea lo que el checkpoint ya dio
    // por comprobado. Se mide el PARTE (el exit no lo delata: un re-proceso cae en
    // `nuevos`). Checkpoint: a.txt="ok" → vistos=2 (a.txt del checkpoint + sub/b.txt),
    // sin nuevos. MUTANTE = quitar el salto: a.txt se re-hashea, su esperado ya se
    // consumió → entra espurio a `nuevos` y vistos sube a 3.
    #[test]
    fn reanudar_no_reprocesa_lo_del_checkpoint() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        let prog = ruta_progreso(&m);
        fs::write(&prog, "ok\ta.txt\n").unwrap();
        let p = calcular_verificacion(d.path(), &m, true).unwrap();
        assert_eq!(p.vistos, 2, "a.txt (checkpoint) + sub/b.txt, sin re-procesar");
        assert!(p.nuevos.is_empty(), "nada espurio en nuevos: {:?}", p.nuevos);
        assert!(!p.hay_problema(), "árbol íntegro");
    }

    // SEGURIDAD (puerta 2026-10-31): un estado DESCONOCIDO en el checkpoint NO es
    // «limpio». a.txt está PODRIDO en disco y el checkpoint lo marca con un estado
    // inválido ("WTF"); al reanudar, ese registro se descarta → a.txt se re-verifica →
    // ROT → ROJO. MUTANTE = el `_ => {}` de antes sin filtrar el estado: "WTF" entraría
    // a `ya`, a.txt se saltaría como íntegro y la pudrición quedaría oculta (exit 0).
    #[test]
    fn estado_desconocido_no_es_limpio_se_reverifica() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        fs::write(d.path().join("a.txt"), b"AAX").unwrap(); // pudre a.txt tras el sello
        let prog = ruta_progreso(&m);
        fs::write(&prog, "WTF\ta.txt\n").unwrap(); // estado inválido: no se debe confiar
        assert_eq!(
            verificar(d.path(), &m, true).unwrap(),
            ExitCode::from(1),
            "un estado inválido obliga a re-verificar; la pudrición no se oculta"
        );
    }

    // SEGURIDAD (puerta 2026-10-31): un checkpoint TORCIDO (última línea sin TAB ni
    // salto por una muerte del proceso) no contamina al reanudar: la línea rota se
    // descarta y lo válido se respeta. a.txt="rot" válido + basura → a.txt cuenta,
    // sale rojo, y no revienta por la basura.
    #[test]
    fn checkpoint_torcido_se_descarta_no_contamina() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        let prog = ruta_progreso(&m);
        fs::write(&prog, "rot\ta.txt\nESTO_ES_UNA_LINEA_TORCIDA_SIN_TAB").unwrap();
        assert_eq!(
            verificar(d.path(), &m, true).unwrap(),
            ExitCode::from(1),
            "la línea válida (a.txt=rot) se respeta pese a la basura torcida"
        );
    }

    // SEGURIDAD (puerta 2026-10-31, 2º pase): el MANIFIESTO es el sello autoritativo;
    // una línea corrupta NO se descarta en silencio (a diferencia del checkpoint). Sello
    // con una línea basura + árbol íntegro → ROJO por manifiesto_corrupto. MUTANTE = el
    // drop silencioso al cargar el manifiesto: la línea basura se ignora → exit 0.
    #[test]
    fn manifiesto_corrupto_no_se_traga_en_silencio() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap(); // sello válido del árbol íntegro
        // ensuciar el sello con una línea mal formada (hash que no es 64-hex)
        let mut cont = fs::read_to_string(&m).unwrap();
        cont.push_str("NO_ES_UN_HASH\tsub/b.txt\n");
        fs::write(&m, cont).unwrap();
        let p = calcular_verificacion(d.path(), &m, false).unwrap();
        assert!(
            !p.manifiesto_corrupto.is_empty(),
            "una línea corrupta del sello se reporta"
        );
        assert!(p.hay_problema(), "y el veredicto es ROJO, no un verde falso");
    }

    // SEGURIDAD (puerta 2026-10-31): un nombre con un carácter de control (\n) no se
    // puede representar en el formato por línea; se REPORTA como problema en vez de
    // romper el manifiesto/checkpoint o inyectar un registro falso. MUTANTE = sin el
    // chequeo `tiene_control`: el nombre con \n parte la línea en dos, corrompe el
    // sello y NO aparece en `no_representables`.
    #[test]
    fn nombre_con_control_se_reporta_no_se_ingiere() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        // archivo con salto de línea en el nombre (válido en ext4/tmpfs)
        if fs::write(d.path().join("x\ny.txt"), b"Z").is_err() {
            return; // el FS no admite \n en nombres: el caso no aplica aquí
        }
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        // el manifiesto no tiene una línea partida por el \n del nombre
        let cont = fs::read_to_string(&m).unwrap();
        assert!(!cont.contains("x\ny.txt"), "el nombre con \\n no se seló crudo");
        let p = calcular_verificacion(d.path(), &m, false).unwrap();
        assert!(
            !p.no_representables.is_empty(),
            "el nombre con control se reporta como problema"
        );
        assert!(p.hay_problema(), "y el veredicto es rojo");
    }

    // SEGURIDAD (puerta 2026-10-31, unix): el `.verif-progreso` se crea sin seguir un
    // symlink plantado en esa ruta. Si fuera un enlace a un archivo testigo, abrirlo
    // de la forma normal lo truncaría. MUTANTE = `File::create` corriente: el testigo
    // se trunca a vacío.
    #[cfg(unix)]
    #[test]
    fn progreso_no_sigue_symlink() {
        let d = tempfile::tempdir().unwrap();
        arbol(d.path());
        let m = d.path().join("manif.txt");
        sellar(d.path(), &m, false).unwrap();
        let testigo = d.path().join("testigo");
        fs::write(&testigo, b"IMPORTANTE").unwrap();
        let prog = ruta_progreso(&m);
        std::os::unix::fs::symlink(&testigo, &prog).unwrap();
        // verificar intenta crear el progreso: con O_NOFOLLOW falla, no sigue el enlace.
        let r = calcular_verificacion(d.path(), &m, false);
        assert!(r.is_err(), "O_NOFOLLOW debe rechazar el symlink");
        assert_eq!(
            fs::read(&testigo).unwrap(),
            b"IMPORTANTE",
            "el archivo apuntado por el symlink NO se truncó"
        );
    }
}
