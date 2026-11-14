// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Copia de un árbol entre dos unidades.
//!
//! RÁPIDA: usa reflink (Copy-on-Write, ioctl FICLONE) cuando origen y destino
//! comparten sistema de archivos — la copia es casi instantánea y no duplica
//! bloques; si no se puede, cae a copia normal.
//!
//! SEGURA: nunca toca el origen; rechaza rutas anidadas (copiar un árbol dentro
//! de sí mismo); preserva permisos y fecha; hace `fsync`; y si se pide, VERIFICA
//! por hash que cada archivo del destino es idéntico al del origen.

use crate::perfil::planificar;
use crate::plataforma::{self, Sincronizador};
use crate::verificacion::hash_archivo;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Archivos por lote antes de sincronizar y verificar. Acota la memoria (cada
/// pendiente son ~1 ruta + 32 B de hash) y amortiza el `syncfs` sobre muchos.
const LOTE_ARCHIVOS: usize = 512;
/// …o este volumen de bytes copiados, lo que llegue antes.
const LOTE_BYTES: u64 = 256 << 20;

/// Tolerancia al comparar la FECHA de modificación entre origen y destino, para
/// el salto incremental por tamaño+fecha (`Deteccion::TamanoFecha`). Es el
/// `--modify-window` de rsync, y NO es un lujo: cada sistema de archivos guarda
/// el mtime con distinta resolución —ext4 1 ns, NTFS 100 ns, exFAT 10 ms, FAT
/// 2 s—, así que una copia ext4→NTFS fiel deja el destino desviado hasta ~100 ns
/// del origen (MEDIDO: 77 ns). Sin ventana, `to == td` falla SIEMPRE cross-FS y
/// el salto nunca dispara: la re-copia rehashea todo el árbol aunque nada cambió.
/// 2 s cubre FAT (el peor caso) y por tanto NTFS/exFAT. El precio —un archivo
/// re-modificado dentro de la ventana con tamaño idéntico se omite— es el mismo
/// de rsync y se evita con `Deteccion::Contenido`.
const VENTANA_MTIME: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sobrescritura {
    /// Reescribe siempre el archivo del destino.
    Siempre,
    /// Solo si el del destino difiere (por contenido) del origen.
    SiDistinto,
    /// Nunca reescribe: deja el que ya está.
    Nunca,
}

/// Cómo se decide si un archivo que YA existe en el destino puede omitirse. Es un
/// eje ORTOGONAL a [`Opciones::verificar`]: `verificar` gobierna si lo que SE
/// COPIA se re-lee del plato y se hashea; `Deteccion` gobierna cómo se decide
/// NO copiar algo que ya está. Antes los dos vivían en un solo flag
/// (`salto_rapido && !verificar`), y eso ataba la velocidad de la re-copia a la
/// verificación: con `verificar=true` la re-copia rehasheaba TODO el destino
/// (medido: 13× más lento que el salto por fecha) aunque no reescribiera un byte.
/// Son dos preguntas distintas, así que son dos campos (directiva 32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deteccion {
    /// Omitir si el destino tiene el MISMO tamaño y fecha (como `rsync -a`). No
    /// lee el contenido: la re-copia es incremental y barata. No detecta una
    /// corrupción EN SITIO que conserve tamaño+mtime — eso es bit-rot del
    /// destino, y su detección es trabajo de `boza-scrub` (manifiesto), no de
    /// cada copia. Lo omitido se informa como `omitidos`, NO como `verificados`:
    /// el parte no afirma haber comprobado su contenido (directiva 20).
    TamanoFecha,
    /// Omitir solo si el CONTENIDO coincide (hash de ambos lados, el destino
    /// leído del plato). Caro en re-copias (relee el destino entero), pero una
    /// corrupción en sitio de igual tamaño+fecha se detecta y se recopia.
    /// Equivale a `rsync -c` y además audita el destino en la misma pasada.
    Contenido,
}

pub struct Opciones {
    pub verificar: bool,
    pub preservar_tiempo: bool,
    pub preservar_permisos: bool,
    pub sobrescribir: Sobrescritura,
    /// Cómo se decide omitir un archivo que ya existe en el destino (ver
    /// [`Deteccion`]). Solo afecta la DECISIÓN de omitir; lo que SÍ se copia se
    /// verifica según `verificar`, con independencia de este campo.
    pub deteccion: Deteccion,
    /// Nombres de carpeta/archivo que NO se copian: se poda todo su subárbol. Un
    /// nodo se excluye si ALGÚN componente de su ruta relativa casa uno de estos
    /// nombres (p. ej. `target` poda `a/target/b`). Pensado para cachés
    /// regenerables (compilación, dependencias) que inflan un respaldo sin valor.
    pub exclusiones: Vec<String>,
}

/// Cachés regenerables que por defecto NO se respaldan: los reconstruyen sus
/// herramientas (cargo, npm, pip…) y clonarlos multiplica el tiempo y el tamaño.
///
/// `.git` NO está aquí a propósito: es la HISTORIA del repositorio, no un caché
/// regenerable — excluirlo por defecto haría un respaldo que pierde el historial
/// en silencio (directiva 22: una poda que borra más de lo que se cree es pérdida
/// de datos). Quien quiera excluirlo lo añade a mano.
pub fn exclusiones_por_defecto() -> Vec<String> {
    ["target", "node_modules", ".venv", "__pycache__", ".cache"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl Default for Opciones {
    fn default() -> Self {
        Self {
            verificar: true,
            preservar_tiempo: true,
            preservar_permisos: true,
            sobrescribir: Sobrescritura::SiDistinto,
            // Por defecto, la DECISIÓN de omitir es barata (tamaño+fecha, como
            // rsync): una re-copia incremental no relee lo que no va a tocar. La
            // verificación por hash se aplica a lo que SE COPIA (`verificar:
            // true`), no a lo que se omite. Para auditar bit-rot del destino
            // completo está `boza-scrub` o `Deteccion::Contenido`.
            deteccion: Deteccion::TamanoFecha,
            // Vacío A PROPÓSITO: el default de la LIBRERÍA es copia FIEL sin
            // sorpresas. Podar cachés es una decisión del llamante (la GTK la
            // activa con su casilla, encendida por defecto en la interfaz). Así
            // un consumidor programático que use `..Default::default()` no hereda
            // una poda silenciosa (obs. del concilio 2026-09-10).
            exclusiones: Vec::new(),
        }
    }
}

/// ¿Este nodo del árbol está excluido? True si algún componente de su ruta
/// relativa a `origen` casa un nombre de `exclusiones`.
pub(crate) fn excluido(entrada: &walkdir::DirEntry, origen: &Path, exclusiones: &[String]) -> bool {
    let Ok(rel) = entrada.path().strip_prefix(origen) else {
        return false;
    };
    excluido_rel(rel, exclusiones)
}

/// ¿La ruta RELATIVA está excluida? True si algún componente casa (igualdad
/// EXACTA, no subcadena) un nombre de `exclusiones`. La comparten el recorrido
/// filtrado (`recorrer`, para conteo y verificación) y el bucle de copia (que
/// además CUENTA lo podado) — así la poda es idéntica en todas las pasadas.
pub(crate) fn excluido_rel(rel: &Path, exclusiones: &[String]) -> bool {
    if exclusiones.is_empty() {
        return false;
    }
    rel.components().any(|c| {
        let nombre = c.as_os_str();
        exclusiones.iter().any(|x| nombre == x.as_str())
    })
}

/// Recorre `origen` podando los subárboles excluidos (no desciende en ellos).
/// La MISMA función alimenta el conteo (pase 1), la copia (pase 2) y la
/// verificación independiente, para que los tres vean exactamente el mismo árbol.
pub(crate) fn recorrer<'a>(
    origen: &'a Path,
    exclusiones: &'a [String],
) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> + 'a {
    WalkDir::new(origen)
        .follow_links(false)
        .into_iter()
        .filter_entry(move |e| !excluido(e, origen, exclusiones))
}

/// En qué está el motor ahora mismo — para que la barra diga la verdad («por
/// proceso») en vez de fingir un porcentaje mientras hace otra cosa.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fase {
    /// Contando el árbol de origen (pase 1): aún no se copia nada.
    #[default]
    Midiendo,
    /// Copiando bytes de un archivo.
    Copiando,
    /// Volcando un lote al disco con `syncfs` (puede tardar en un HDD).
    Sincronizando,
    /// Releyendo del plato y comparando hash (verificación).
    Verificando,
    /// Terminado.
    Hecho,
}

/// Estado del avance, para dibujar la barra. Se pasa a la función de progreso
/// DURANTE el trabajo (dentro de un archivo grande, y al cambiar de fase), no
/// solo al terminar cada archivo.
#[derive(Default, Clone, Debug)]
pub struct Progreso {
    pub fase: Fase,
    pub archivos_totales: usize,
    pub archivos_hechos: usize,
    pub bytes_totales: u64,
    /// Bytes ya procesados, INCLUIDOS los parciales del archivo en curso. La
    /// barra real va por bytes (no por nº de archivos), que es fiel cuando los
    /// tamaños son dispares.
    pub bytes_hechos: u64,
    pub actual: Option<PathBuf>,
}

#[derive(Default, Debug)]
pub struct Resultado {
    pub copiados: usize,
    pub omitidos: usize,
    pub verificados: usize,
    pub bytes: u64,
    /// Cuántos nodos (carpeta o archivo) se PODARON por las exclusiones. Existe
    /// para que «verde» no calle la poda: una carpeta REAL llamada como un caché
    /// (`target`, `.cache`…) se descarta por diseño, y el usuario tiene que poder
    /// VERLO en el parte final (hallazgo del concilio — la omisión silenciosa es
    /// el mismo modo de fallo que un fallo que no se reporta, directiva 20).
    pub excluidos: usize,
    /// Las primeras rutas excluidas (tope 50), para enseñarlas en el parte y que
    /// una exclusión errónea salte a la vista.
    pub rutas_excluidas: Vec<PathBuf>,
    /// Ruta relativa + motivo de cada archivo que NO se pudo copiar o que falló
    /// la verificación. Que exista no aborta el resto: se copia todo lo posible y
    /// se informa lo que no (un acta que calla lo que falló sería falsa).
    pub fallos: Vec<(PathBuf, String)>,
}

enum Copia {
    /// Copiado: bytes movidos y, si se verifica, el hash del origen calculado
    /// al vuelo (para verificar por lote leyendo del plato).
    Copiado {
        bytes: u64,
        hash_origen: Option<blake3::Hash>,
    },
    Omitido,
}

/// Un archivo copiado que espera su verificación por lote: la ruta del destino,
/// su ruta relativa (para el informe) y el hash del ORIGEN calculado al vuelo
/// durante la copia. La verificación re-lee el destino del PLATO y compara.
struct PendienteVerif {
    dst: PathBuf,
    rel: PathBuf,
    esperado: blake3::Hash,
}

/// Copia el árbol de `origen` dentro de `destino`, informando el avance.
///
/// El plan de copia (reflink / secuencial / paralelo, buffer, concurrencia) se
/// deriva del PAR de dispositivos — ver [`crate::perfil`].
///
/// Durabilidad y verificación van por LOTES, no por archivo: se copia con
/// `rename` atómico (sin `fsync` por archivo), y cada lote se hace durable con
/// un solo `syncfs` y luego se verifica leyendo del plato (`FADV_DONTNEED`
/// descarta la caché para que el hash no compare la RAM consigo misma). Esto
/// iguala la velocidad de `rsync` sin renunciar a la copia fiel.
pub fn copiar_arbol(
    origen: &Path,
    destino: &Path,
    opts: &Opciones,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<Resultado> {
    validar(origen, destino)?;
    fs::create_dir_all(destino)
        .with_context(|| format!("no se pudo crear el destino {destino:?}"))?;

    let plan = planificar(origen, destino);

    // Fase 1 anunciada ANTES de contar: en un árbol grande el pase 1 tarda, y sin
    // esto la ventana parecería colgada antes de empezar.
    let mut prog = Progreso::default(); // fase = Midiendo
    progreso(&prog);

    // Pase 1: contar (podando exclusiones), para que la barra tenga un total real.
    let (total_arch, total_bytes) = medir(origen, &opts.exclusiones)?;
    prog.archivos_totales = total_arch;
    prog.bytes_totales = total_bytes;
    prog.fase = Fase::Copiando;
    let mut res = Resultado::default();

    // Bytes de archivos YA terminados: base sobre la que el progreso intra-archivo
    // suma los parciales del archivo en curso (para no contar dos veces).
    let mut bytes_base: u64 = 0;

    // Sincronizador del destino: en Linux guarda el descriptor del directorio para
    // `syncfs` por lote; en Windows la durabilidad va por archivo (ver módulo
    // `plataforma`). Abierto una sola vez.
    let sinc = Sincronizador::nuevo(destino)?;

    // Lote pendiente de verificación (solo en modo verificar).
    let mut lote: Vec<PendienteVerif> = Vec::new();
    let mut lote_bytes: u64 = 0;
    // Buffer de la copia en flujo, reusado entre archivos (modo verificar).
    let mut buf = vec![0u8; plan.buffer];

    // Pase 2: copiar. Se recorre MANUALMENTE (no con `recorrer`) para poder CONTAR
    // lo que se poda: `recorrer` filtra en silencio; aquí cada nodo excluido se
    // cuenta antes de saltarlo, con la MISMA regla (`excluido_rel`) que usa el
    // conteo. `skip_current_dir` evita descender en un directorio excluido.
    let mut it = WalkDir::new(origen).follow_links(false).into_iter();
    while let Some(entrada) = it.next() {
        let entrada = match entrada {
            Ok(e) => e,
            Err(err) => {
                // Directorio ilegible: se REGISTRA y se SIGUE (el contrato dice
                // que los fallos se acumulan, no abortan; WalkDir continúa con
                // los hermanos tras un directorio que no pudo leer). Antes un `?`
                // tumbaba el clonado entero por un solo `lost+found` root:700.
                let ruta = err
                    .path()
                    .and_then(|p| p.strip_prefix(origen).ok())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();
                res.fallos.push((ruta, err.to_string()));
                continue;
            }
        };
        let rel = match entrada.path().strip_prefix(origen) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
            _ => continue,
        };
        // Poda VISIBLE: cuenta el nodo excluido, guarda su ruta (tope 50) y no
        // desciende. Así el parte final puede decir cuánto se omitió — «verde» no
        // calla la poda (concilio 2026-09-10).
        if excluido_rel(&rel, &opts.exclusiones) {
            res.excluidos += 1;
            if res.rutas_excluidas.len() < 50 {
                res.rutas_excluidas.push(rel.clone());
            }
            if entrada.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        let dst = destino.join(&rel);
        let ft = entrada.file_type();

        if ft.is_dir() {
            // Si en el destino ya hay un SYMLINK con este nombre, quitarlo ANTES
            // de crear el directorio: `create_dir_all` seguiría el enlace y todos
            // los hijos se escribirían FUERA del árbol (o dentro del origen), con
            // acta "limpia". El origen nunca trae symlinks aquí (van por la rama
            // is_symlink), así que esto solo deshace un enlace preexistente del destino.
            quitar_symlink_preexistente(&dst);
            // Un fallo al crear el directorio (el destino ya es un ARCHIVO, sin
            // permiso, ENOSPC…) se ACUMULA y se sigue — NO aborta con `?`. Si
            // abortara, se saltaría el vaciado final del lote y los archivos ya
            // renombrados quedarían sin `syncfs` (durabilidad perdida en silencio,
            // hallazgo del concilio). Es el mismo contrato que el dir ilegible del
            // origen: los fallos se informan, no tumban la copia (directiva 20).
            if let Err(e) =
                fs::create_dir_all(&dst).with_context(|| format!("no se pudo crear {dst:?}"))
            {
                res.fallos.push((rel.clone(), e.to_string()));
                continue;
            }
            if let Ok(m) = fs::metadata(entrada.path()) {
                copiar_metadatos(&m, &dst, opts).ok();
            }
            continue;
        }

        if ft.is_symlink() {
            if let Err(e) = recrear_enlace(entrada.path(), &dst) {
                res.fallos.push((rel.clone(), e.to_string()));
            }
            continue;
        }

        if ft.is_file() {
            let bytes_archivo = entrada.metadata().map(|m| m.len()).unwrap_or(0);
            prog.fase = Fase::Copiando;
            prog.actual = Some(rel.clone());
            progreso(&prog);
            match copiar_uno(
                entrada.path(),
                &dst,
                opts,
                &mut buf,
                bytes_base,
                &mut prog,
                progreso,
            ) {
                Ok(Copia::Copiado { bytes, hash_origen }) => {
                    res.copiados += 1;
                    res.bytes += bytes;
                    if let Some(esperado) = hash_origen {
                        lote.push(PendienteVerif {
                            dst: dst.clone(),
                            rel: rel.clone(),
                            esperado,
                        });
                        lote_bytes += bytes;
                        if lote.len() >= LOTE_ARCHIVOS || lote_bytes >= LOTE_BYTES {
                            vaciar_lote(&sinc, &mut lote, &mut res, &mut prog, progreso)?;
                            lote_bytes = 0;
                        }
                    }
                }
                Ok(Copia::Omitido) => res.omitidos += 1,
                Err(e) => res.fallos.push((rel.clone(), e.to_string())),
            }
            // El archivo terminó: su tamaño pasa a la BASE y la barra queda en el
            // total exacto de lo hecho (los parciales intra-archivo ya no aplican).
            bytes_base += bytes_archivo;
            prog.archivos_hechos += 1;
            prog.bytes_hechos = bytes_base;
            prog.fase = Fase::Copiando;
            progreso(&prog);
        }
    }

    // Vaciar lo que quede del lote (verifica lo que aún no se verificó); si no se
    // verifica, un solo `syncfs` final hace durable toda la copia (los `rename`
    // ya publicaron los nombres; esto flushea sus datos al disco).
    if !lote.is_empty() {
        vaciar_lote(&sinc, &mut lote, &mut res, &mut prog, progreso)?;
    } else {
        prog.fase = Fase::Sincronizando;
        progreso(&prog);
        sinc.sincronizar()
            .with_context(|| "no se pudo sincronizar el destino al disco".to_string())?;
    }

    prog.fase = Fase::Hecho;
    prog.actual = None;
    progreso(&prog);
    Ok(res)
}

/// Copia UN archivo suelto `orig` al destino ya resuelto `dst`, con las MISMAS
/// garantías del motor: copia a temporal + `rename` atómico, verificación por hash
/// leída del plato (si `opts.verificar`) y durabilidad (`syncfs`). Para el selector
/// de archivos sueltos de la GUI. Rechaza copiar un archivo sobre sí mismo.
pub fn copiar_un_archivo(
    orig: &Path,
    dst: &Path,
    opts: &Opciones,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<Resultado> {
    if !orig.is_file() {
        bail!("el origen {orig:?} no es un archivo");
    }
    if let (Ok(o), Ok(d)) = (orig.canonicalize(), dst.canonicalize()) {
        if o == d {
            bail!("origen y destino son el mismo archivo: {}", dst.display());
        }
    }
    let padre = dst.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(padre).with_context(|| format!("no se pudo crear {padre:?}"))?;
    let sinc = Sincronizador::nuevo(padre)?;
    let mut buf = vec![0u8; 8 << 20];
    let mut res = Resultado::default();
    let bytes_archivo = fs::metadata(orig).map(|m| m.len()).unwrap_or(0);
    let nombre = PathBuf::from(dst.file_name().unwrap_or_default());

    let mut prog = Progreso {
        fase: Fase::Copiando,
        archivos_totales: 1,
        bytes_totales: bytes_archivo,
        actual: Some(nombre.clone()),
        ..Default::default()
    };
    progreso(&prog);

    let mut lote: Vec<PendienteVerif> = Vec::new();
    match copiar_uno(orig, dst, opts, &mut buf, 0, &mut prog, progreso) {
        Ok(Copia::Copiado { bytes, hash_origen }) => {
            res.copiados += 1;
            res.bytes += bytes;
            if let Some(esperado) = hash_origen {
                lote.push(PendienteVerif {
                    dst: dst.to_path_buf(),
                    rel: nombre,
                    esperado,
                });
            }
        }
        Ok(Copia::Omitido) => res.omitidos += 1,
        Err(e) => res.fallos.push((nombre, e.to_string())),
    }
    prog.archivos_hechos = 1;
    prog.bytes_hechos = bytes_archivo;
    if lote.is_empty() {
        prog.fase = Fase::Sincronizando;
        progreso(&prog);
        sinc.sincronizar()
            .with_context(|| "no se pudo sincronizar el destino al disco".to_string())?;
    } else {
        vaciar_lote(&sinc, &mut lote, &mut res, &mut prog, progreso)?;
    }
    prog.fase = Fase::Hecho;
    prog.actual = None;
    progreso(&prog);
    Ok(res)
}

/// Copia una LISTA de items (archivos y/o carpetas) DENTRO de `destino`: cada uno se
/// recrea por su nombre (`a.txt` → `destino/a.txt`, `proj/` → `destino/proj/…`).
/// Mide todo primero para una barra AGREGADA, y reusa `copiar_arbol` (carpetas) y
/// `copiar_un_archivo` (archivos), acumulando un solo `Resultado`. Para el selector
/// multi-origen de la GUI. Un item que falla no aborta el resto (D20).
pub fn copiar_items(
    items: &[PathBuf],
    destino: &Path,
    opts: &Opciones,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<Resultado> {
    if items.is_empty() {
        bail!("no hay nada que copiar: la selección de origen está vacía");
    }
    fs::create_dir_all(destino)
        .with_context(|| format!("no se pudo crear el destino {destino:?}"))?;

    // Pase de medición agregado (para una sola barra honesta sobre todo el conjunto).
    let mut prog = Progreso::default(); // Midiendo
    progreso(&prog);
    let medir_item = |it: &Path| -> (usize, u64) {
        if it.is_dir() {
            medir(it, &opts.exclusiones).unwrap_or((0, 0))
        } else if it.is_file() {
            (1, fs::metadata(it).map(|m| m.len()).unwrap_or(0))
        } else {
            (0, 0)
        }
    };
    let mut total_f = 0usize;
    let mut total_b = 0u64;
    for it in items {
        let (f, b) = medir_item(it);
        total_f += f;
        total_b += b;
    }

    let mut res = Resultado::default();
    let mut base_f = 0usize;
    let mut base_b = 0u64;

    for it in items {
        let nombre = it.file_name().map(PathBuf::from).unwrap_or_default();
        let dest_item = destino.join(&nombre);
        let (item_f, item_b) = medir_item(it);
        // Envoltura: traduce el progreso por-item al acumulado global.
        let (bf, bb) = (base_f, base_b);
        let mut wrap = |p: &Progreso| {
            let mut q = p.clone();
            q.archivos_totales = total_f;
            q.bytes_totales = total_b;
            q.archivos_hechos = bf + p.archivos_hechos;
            q.bytes_hechos = bb + p.bytes_hechos;
            progreso(&q);
        };
        let r = if it.is_dir() {
            copiar_arbol(it, &dest_item, opts, &mut wrap)
        } else if it.is_file() {
            copiar_un_archivo(it, &dest_item, opts, &mut wrap)
        } else {
            Err(anyhow::anyhow!("{} no existe", it.display()))
        };
        match r {
            Ok(ri) => {
                res.copiados += ri.copiados;
                res.omitidos += ri.omitidos;
                res.verificados += ri.verificados;
                res.bytes += ri.bytes;
                res.excluidos += ri.excluidos;
                res.rutas_excluidas.extend(ri.rutas_excluidas);
                res.fallos.extend(ri.fallos);
            }
            Err(e) => res.fallos.push((nombre, e.to_string())),
        }
        base_f += item_f;
        base_b += item_b;
    }

    prog.fase = Fase::Hecho;
    prog.archivos_totales = total_f;
    prog.bytes_totales = total_b;
    prog.archivos_hechos = total_f;
    prog.bytes_hechos = total_b;
    prog.actual = None;
    progreso(&prog);
    Ok(res)
}

/// Sincroniza un lote al disco y verifica cada archivo leyéndolo del PLATO.
/// Vacía `lote`. Un fallo de `syncfs` es un fallo de durabilidad REAL y se
/// propaga (callar aquí es la "copia verde a medias": RAM comparada consigo
/// misma). Una discrepancia de hash se acumula como fallo del archivo.
fn vaciar_lote(
    sinc: &Sincronizador,
    lote: &mut Vec<PendienteVerif>,
    res: &mut Resultado,
    prog: &mut Progreso,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<()> {
    prog.fase = Fase::Sincronizando;
    progreso(prog);
    sinc.sincronizar().with_context(|| {
        "no se pudo sincronizar el lote al disco antes de verificar".to_string()
    })?;
    prog.fase = Fase::Verificando;
    for p in lote.drain(..) {
        prog.actual = Some(p.rel.clone());
        progreso(prog);
        match verificar_del_plato(&p.dst, &p.esperado) {
            Ok(true) => res.verificados += 1,
            Ok(false) => res
                .fallos
                .push((p.rel, "la verificación no coincide".into())),
            Err(e) => res
                .fallos
                .push((p.rel, format!("no se pudo verificar: {e}"))),
        }
    }
    Ok(())
}

/// Rechaza lo que NO se puede copiar con seguridad. El sesgo aquí es cerrar: ante
/// la duda, no se copia (una copia mal encuadrada destruye datos).
fn validar(origen: &Path, destino: &Path) -> Result<()> {
    if !origen.exists() {
        bail!("el origen {origen:?} no existe");
    }
    if !origen.is_dir() {
        bail!("el origen {origen:?} no es un directorio");
    }
    // Canonicalizar para comparar rutas reales (resuelve enlaces y `..`).
    let o = origen
        .canonicalize()
        .with_context(|| format!("no se pudo resolver {origen:?}"))?;
    // El destino puede no existir aún; canonicalizamos su padre.
    let d = if destino.exists() {
        destino.canonicalize()?
    } else {
        let padre = destino.parent().unwrap_or(destino);
        let padre = if padre.as_os_str().is_empty() {
            Path::new(".")
        } else {
            padre
        };
        padre
            .canonicalize()
            .with_context(|| format!("no se pudo resolver el padre de {destino:?}"))?
            .join(destino.file_name().unwrap_or_default())
    };
    if o == d {
        bail!("origen y destino son la misma ruta");
    }
    if d.starts_with(&o) {
        bail!("el destino está DENTRO del origen: se copiaría a sí mismo sin fin");
    }
    if o.starts_with(&d) {
        bail!("el origen está dentro del destino: la copia podría pisar el origen");
    }
    Ok(())
}

fn medir(origen: &Path, exclusiones: &[String]) -> Result<(usize, u64)> {
    let mut n = 0;
    let mut bytes = 0;
    for e in recorrer(origen, exclusiones) {
        // Un subárbol ilegible (p. ej. `lost+found` root:700, presente en todo
        // ext4) NO debe tumbar el conteo entero: se salta aquí y el bucle de
        // copia lo registra como fallo. Antes un `?` abortaba antes de copiar
        // nada — justo lo que el contrato de `Resultado::fallos` dice no hacer.
        let Ok(e) = e else { continue };
        if e.file_type().is_file() {
            n += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok((n, bytes))
}

fn es_symlink(p: &Path) -> bool {
    p.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn quitar_symlink_preexistente(dst: &Path) {
    if es_symlink(dst) {
        fs::remove_file(dst).ok();
    }
}

#[allow(clippy::too_many_arguments)]
fn copiar_uno(
    orig: &Path,
    dst: &Path,
    opts: &Opciones,
    buf: &mut [u8],
    base: u64,
    prog: &mut Progreso,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<Copia> {
    // `Nunca`: si ya hay ALGO con ese nombre (archivo o symlink), no se toca —
    // ni siquiera para quitar el symlink. Va ANTES de la limpieza de symlink de
    // abajo: si no, quitar el enlace haría `dst.exists()` falso y se copiaría
    // igual, incumpliendo «Nunca reescribe: deja el que ya está».
    if matches!(opts.sobrescribir, Sobrescritura::Nunca) && (es_symlink(dst) || dst.exists()) {
        return Ok(Copia::Omitido);
    }
    // Nunca escribir a través de un symlink preexistente del destino: se seguiría
    // y la escritura acabaría fuera del árbol o dentro del origen. Se quita antes.
    if es_symlink(dst) {
        fs::remove_file(dst).ok();
    } else if dst.exists() && plataforma::mismo_archivo(orig, dst)? {
        // Origen y destino resuelven al MISMO archivo (bind mount, doble
        // montaje): sobrescribirlo destruiría la fuente. No se toca.
        bail!(
            "origen y destino resuelven al mismo archivo: {}",
            dst.display()
        );
    }
    if dst.exists() {
        match opts.sobrescribir {
            Sobrescritura::Nunca => return Ok(Copia::Omitido),
            Sobrescritura::SiDistinto => {
                // La DECISIÓN de omitir la gobierna `opts.deteccion`, NO
                // `verificar` (son ejes ortogonales, ver `Deteccion`):
                //   - `TamanoFecha`: omite por tamaño+fecha sin leer contenido
                //     (re-copia barata). Lo omitido va a `omitidos`, no a
                //     `verificados`: el parte no afirma haber comprobado su
                //     contenido. Auditar bit-rot en sitio es `boza-scrub`.
                //   - `Contenido`: compara por HASH de ambos lados (destino del
                //     plato); una corrupción en sitio de igual tamaño+fecha se
                //     detecta y se recopia. Caro, pero audita el destino.
                if no_difiere(orig, dst, opts.deteccion)? {
                    return Ok(Copia::Omitido);
                }
            }
            Sobrescritura::Siempre => {}
        }
    }
    let padre = dst.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(padre)?;

    // Barre temporales HUÉRFANOS de este mismo destino que dejara un proceso
    // muerto (kill -9, corte) en una corrida anterior: si no, `verificar_arbol`
    // los certificaría como parte del árbol y se acumularía basura invisible.
    limpiar_temporales_viejos(dst);

    // Copia a un TEMPORAL hermano y luego `rename` atómico sobre el destino: si la
    // copia falla a mitad (disco lleno, EIO), el destino ANTERIOR queda intacto —
    // antes se borraba primero y un fallo dejaba el destino perdido.
    //
    // NO se hace `fsync` por archivo: la durabilidad va por LOTES (`syncfs` en
    // `vaciar_lote`), que es lo que iguala a rsync en árboles de muchos archivos.
    // El `rename` es atómico con o sin `fsync`; el `syncfs` posterior flushea los
    // datos ya renombrados.
    // Snapshot de la metadata del origen ANTES de leerlo. Si el origen cambia
    // DURANTE la copia —un archivo VIVO reescrito en sitio: una imagen de VM, un
    // contenedor cifrado, un sqlite de páginas fijas—, el temporal queda MEZCLADO
    // de dos versiones. Estampar el mtime de DESPUÉS lo haría pasar por fiel y,
    // con `Deteccion::TamanoFecha`, la re-copia lo omitiría PARA SIEMPRE (regresión
    // que el cambio de default abrió: el default viejo comparaba por contenido y
    // sanaba la copia torcida en la 2ª pasada). Se detecta comparando metadata
    // antes/después y se informa como FALLO ruidoso (D20): el `rename` no ocurre,
    // el destino anterior se conserva. (Sin snapshot del FS no existe copia
    // consistente de un archivo vivo; fallar es más honesto que un verde torcido.)
    let meta_antes = fs::metadata(orig)
        .with_context(|| format!("no se pudo leer la metadata de {}", orig.display()))?;
    let tmp = ruta_temporal(dst);
    let resultado = (|| -> Result<(u64, Option<blake3::Hash>)> {
        let (bytes, hash) = if opts.verificar {
            // Copia en FLUJO calculando el hash del origen con la MISMA lectura
            // que ya hace falta para copiar: elimina una relectura completa del
            // origen frente a "copiar y luego hashear".
            let (n, h) = copiar_en_flujo(orig, &tmp, buf, base, prog, progreso)?;
            (n, Some(h))
        } else {
            // Ruta rápida: reflink (CoW) si comparten fs; si no, copia del kernel
            // (`copy_file_range`). Sin verificación no hace falta el hash al vuelo.
            let n = match reflink_copy::reflink_or_copy(orig, &tmp).with_context(|| {
                format!("no se pudo copiar {} -> {}", orig.display(), tmp.display())
            })? {
                Some(n) => n,
                None => fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0),
            };
            (n, None)
        };
        // El origen no puede haber cambiado durante la copia: si cambió, el
        // temporal está torcido y NO se publica (D20). Compara tamaño y mtime
        // contra el snapshot previo.
        let meta_despues = fs::metadata(orig)
            .with_context(|| format!("no se pudo releer la metadata de {}", orig.display()))?;
        if meta_despues.len() != meta_antes.len()
            || meta_despues.modified().ok() != meta_antes.modified().ok()
        {
            bail!(
                "el origen {} cambió durante la copia (tamaño o fecha): la copia \
                 quedaría torcida y no se publica",
                orig.display()
            );
        }
        // Estampa el mtime del snapshot PREVIO (no re-lee): ver `copiar_metadatos`.
        copiar_metadatos(&meta_antes, &tmp, opts)?;
        // Durabilidad por ARCHIVO antes de publicar el nombre. En Linux es no-op
        // (la hace el `syncfs` por lote); en Windows hace `sync_all` del temporal,
        // porque ahí no hay `syncfs` (ver módulo `plataforma`).
        plataforma::durabilizar_ruta(&tmp)
            .with_context(|| format!("no se pudo hacer durable {}", tmp.display()))?;
        fs::rename(&tmp, dst)
            .with_context(|| format!("no se pudo reemplazar {}", dst.display()))?;
        Ok((bytes, hash))
    })();
    let (bytes, hash_origen) = match resultado {
        Ok(v) => v,
        Err(e) => {
            fs::remove_file(&tmp).ok(); // limpiar el temporal a medio escribir
            return Err(e);
        }
    };
    Ok(Copia::Copiado { bytes, hash_origen })
}

/// Copia `orig` → `tmp` leyéndolo UNA vez y calculando su hash blake3 sobre la
/// marcha. Devuelve (bytes, hash del origen). El buffer se reusa entre archivos.
fn copiar_en_flujo(
    orig: &Path,
    tmp: &Path,
    buf: &mut [u8],
    base: u64,
    prog: &mut Progreso,
    progreso: &mut dyn FnMut(&Progreso),
) -> Result<(u64, blake3::Hash)> {
    let mut entrada =
        File::open(orig).with_context(|| format!("no se pudo abrir {}", orig.display()))?;
    let mut salida =
        File::create(tmp).with_context(|| format!("no se pudo crear {}", tmp.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut total: u64 = 0;
    loop {
        let n = entrada
            .read(buf)
            .with_context(|| format!("error leyendo {}", orig.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        salida
            .write_all(&buf[..n])
            .with_context(|| format!("error escribiendo {}", tmp.display()))?;
        total += n as u64;
        // Progreso DENTRO del archivo: la barra se mueve aunque un solo archivo
        // pese varios GB (antes se congelaba en su nombre hasta terminarlo). Un
        // aviso por trozo (buffer de 4–8 MiB) es un ritmo razonable.
        prog.bytes_hechos = base + total;
        progreso(prog);
    }
    Ok((total, hasher.finalize()))
}

/// Verifica que `dst` en disco coincide con `esperado` (hash del origen),
/// leyéndolo del PLATO: se descarta la caché (limpia tras el `sync`) para que el
/// hash no compare la RAM consigo misma. En Windows el descarte es no-op
/// documentado (ver `plataforma::descartar_cache`).
fn verificar_del_plato(dst: &Path, esperado: &blake3::Hash) -> Result<bool> {
    plataforma::descartar_cache(dst);
    Ok(&hash_archivo(dst)? == esperado)
}

/// Límite de longitud de un NOMBRE de archivo (no la ruta) en ext4 y la mayoría
/// de fs de Linux: 255 bytes.
const NOMBRE_MAX: usize = 255;
/// Espacio que se reserva para el sufijo `.boza-tmp.<pid>.<n>` (más el punto
/// inicial): `.boza-tmp.` = 10, pid hasta ~8 dígitos, `.`, y el contador `n`
/// (u64, hasta 20 dígitos) → holgado con 48.
const RESERVA_TEMP: usize = 48;

/// Trunca `s` a como mucho `max` BYTES sin partir un carácter UTF-8.
fn truncar_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut fin = max;
    while fin > 0 && !s.is_char_boundary(fin) {
        fin -= 1;
    }
    &s[..fin]
}

/// La base del nombre para construir un temporal, RECORTADA si hace falta para
/// que `.<base>.boza-tmp.<pid>.<n>` no pase de `NOMBRE_MAX` bytes. La usan TANTO
/// `ruta_temporal` (crea el temporal) como `prefijo_temporal` (barre huérfanos),
/// para que el barrido siga casando cuando el nombre es largo — si divergieran,
/// un temporal huérfano de un nombre largo no se limpiaría nunca.
fn base_temporal(dst: &Path) -> String {
    let base = dst
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("archivo");
    // 1 por el punto inicial + RESERVA para el sufijo.
    let presupuesto = NOMBRE_MAX.saturating_sub(1 + RESERVA_TEMP);
    truncar_bytes(base, presupuesto).to_string()
}

/// Prefijo de los temporales de un destino dado: `.<base_recortada>.boza-tmp.`.
fn prefijo_temporal(dst: &Path) -> Option<String> {
    dst.file_name()?; // solo para descartar rutas sin nombre (raíz)
    Some(format!(".{}.boza-tmp.", base_temporal(dst)))
}

/// Borra los temporales huérfanos de ESTE destino (de un proceso muerto). Solo
/// toca archivos que casan el prefijo exacto `.<base>.boza-tmp.` en el directorio
/// del destino — nunca datos reales.
fn limpiar_temporales_viejos(dst: &Path) {
    let (Some(padre), Some(prefijo)) = (dst.parent(), prefijo_temporal(dst)) else {
        return;
    };
    let Ok(entradas) = fs::read_dir(padre) else {
        return;
    };
    for e in entradas.flatten() {
        if let Some(nombre) = e.file_name().to_str() {
            if nombre.starts_with(&prefijo) {
                fs::remove_file(e.path()).ok();
            }
        }
    }
}

/// Ruta de un temporal hermano del destino (mismo directorio y fs, para que el
/// `rename` sea atómico). Única por proceso y por llamada.
fn ruta_temporal(dst: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    // Base RECORTADA (misma que usa el barrido) para que el nombre del temporal
    // no pase de 255 bytes: un origen con nombre casi máximo desbordaba al
    // añadirle el sufijo `.boza-tmp.<pid>.<n>` (ENAMETOOLONG → no se copiaba).
    let base = base_temporal(dst);
    dst.with_file_name(format!(".{base}.boza-tmp.{}.{n}", std::process::id()))
}

/// Estampa permisos y mtime en `dst` a partir de una metadata YA LEÍDA del
/// origen. Recibe el snapshot (no re-lee) a propósito: así se estampa el mtime
/// de ANTES de copiar, no el de después —si el origen se reescribió durante la
/// copia, re-leer aquí grabaría el mtime nuevo sobre un contenido viejo/mezclado
/// (ver la guarda de cambio-durante-la-copia en `copiar_uno`)—.
fn copiar_metadatos(meta: &fs::Metadata, dst: &Path, opts: &Opciones) -> Result<()> {
    if opts.preservar_permisos {
        fs::set_permissions(dst, meta.permissions()).ok();
    }
    if opts.preservar_tiempo {
        if let Ok(mtime) = meta.modified() {
            filetime::set_file_mtime(dst, filetime::FileTime::from_system_time(mtime)).ok();
        }
    }
    Ok(())
}

fn recrear_enlace(orig: &Path, dst: &Path) -> Result<()> {
    let destino_del_enlace = fs::read_link(orig)?;
    if dst.exists() || dst.symlink_metadata().is_ok() {
        fs::remove_file(dst).ok();
    }
    if let Some(padre) = dst.parent() {
        fs::create_dir_all(padre)?;
    }
    plataforma::crear_symlink(&destino_del_enlace, dst)
        .with_context(|| format!("no se pudo recrear el enlace {dst:?}"))?;
    Ok(())
}

/// ¿El destino ya es igual al origen y puede omitirse? Tamaños distintos =>
/// difiere seguro, en los dos modos. Luego, según [`Deteccion`]:
///
/// - `TamanoFecha`: tamaño + fecha iguales bastan para asumir igual sin hashear
///   (como `rsync -a`). Puede engañarse con un archivo del mismo tamaño y fecha
///   pero distinto contenido (bit-rot en sitio); es la contrapartida de velocidad
///   de rsync, y detectarlo es trabajo de `boza-scrub`.
/// - `Contenido`: compara por HASH. El destino se lee del PLATO
///   (`descartar_cache`) para no comparar la RAM consigo misma — una corrupción
///   en sitio que aún viva en caché pasaría inadvertida si se leyera de RAM.
fn no_difiere(orig: &Path, dst: &Path, deteccion: Deteccion) -> Result<bool> {
    let (mo, md) = (fs::metadata(orig)?, fs::metadata(dst)?);
    if mo.len() != md.len() {
        return Ok(false);
    }
    if deteccion == Deteccion::TamanoFecha {
        if let (Ok(to), Ok(td)) = (mo.modified(), md.modified()) {
            // Iguales DENTRO DE LA VENTANA (ver VENTANA_MTIME): el redondeo del
            // mtime al cruzar de sistema de archivos (ext4→NTFS, etc.) desvía la
            // fecha unos ns/ms aunque la copia sea fiel; comparar con `==` haría
            // que el salto no dispare nunca cross-FS.
            let dif = to.duration_since(td).unwrap_or_else(|e| e.duration());
            if dif <= VENTANA_MTIME {
                return Ok(true);
            }
        }
        // Sin fecha fiable, se cae a comparar por contenido (lado seguro): es
        // mejor releer de más que omitir sin ninguna base (directiva 20).
    }
    plataforma::descartar_cache(dst);
    Ok(hash_archivo(orig)? == hash_archivo(dst)?)
}

/// Destino EFECTIVO de una copia carpeta→carpeta (`Ruta`→`Ruta` en la GUI): si el
/// origen es una carpeta CON nombre, la copia la recrea DENTRO del destino
/// (`destino/<nombre>`), como un gestor de archivos; una raíz de unidad (`X:\` en
/// Windows, `/` o un punto de montaje sin `file_name` en Linux) no tiene nombre, así
/// que su CONTENIDO se clona directo en el destino (semántica «entre dos unidades»).
///
/// Vive en el núcleo (D9): los dos frontales —`boza-egui` en Windows y `boza-gtk` en
/// Linux— comparten exactamente esta regla, y aquí se prueba en AMBAS plataformas en
/// vez de solo en una.
pub fn subcarpeta_destino(origen: &Path, destino: &Path) -> PathBuf {
    match origen.file_name() {
        Some(nombre) => destino.join(nombre),
        None => destino.to_path_buf(),
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    // Par discriminante (D33): una carpeta CON nombre se recrea dentro del destino;
    // una raíz sin nombre clona su contenido. Un mutante que "siempre une" rompe el
    // segundo caso; uno que "nunca une" rompe el primero. Los literales de ruta van
    // por plataforma para que el caso "sin nombre" use una raíz REAL en cada SO.
    #[test]
    fn subcarpeta_con_nombre_se_recrea_en_destino() {
        #[cfg(windows)]
        let (o, d, esp) = (r"C:\datos\proj", r"D:\backup", r"D:\backup\proj");
        #[cfg(unix)]
        let (o, d, esp) = ("/datos/proj", "/mnt/backup", "/mnt/backup/proj");
        assert_eq!(
            subcarpeta_destino(Path::new(o), Path::new(d)),
            PathBuf::from(esp),
            "una carpeta con nombre tiene que aparecer dentro del destino"
        );
    }

    #[test]
    fn subcarpeta_raiz_clona_el_contenido() {
        #[cfg(windows)]
        let (o, d) = (r"X:\", r"D:\backup");
        #[cfg(unix)]
        let (o, d) = ("/", "/mnt/backup");
        assert_eq!(
            subcarpeta_destino(Path::new(o), Path::new(d)),
            PathBuf::from(d),
            "una raíz sin nombre no añade subcarpeta (clona su contenido)"
        );
    }
    use crate::verificacion::{verificar_arbol, verificar_arbol_con};
    use std::fs;

    fn arbol(dir: &Path) {
        fs::create_dir_all(dir.join("sub/hondo")).unwrap();
        fs::write(dir.join("a.txt"), b"contenido A").unwrap();
        fs::write(dir.join("sub/b.txt"), vec![7u8; 3000]).unwrap();
        fs::write(dir.join("sub/hondo/c.bin"), vec![0u8; 100_000]).unwrap();
        std::os::unix::fs::symlink("a.txt", dir.join("enlace")).unwrap();
    }

    fn nada(_: &Progreso) {}

    // AGUJA de extremo a extremo: copia un árbol real y la VERIFICACIÓN
    // independiente lo confirma idéntico.
    #[test]
    fn copia_fiel_y_verificada() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        let dst = d.path().join("copia");
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert_eq!(res.copiados, 3, "tres archivos");
        assert_eq!(res.verificados, 3, "los tres verificados");
        assert!(res.fallos.is_empty(), "sin fallos: {:?}", res.fallos);
        assert_eq!(verificar_arbol(o.path(), &dst), vec![]);
        // el enlace se recreó como enlace
        assert!(dst
            .join("enlace")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }

    // El origen queda INTACTO tras copiar (nunca se borra ni se toca).
    #[test]
    fn no_toca_el_origen() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        copiar_arbol(o.path(), d.path(), &Opciones::default(), &mut nada).unwrap();
        assert_eq!(fs::read(o.path().join("a.txt")).unwrap(), b"contenido A");
        assert!(o.path().join("sub/b.txt").exists());
    }

    // SiDistinto: una segunda pasada no recopia nada (todo idéntico) y sí cuando
    // un archivo cambió. Pareja: primera copia vs. segunda omisión.
    #[test]
    fn si_distinto_omite_lo_igual_y_recopia_lo_cambiado() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        let dst = d.path().join("c");
        let r1 = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert_eq!(r1.copiados, 3);
        let r2 = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert_eq!(r2.copiados, 0, "nada cambió");
        assert_eq!(r2.omitidos, 3);
        // cambiar uno y repetir: se recopia solo ese
        fs::write(o.path().join("a.txt"), b"CONTENIDO A2").unwrap();
        let r3 = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert_eq!(r3.copiados, 1, "solo el que cambió");
        assert_eq!(r3.omitidos, 2);
    }

    // SEGURIDAD: rechaza copiar el árbol dentro de sí mismo (recursión infinita)
    // y a la misma ruta. Es el sesgo a cerrar de la directiva 21.
    #[test]
    fn rechaza_rutas_peligrosas() {
        let o = tempfile::tempdir().unwrap();
        arbol(o.path());
        // destino dentro del origen
        assert!(copiar_arbol(
            o.path(),
            &o.path().join("sub/dentro"),
            &Opciones::default(),
            &mut nada
        )
        .is_err());
        // misma ruta
        assert!(copiar_arbol(o.path(), o.path(), &Opciones::default(), &mut nada).is_err());
    }

    // CONCILIO (opus+fable, 9/10): un SYMLINK preexistente en el destino NO debe
    // desviar la escritura fuera del árbol. Con el bug, el archivo caía en `fuera`
    // (a través del enlace) y el origen podía ser pisado; con el arreglo, el
    // enlace se deshace y el directorio real se crea en el destino.
    #[test]
    fn symlink_en_el_destino_no_desvia_la_escritura_fuera_del_arbol() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        let fuera = tempfile::tempdir().unwrap();
        fs::create_dir(o.path().join("sub")).unwrap();
        fs::write(o.path().join("sub/b.txt"), b"NUEVO").unwrap();
        let dst = d.path().join("c");
        fs::create_dir_all(&dst).unwrap();
        // el destino ya tiene `sub` como SYMLINK a un directorio FUERA del árbol
        std::os::unix::fs::symlink(fuera.path(), dst.join("sub")).unwrap();

        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert!(res.fallos.is_empty(), "fallos: {:?}", res.fallos);
        // la escritura NO escapó por el enlace
        assert!(
            !fuera.path().join("b.txt").exists(),
            "la escritura escapó del árbol por el symlink del destino"
        );
        // el destino ahora tiene `sub` como directorio REAL con el archivo dentro
        assert!(dst
            .join("sub")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_dir());
        assert_eq!(fs::read(dst.join("sub/b.txt")).unwrap(), b"NUEVO");
    }

    // CONCILIO (sonnet+fable, 9/10): un subdirectorio ILEGIBLE en el origen no
    // debe abortar el clonado entero; se copia el resto y se registra el fallo.
    #[test]
    fn un_dir_ilegible_en_el_origen_no_aborta_copia_el_resto() {
        use std::os::unix::fs::PermissionsExt;
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        fs::write(o.path().join("a.txt"), b"A").unwrap();
        fs::create_dir(o.path().join("prohibido")).unwrap();
        fs::write(o.path().join("prohibido/x.txt"), b"x").unwrap();
        fs::write(o.path().join("z.txt"), b"Z").unwrap();
        fs::set_permissions(
            o.path().join("prohibido"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        // si corremos como root, chmod 000 no impide leer: el caso rojo no se da
        let ilegible_de_verdad = fs::read_dir(o.path().join("prohibido")).is_err();

        let dst = d.path().join("c");
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();

        // los archivos legibles SÍ se copiaron, pese al subdir ilegible
        assert!(dst.join("a.txt").exists() && dst.join("z.txt").exists());
        assert!(res.copiados >= 2, "copiados: {}", res.copiados);
        if ilegible_de_verdad {
            assert!(
                !res.fallos.is_empty(),
                "el subdir ilegible debe quedar registrado"
            );
        }
        // restaurar permisos para que TempDir pueda limpiar
        fs::set_permissions(
            o.path().join("prohibido"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    // CONCILIO (opus obs, 7): sobrescritura ATÓMICA. Si la copia de un archivo
    // falla al reescribir un destino que YA existe, el destino anterior debe
    // conservarse. Discrimina contra el código viejo (borrar dst y luego copiar):
    // se hace ILEGIBLE el ORIGEN, así el borrado viejo del destino tendría éxito
    // y la copia posterior fallaría, dejando el destino perdido; el nuevo escribe
    // primero en un temporal y solo hace `rename` si la copia salió bien.
    #[test]
    fn una_sobrescritura_fallida_conserva_el_destino_anterior() {
        use std::os::unix::fs::PermissionsExt;
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        fs::write(o.path().join("a.txt"), b"NUEVO").unwrap();
        fs::set_permissions(o.path().join("a.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        // OJO (concilio): esta prueba discrimina bajo USUARIO NORMAL; corrida como
        // ROOT, chmod 000 no impide leer, así que `ilegible_de_verdad` es false y
        // las aserciones se saltan (el test pasa sin probar). El runner solo-local
        // corre como usuario (uid 1000), no root, así que ahí sí discrimina.
        let ilegible_de_verdad = fs::read(o.path().join("a.txt")).is_err();

        let dst = d.path().join("c");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("a.txt"), b"VIEJO-IMPORTANTE").unwrap();

        let opts = Opciones {
            sobrescribir: Sobrescritura::Siempre,
            ..Opciones::default()
        };
        let res = copiar_arbol(o.path(), &dst, &opts, &mut nada).unwrap();
        fs::set_permissions(o.path().join("a.txt"), fs::Permissions::from_mode(0o644)).ok();

        if ilegible_de_verdad {
            assert!(
                !res.fallos.is_empty(),
                "el archivo ilegible debía registrar el fallo"
            );
            assert_eq!(
                fs::read(dst.join("a.txt")).unwrap(),
                b"VIEJO-IMPORTANTE",
                "el destino anterior tiene que conservarse tras un fallo de copia"
            );
            let temporales = fs::read_dir(&dst)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains("boza-tmp"))
                .count();
            assert_eq!(temporales, 0, "no debe quedar ningún temporal a medias");
        }
    }

    // CONCILIO (opus+sonnet obs, 6): un temporal HUÉRFANO de una corrida muerta
    // se barre al volver a copiar; no queda basura que `verificar_arbol` trate
    // como parte del árbol.
    #[test]
    fn barre_temporales_huerfanos_al_recopiar() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        fs::write(o.path().join("a.txt"), b"A").unwrap();
        let dst = d.path().join("c");
        fs::create_dir_all(&dst).unwrap();
        // huérfano de una corrida anterior (proceso muerto antes del rename)
        let huerfano = dst.join(".a.txt.boza-tmp.99999.0");
        fs::write(&huerfano, b"basura a medias").unwrap();

        copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();

        assert!(!huerfano.exists(), "el temporal huérfano debía barrerse");
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"A");
        // no quedó NINGÚN boza-tmp
        let restos = fs::read_dir(&dst)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("boza-tmp"))
            .count();
        assert_eq!(restos, 0);
    }

    // El progreso llega a completarse: al final, hechos == totales.
    #[test]
    fn el_progreso_llega_al_total() {
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        arbol(o.path());
        let mut ultimo = Progreso::default();
        let mut cb = |p: &Progreso| ultimo = p.clone();
        copiar_arbol(o.path(), &d.path().join("c"), &Opciones::default(), &mut cb).unwrap();
        assert_eq!(ultimo.archivos_hechos, ultimo.archivos_totales);
        assert_eq!(ultimo.archivos_totales, 3);
    }

    // [A] El hash del origen calculado AL VUELO durante la copia coincide con el
    // hash leído del archivo aparte. Si divergiera, toda verificación mentiría.
    #[test]
    fn el_hash_al_vuelo_coincide_con_el_del_archivo() {
        let d = tempfile::tempdir().unwrap();
        let orig = d.path().join("f.bin");
        let tmp = d.path().join(".f.bin.tmp");
        // Contenido no trivial (cruza varias vueltas de un buffer pequeño).
        let datos: Vec<u8> = (0..250_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&orig, &datos).unwrap();
        let mut buf = vec![0u8; 64 << 10];
        let (n, h) = copiar_en_flujo(
            &orig,
            &tmp,
            &mut buf,
            0,
            &mut Progreso::default(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(n, datos.len() as u64);
        assert_eq!(
            h,
            hash_archivo(&orig).unwrap(),
            "hash al vuelo == hash real"
        );
        assert_eq!(fs::read(&tmp).unwrap(), datos, "el temporal es copia fiel");
    }

    // [C] PAREJA: el verificador de plato dice SÍ al hash correcto y NO al
    // incorrecto. Es la sonda que discrimina: sin el caso rojo, un verificador
    // que dijera "sí" a todo pasaría igual. (La lectura DESDE el plato con
    // FADV_DONTNEED no se puede probar en tmpfs; aquí se prueba que discrimina.)
    #[test]
    fn el_verificador_de_plato_discrimina() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x");
        fs::write(&f, b"contenido real").unwrap();
        let bueno = hash_archivo(&f).unwrap();
        let malo = blake3::hash(b"otra cosa");
        assert!(
            verificar_del_plato(&f, &bueno).unwrap(),
            "verde: hash correcto"
        );
        assert!(
            !verificar_del_plato(&f, &malo).unwrap(),
            "ROJO: hash distinto NO debe pasar"
        );
    }

    // [D] PAREJA de `no_difiere`: MISMO tamaño y fecha pero CONTENIDO distinto.
    // Con `Deteccion::TamanoFecha` (rsync) se OMITE confiando en tamaño+fecha; con
    // `Deteccion::Contenido` se detecta la diferencia y NO se omite. El caso rojo
    // (Contenido) falla por la vía real: comparar hash.
    #[test]
    fn tamano_fecha_confia_contenido_no() {
        use filetime::{set_file_mtime, FileTime};
        let d = tempfile::tempdir().unwrap();
        let (o, dst) = (d.path().join("o"), d.path().join("d"));
        fs::write(&o, b"AAAAAAAA").unwrap(); // 8 bytes
        fs::write(&dst, b"BBBBBBBB").unwrap(); // 8 bytes, distinto contenido
        let t = FileTime::from_unix_time(1_600_000_000, 0);
        set_file_mtime(&o, t).unwrap();
        set_file_mtime(&dst, t).unwrap(); // misma fecha

        assert!(
            no_difiere(&o, &dst, Deteccion::TamanoFecha).unwrap(),
            "TamanoFecha: tamaño+fecha iguales => se omite"
        );
        assert!(
            !no_difiere(&o, &dst, Deteccion::Contenido).unwrap(),
            "Contenido: el contenido difiere => NO se omite"
        );
    }

    // VENTANA_MTIME (= --modify-window de rsync): el redondeo del mtime al cruzar
    // de FS (ext4→NTFS, medido 77 ns) desviaría la fecha y mataría el salto. Par
    // D33 que discrimina la ventana: con tamaño igual y contenido DISTINTO (para
    // que el rescate por contenido no enmascare), DENTRO de la ventana se OMITE y
    // FUERA no. Un mutante que use `==` en vez de la ventana pone rojo el primero.
    #[test]
    fn ventana_mtime_absorbe_el_redondeo_cross_fs() {
        use filetime::{set_file_mtime, FileTime};
        let d = tempfile::tempdir().unwrap();
        let (o, dst) = (d.path().join("o"), d.path().join("d"));
        fs::write(&o, b"AAAAAAAA").unwrap();
        fs::write(&dst, b"BBBBBBBB").unwrap(); // mismo tamaño, distinto contenido
        set_file_mtime(&o, FileTime::from_unix_time(1_600_000_000, 500)).unwrap();
        // 1 s de desvío (< 2 s de ventana): como el redondeo cross-FS, se omite.
        set_file_mtime(&dst, FileTime::from_unix_time(1_600_000_001, 500)).unwrap();
        assert!(
            no_difiere(&o, &dst, Deteccion::TamanoFecha).unwrap(),
            "dentro de la ventana: tamaño+fecha ~iguales => se omite (como rsync)"
        );
        // 10 s de desvío (> ventana): ya NO es redondeo; cae a contenido y difiere.
        set_file_mtime(&dst, FileTime::from_unix_time(1_600_000_010, 500)).unwrap();
        assert!(
            !no_difiere(&o, &dst, Deteccion::TamanoFecha).unwrap(),
            "fuera de la ventana: cae a contenido, que difiere => NO se omite"
        );

        // DIRECCIÓN que importa en producción: el ORIGEN más nuevo que el destino
        // (se editó DESPUÉS del respaldo). Un mutante ciego a este sentido
        // —p.ej. `td.duration_since(to)`— omitiría PARA SIEMPRE todo archivo
        // editado de igual tamaño (pérdida de datos). El valor absoluto tiene que
        // valer en los dos sentidos (hallazgo fable, concilio 2026-10-03).
        set_file_mtime(&o, FileTime::from_unix_time(1_600_000_010, 500)).unwrap();
        set_file_mtime(&dst, FileTime::from_unix_time(1_600_000_000, 500)).unwrap();
        assert!(
            !no_difiere(&o, &dst, Deteccion::TamanoFecha).unwrap(),
            "ORIGEN 10 s más nuevo (> ventana): NO se omite (se recopia)"
        );
        // Origen 1 s más nuevo (< ventana): dentro de tolerancia => se omite.
        set_file_mtime(&o, FileTime::from_unix_time(1_600_000_001, 500)).unwrap();
        assert!(
            no_difiere(&o, &dst, Deteccion::TamanoFecha).unwrap(),
            "ORIGEN 1 s más nuevo (< ventana): se omite (simetría de la ventana)"
        );

        // BORDE EXACTO (desvío == VENTANA_MTIME = 2 s): el contrato es `<=`, así
        // que se OMITE. Un mutante `<` en vez de `<=` pone rojo este aserto
        // (hallazgo haiku). origen 2 s más nuevo que destino, mismo tamaño.
        set_file_mtime(&o, FileTime::from_unix_time(1_600_000_002, 0)).unwrap();
        set_file_mtime(&dst, FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
        assert!(
            no_difiere(&o, &dst, Deteccion::TamanoFecha).unwrap(),
            "borde exacto (== 2 s): se omite (contrato <=)"
        );
    }

    // HALLAZGO fable (concilio 2026-10-03): un archivo VIVO reescrito en sitio
    // DURANTE la copia dejaba el destino torcido y —con el default TamanoFecha— la
    // re-copia lo omitía para siempre con parte limpio. La guarda compara la
    // metadata del origen antes/después y lo informa como FALLO (D20). Caso rojo
    // por la vía real: se reescribe el origen desde el callback de progreso, a
    // mitad de copia. Mutante que lo pone rojo: quitar la guarda `bail!`.
    #[test]
    fn origen_que_cambia_durante_la_copia_es_fallo_no_verde() {
        use std::cell::Cell;
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        // Archivo > buffer (4 MiB en mismo-fs) para que el callback dispare a
        // mitad de lectura: 12 MiB.
        let grande = o.path().join("vivo.bin");
        fs::write(&grande, vec![b'A'; 12 << 20]).unwrap();

        let reescrito = Cell::new(false);
        let mut cb = |p: &Progreso| {
            // `bytes_hechos > 0` = la LECTURA ya empezó (progreso intra-archivo),
            // así que el snapshot `meta_antes` de `copiar_uno` ya se tomó: ahora
            // reescribir el ORIGEN en sitio (mismo tamaño, distinto contenido)
            // desgarra la copia. Simula la VM/base viva guardada durante el clon.
            if p.fase == Fase::Copiando && p.bytes_hechos > 0 && !reescrito.get() {
                let _ = fs::write(&grande, vec![b'B'; 12 << 20]);
                reescrito.set(true);
            }
        };
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut cb).unwrap();
        assert!(reescrito.get(), "el callback tuvo que dispararse a mitad");
        assert_eq!(res.copiados, 0, "una copia torcida NO cuenta como copiada");
        assert_eq!(
            res.fallos.len(),
            1,
            "el cambio durante la copia se informa como fallo: {:?}",
            res.fallos
        );
        assert!(
            !dst.join("vivo.bin").exists(),
            "el destino torcido NO se publica (rename no ocurre)"
        );
    }

    // [B] Cruce de LOTE: más archivos que LOTE_ARCHIVOS. Prueba que el lote
    // parcial final se vacía y verifica — si no, `verificados` se quedaría corto.
    #[test]
    fn cruza_el_lote_y_verifica_todos() {
        let o = tempfile::tempdir().unwrap();
        let n = LOTE_ARCHIVOS + 88; // fuerza un vaciado a mitad + un parcial final
        for i in 0..n {
            fs::write(o.path().join(format!("f{i}")), format!("dato {i}")).unwrap();
        }
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert_eq!(res.copiados, n, "copiados todos");
        assert_eq!(
            res.verificados, n,
            "verificados TODOS (lote parcial incluido)"
        );
        assert!(res.fallos.is_empty(), "sin fallos: {:?}", res.fallos);
        assert_eq!(verificar_arbol(o.path(), &dst), vec![], "árbol idéntico");
    }

    // Modo SIN verificar: copia fiel igual (ruta reflink/kernel) y no cuenta
    // verificados. Ejercita el `syncfs` final del camino no-verificar.
    #[test]
    fn sin_verificar_copia_fiel_y_no_verifica() {
        let o = tempfile::tempdir().unwrap();
        arbol(o.path());
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        let opts = Opciones {
            verificar: false,
            ..Default::default()
        };
        let res = copiar_arbol(o.path(), &dst, &opts, &mut nada).unwrap();
        assert_eq!(res.copiados, 3);
        assert_eq!(res.verificados, 0, "sin verificar no cuenta verificados");
        assert_eq!(verificar_arbol(o.path(), &dst), vec![], "aun así es fiel");
    }

    // Fija el contrato de los dos ejes para una corrupción EN SITIO de igual
    // tamaño+mtime (el caso del hallazgo 1 del concilio 2026-09-10). Es el par
    // D33: el mismo escenario rojo visto por los dos modos de `Deteccion`, cada
    // uno fallando/omitiendo por SU vía real.
    //
    // Antes, `verificar=true` forzaba comparación por contenido en la re-copia:
    // seguro, pero releía TODO el destino (medido 13× más lento) aunque no
    // reescribiera un byte. El rediseño separó la DECISIÓN de omitir (`Deteccion`)
    // de la verificación de lo copiado (`verificar`): la auditoría de bit-rot del
    // destino es trabajo de `Deteccion::Contenido` o de `boza-scrub`, no de cada
    // copia. El contrato nuevo, documentado y probado:
    fn corrupcion_en_sitio() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
        use filetime::{set_file_mtime, FileTime};
        let o = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        fs::create_dir_all(&dst).unwrap();
        // MISMO tamaño (17), DISTINTO contenido, MISMA fecha.
        fs::write(o.path().join("f"), b"DATOS-BUENOS-1234").unwrap();
        fs::write(dst.join("f"), b"DATOS-PODRIDOS-99").unwrap();
        let t = FileTime::from_unix_time(1_600_000_000, 0);
        set_file_mtime(o.path().join("f"), t).unwrap();
        set_file_mtime(dst.join("f"), t).unwrap();
        (o, d, dst)
    }

    // CASO ROJO (Deteccion::Contenido): el modo de auditoría DETECTA la corrupción
    // y recopia; el destino queda FIEL. Falla por la vía real: comparar hash.
    #[test]
    fn deteccion_contenido_detecta_corrupcion_en_sitio() {
        let (o, _d, dst) = corrupcion_en_sitio();
        let opts = Opciones {
            deteccion: Deteccion::Contenido,
            ..Default::default()
        };
        let res = copiar_arbol(o.path(), &dst, &opts, &mut nada).unwrap();
        assert_eq!(res.copiados, 1, "Contenido recopia el archivo corrupto");
        assert_eq!(res.omitidos, 0);
        assert_eq!(
            verificar_arbol(o.path(), &dst),
            vec![],
            "el destino queda FIEL tras recopiar"
        );
        assert_eq!(fs::read(dst.join("f")).unwrap(), b"DATOS-BUENOS-1234");
    }

    // PAREJA (default = Deteccion::TamanoFecha): la re-copia OMITE por tamaño+fecha
    // sin leer contenido — barata, como rsync -a. No detecta la corrupción en
    // sitio (eso es trabajo de boza-scrub), y lo omitido NO se cuenta como
    // `verificados`: el parte no afirma haberlo comprobado (D20, "verde" honesto).
    // Discrimina que el default es el camino rápido y que su límite es DELIBERADO.
    #[test]
    fn default_tamano_fecha_omite_sin_releer_y_no_lo_cuenta_verificado() {
        let (o, _d, dst) = corrupcion_en_sitio();
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert_eq!(res.omitidos, 1, "default: omite por tamaño+fecha");
        assert_eq!(res.copiados, 0);
        assert_eq!(
            res.verificados, 0,
            "lo omitido NO se cuenta como verificado (no se comprobó su contenido)"
        );
        // El límite es deliberado: el contenido corrupto permanece. Auditarlo es
        // Deteccion::Contenido o boza-scrub.
        assert_eq!(fs::read(dst.join("f")).unwrap(), b"DATOS-PODRIDOS-99");
    }

    // HALLAZGO 2 (concilio: sonnet) — un error AJENO a mitad de árbol
    // (create_dir_all falla porque el destino ya es un ARCHIVO con ese nombre) NO
    // debe abortar la copia con `?`: antes se saltaba el vaciado final del lote y
    // los archivos ya renombrados quedaban sin syncfs. Ahora se acumula el fallo
    // y se sigue → la copia termina, los otros archivos quedan fieles.
    #[test]
    fn un_error_a_mitad_de_arbol_no_aborta_ni_salta_el_vaciado() {
        let o = tempfile::tempdir().unwrap();
        fs::create_dir_all(o.path().join("sub")).unwrap();
        fs::write(o.path().join("a.txt"), b"AAA").unwrap();
        fs::write(o.path().join("z.txt"), b"ZZZ").unwrap();
        fs::write(o.path().join("sub/dentro.txt"), b"DDD").unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        fs::create_dir_all(&dst).unwrap();
        // Trampa: en el destino, `sub` ya existe como ARCHIVO → create_dir_all falla.
        fs::write(dst.join("sub"), b"soy un archivo, no un dir").unwrap();

        // NO debe ser Err (antes abortaba); debe volver Ok acumulando el fallo.
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert!(
            !res.fallos.is_empty(),
            "el fallo del dir debe quedar registrado"
        );
        // Los archivos del nivel raíz se copiaron y son fieles pese al error.
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"AAA");
        assert_eq!(fs::read(dst.join("z.txt")).unwrap(), b"ZZZ");
        assert!(res.copiados >= 2, "copiados >= 2: {}", res.copiados);
    }

    // Observación opus (conf. 4) — `Nunca` + symlink preexistente en destino: se
    // deja como está, no se copia por encima (antes se quitaba el symlink y luego
    // se copiaba, incumpliendo "Nunca reescribe").
    #[test]
    fn nunca_respeta_un_symlink_preexistente() {
        let o = tempfile::tempdir().unwrap();
        fs::write(o.path().join("f"), b"NUEVO").unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        fs::create_dir_all(&dst).unwrap();
        std::os::unix::fs::symlink("/otro/sitio", dst.join("f")).unwrap();

        let opts = Opciones {
            sobrescribir: Sobrescritura::Nunca,
            ..Default::default()
        };
        let res = copiar_arbol(o.path(), &dst, &opts, &mut nada).unwrap();
        assert_eq!(res.copiados, 0, "Nunca no copia sobre lo existente");
        assert_eq!(res.omitidos, 1);
        let md = dst.join("f").symlink_metadata().unwrap();
        assert!(md.file_type().is_symlink(), "el symlink sigue intacto");
    }

    // EXCLUSIONES — CASO ROJO/VERDE: los cachés regenerables NO se copian, lo real
    // sí, y la verificación FILTRADA da limpio mientras la sin filtrar los ve como
    // Falta (prueba que la poda actúa igual en copia y en verificación).
    #[test]
    fn excluye_cachas_regenerables() {
        let o = tempfile::tempdir().unwrap();
        fs::create_dir_all(o.path().join("target/deps")).unwrap();
        fs::create_dir_all(o.path().join("sub/node_modules")).unwrap();
        fs::write(o.path().join("a.txt"), b"REAL").unwrap();
        fs::write(o.path().join("sub/util.txt"), b"util").unwrap();
        fs::write(o.path().join("target/deps/basura.o"), vec![0u8; 5000]).unwrap();
        fs::write(o.path().join("sub/node_modules/x.js"), b"junk").unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        // La poda es EXPLÍCITA (el default de la librería ya no la trae).
        let opts = Opciones {
            exclusiones: exclusiones_por_defecto(),
            ..Default::default()
        };

        let res = copiar_arbol(o.path(), &dst, &opts, &mut nada).unwrap();
        assert!(dst.join("a.txt").exists() && dst.join("sub/util.txt").exists());
        assert!(!dst.join("target").exists(), "target NO se copia");
        assert!(
            !dst.join("sub/node_modules").exists(),
            "node_modules NO se copia"
        );
        assert_eq!(res.copiados, 2, "solo los 2 archivos reales");
        // La poda es VISIBLE: 2 nodos excluidos (target/ y node_modules/), con sus
        // rutas registradas para el parte.
        assert_eq!(res.excluidos, 2, "cuenta los 2 nodos podados");
        assert_eq!(res.rutas_excluidas.len(), 2);
        // verificación filtrada: limpia; sin filtrar: los excluidos figuran como Falta.
        assert_eq!(
            verificar_arbol_con(o.path(), &dst, &opts.exclusiones),
            vec![]
        );
        assert!(
            !verificar_arbol(o.path(), &dst).is_empty(),
            "sin la poda, los excluidos se verían como Falta"
        );
    }

    // EXCLUSIONES — PAREJA: sin exclusiones, el caché SÍ se copia (discrimina que
    // la poda es lo que lo excluye, no otra cosa).
    #[test]
    fn sin_exclusiones_se_copia_todo() {
        let o = tempfile::tempdir().unwrap();
        fs::create_dir_all(o.path().join("target")).unwrap();
        fs::write(o.path().join("target/x.o"), b"junk").unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        let opts = Opciones {
            exclusiones: vec![],
            ..Default::default()
        };
        copiar_arbol(o.path(), &dst, &opts, &mut nada).unwrap();
        assert!(
            dst.join("target/x.o").exists(),
            "sin exclusiones, target se copia"
        );
    }

    // MEDIR excluye: el TOTAL del progreso (de `medir`) no cuenta lo podado; si
    // no, la barra no llegaría al 100% o saltaría. Cierra el hunk "SIN CASO ROJO"
    // que señaló el concilio (nadie probaba que el conteo excluyera).
    #[test]
    fn medir_no_cuenta_lo_excluido() {
        let o = tempfile::tempdir().unwrap();
        fs::create_dir_all(o.path().join("target")).unwrap();
        fs::write(o.path().join("real.txt"), vec![1u8; 1000]).unwrap();
        fs::write(o.path().join("target/basura.o"), vec![0u8; 9000]).unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        let opts = Opciones {
            exclusiones: exclusiones_por_defecto(),
            ..Default::default()
        };
        let mut totales = (0usize, 0u64);
        let mut cb = |p: &Progreso| {
            if p.archivos_totales > 0 {
                totales = (p.archivos_totales, p.bytes_totales);
            }
        };
        copiar_arbol(o.path(), &dst, &opts, &mut cb).unwrap();
        assert_eq!(
            totales,
            (1, 1000),
            "el total EXCLUYE target/ (1 archivo/1000 B, no 2/10000)"
        );
    }

    // `.git` NO es caché regenerable sino HISTORIA: debe respaldarse por defecto.
    // Este test fija la regla para que nadie lo re-añada a las exclusiones.
    #[test]
    fn git_se_respalda_por_defecto() {
        assert!(
            !exclusiones_por_defecto().iter().any(|e| e == ".git"),
            ".git NUNCA en las exclusiones por defecto (es historia, no caché)"
        );
        let o = tempfile::tempdir().unwrap();
        fs::create_dir_all(o.path().join(".git")).unwrap();
        fs::write(o.path().join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert!(
            dst.join(".git/HEAD").exists(),
            ".git se respalda por defecto"
        );
    }

    // PROGRESO intra-archivo + FASES: un archivo grande produce VARIOS avances de
    // Copiando con bytes crecientes DENTRO del archivo (antes solo 1 por archivo),
    // y las fases pasan por Midiendo → … → Verificando → Hecho.
    #[test]
    fn progreso_intra_archivo_y_fases() {
        let o = tempfile::tempdir().unwrap();
        fs::write(o.path().join("grande.bin"), vec![7u8; 12 << 20]).unwrap(); // 12 MiB
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        let mut avances: Vec<(Fase, u64)> = Vec::new();
        {
            let mut cb = |p: &Progreso| avances.push((p.fase, p.bytes_hechos));
            copiar_arbol(o.path(), &dst, &Opciones::default(), &mut cb).unwrap();
        }
        let copiando: Vec<u64> = avances
            .iter()
            .filter(|(f, _)| *f == Fase::Copiando)
            .map(|(_, b)| *b)
            .collect();
        let total = 12u64 << 20;
        assert!(
            copiando.iter().any(|&b| b > 0 && b < total),
            "debe haber un avance PARCIAL dentro del archivo; puntos: {copiando:?}"
        );
        let fases: Vec<Fase> = avances.iter().map(|(f, _)| *f).collect();
        assert_eq!(fases.first(), Some(&Fase::Midiendo), "empieza midiendo");
        assert!(fases.contains(&Fase::Verificando), "pasa por verificar");
        assert_eq!(fases.last(), Some(&Fase::Hecho), "termina en Hecho");
    }

    // NOMBRE LARGO (~250 chars): el temporal `.<base>.boza-tmp.<pid>.<n>` desbordaba
    // el límite de 255 bytes de ext4 (ENAMETOOLONG) y el archivo NO se copiaba
    // (visto en producción: una imagen de hacktricks con nombre patológico). Con la
    // base recortada, el temporal cabe y el archivo se copia y verifica.
    #[test]
    fn nombre_casi_maximo_se_copia() {
        let o = tempfile::tempdir().unwrap();
        // 250 chars: deja <5 para el sufijo -> sin recorte, el temporal pasa de 255.
        let nombre = "x".repeat(250);
        fs::write(o.path().join(&nombre), b"CONTENIDO REAL").unwrap();
        fs::write(o.path().join("normal.txt"), b"otro").unwrap();
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("copia");
        let res = copiar_arbol(o.path(), &dst, &Opciones::default(), &mut nada).unwrap();
        assert!(
            res.fallos.is_empty(),
            "el nombre largo NO debe fallar: {:?}",
            res.fallos
        );
        assert_eq!(res.copiados, 2);
        assert_eq!(fs::read(dst.join(&nombre)).unwrap(), b"CONTENIDO REAL");
        assert_eq!(verificar_arbol(o.path(), &dst), vec![], "copia fiel");
    }

    // El temporal recortado sigue casando el barrido de huérfanos (si divergiera,
    // un temporal huérfano de un nombre largo no se limpiaría jamás).
    #[test]
    fn el_barrido_casa_el_temporal_de_nombre_largo() {
        let o = tempfile::tempdir().unwrap();
        let nombre = "y".repeat(250);
        let dst = o.path().join(&nombre);
        // Un huérfano con el MISMO esquema (base recortada) debe barrerse.
        let base_rec = base_temporal(&dst);
        let huerfano = o.path().join(format!(".{base_rec}.boza-tmp.123.0"));
        fs::write(&huerfano, b"basura").unwrap();
        assert!(huerfano.exists());
        limpiar_temporales_viejos(&dst);
        assert!(
            !huerfano.exists(),
            "el huérfano de nombre largo debe barrerse"
        );
    }
}
