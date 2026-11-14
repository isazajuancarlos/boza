// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Interfaz de escritorio de boza en **egui** (reemplaza la GTK de Linux y la
//! Win32 nativa de Windows por un solo frontal portable, un `.exe` sin runtime).
//!
//! Reusa el motor tal cual —`boza_nucleo::copiar_arbol`— y, para discos Linux
//! (ext4/btrfs/xfs) que Windows no entiende, `boza_montaje`, que los monta por
//! **cuchavira** (RW efímero por construcción, D1) y copia con el mismo motor. El
//! tema lo pone `huitaca`, compartido con chuspa y con la web (los tokens de
//! `boi`). Aquí SOLO está la pantalla: toda la lógica de copia y de riesgo (no
//! tocar el origen, verificar, rechazar rutas anidadas, RW efímero) vive en los
//! crates probados.
//!
//! La copia corre en un HILO aparte para no congelar la ventana; el avance viaja
//! por un canal y despierta el repintado con `ctx.request_repaint()`.

// Sin consola: es una app GUI. Sin esto, Windows abre un símbolo del sistema
// detrás de la ventana. En otras plataformas el atributo se ignora.
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use boza_montaje::{copiar_a_particion, copiar_desde_particion};
use boza_nucleo::{
    copiar_arbol, copiar_items, exclusiones_por_defecto, subcarpeta_destino, unidades_montadas,
    Deteccion, Fase, Opciones, Progreso, Sobrescritura, Unidad,
};
use cuchavira_cliente::{Cuchavira, DiscoMontable};
use eframe::egui;
use egui::{vec2, CornerRadius, Margin, RichText, Stroke};
use huitaca::Paleta;

/// Nombre con el que boza se identifica en el log de auditoría de cuchavira
/// (columna `app=`). Fijo: así una lectura del log distingue quién montó qué.
const APP: &str = "boza";

/// De dónde (o hacia dónde) se copia. Un lado puede ser una carpeta/unidad de
/// Windows (una letra, o una subcarpeta elegida), o una partición Linux que
/// cuchavira sabe montar. No se mezcla con cadenas sueltas: el tipo hace imposible
/// pedir una copia a medio elegir, y distingue el camino directo del montado.
#[derive(Clone, PartialEq)]
enum Fuente {
    /// Nada elegido todavía.
    Ninguna,
    /// Una ruta Windows directa (raíz de unidad `X:\` o carpeta elegida).
    Ruta(PathBuf),
    /// Varios ARCHIVOS y/o carpetas sueltos (selección múltiple). Solo como ORIGEN:
    /// cada uno se copia por su nombre dentro del destino. Vacío = nada elegido.
    Items(Vec<PathBuf>),
    /// Una partición Linux de cuchavira. Solo se ofrecen las que TIENEN etiqueta:
    /// cuchavira monta por LABEL, y sin ella no hay forma de nombrarla (D20: no se
    /// inventa una).
    Particion(DiscoMontable),
}

impl Fuente {
    fn es_ninguna(&self) -> bool {
        matches!(self, Fuente::Ninguna) || matches!(self, Fuente::Items(v) if v.is_empty())
    }

    /// Texto para el selector y el resumen.
    fn descripcion(&self) -> String {
        match self {
            Fuente::Ninguna => "(sin elegir)".to_string(),
            Fuente::Ruta(p) => p.display().to_string(),
            Fuente::Items(v) if v.len() == 1 => v[0].display().to_string(),
            Fuente::Items(v) => {
                let nombres: Vec<String> = v
                    .iter()
                    .take(3)
                    .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                    .collect();
                let mas = if v.len() > 3 {
                    format!(" … (+{})", v.len() - 3)
                } else {
                    String::new()
                };
                format!("{} elementos: {}{mas}", v.len(), nombres.join(", "))
            }
            Fuente::Particion(d) => {
                let label = d.label.as_deref().unwrap_or("(sin etiqueta)");
                format!("[Linux] {label} — {} · {}", d.fs, d.dev)
            }
        }
    }

    /// `true` si es una partición que se montará en ESCRITURA al ser destino: eso
    /// es lo que exige el diálogo de confirmación RW.
    fn es_particion(&self) -> bool {
        matches!(self, Fuente::Particion(_))
    }
}

/// Mensajes del hilo de copia hacia la interfaz.
enum Mensaje {
    Avance(Progreso),
    Terminado {
        copiados: usize,
        omitidos: usize,
        verificados: usize,
        excluidos: usize,
        rutas_excluidas: Vec<PathBuf>,
        fallos: Vec<(PathBuf, String)>,
    },
    Error(String),
}

/// Resultado de una tarea lenta de cuchavira (preparar/liberar), que corre en un
/// hilo aparte porque adjunta discos por WSL y dispara UAC.
enum CuchaviraMsg {
    Preparado(Vec<DiscoMontable>),
    Liberado,
    Error(String),
}

/// En qué punto está la pantalla.
enum Estado {
    Listo,
    Copiando(Progreso),
    /// Terminó bien: el parte ya formateado (copiados/omitidos/verificados, la
    /// poda y los fallos). La poda y los fallos NO se callan (D20).
    Hecho(String),
    Error(String),
}

struct App {
    /// Unidades Windows con letra (equivalente del desplegable de la GTK).
    unidades: Vec<Unidad>,
    /// Particiones Linux que cuchavira ve conectadas (vacío si no hay cuchavira,
    /// p. ej. en Linux o sin la appliance arrancada).
    particiones: Vec<DiscoMontable>,
    /// Por qué no hay particiones, si listar falló: se MUESTRA, no se calla —así
    /// «no hay discos Linux» se distingue de «cuchavira no respondió» (D20/D30).
    aviso_particiones: Option<String>,
    /// Cliente de cuchavira, identificándose como `boza` en el log de auditoría.
    cuchavira: Cuchavira,

    origen: Fuente,
    destino: Fuente,

    verificar: bool,
    excluir_caches: bool,
    reverificar_contenido: bool,
    /// 0 = SiDistinto, 1 = Siempre, 2 = Nunca.
    sobrescritura: usize,

    estado: Estado,
    /// Modal de confirmación de montaje RW pendiente (destino = partición Linux).
    confirmar_rw: bool,
    copiando: bool,
    rx: Option<Receiver<Mensaje>>,
    /// Una tarea de cuchavira (preparar/liberar) está en curso en un hilo aparte.
    cuchavira_ocupada: bool,
    rx_cuchavira: Option<Receiver<CuchaviraMsg>>,
    /// Hay discos Linux montados por cuchavira en ESTA sesión → al cerrar boza se
    /// liberan primero (mostrando la pantalla de progreso).
    hay_preparados: bool,
    /// El usuario pidió cerrar y estamos resolviendo lo pendiente (una operación en
    /// curso, o liberar los discos) antes de salir: el cierre se vetó hasta terminar.
    cerrando: bool,
    /// Ya se lanzó el `liberar` del cierre (una sola vez: si falla, se cierra igual,
    /// no se reintenta en bucle).
    cierre_libero: bool,
    /// Texto de la pantalla de progreso de cuchavira (preparar/liberar).
    cuchavira_fase: String,
    /// Particiones Linux montadas por `preparar`, como `(letra, label)` — p. ej.
    /// `("Y:", "fijos")`. Para saber, al copiar HACIA una letra, que es un disco
    /// Linux montado RO y hay que remontarlo RW efímero durante la copia.
    linux_montados: Vec<(String, String)>,
    /// Si la copia actual escribe a un disco Linux (RO), su LABEL: el hilo lo
    /// remonta RW antes de copiar y lo devuelve a RO al terminar (escritura efímera).
    remontar_label: Option<String>,
    /// Fotogramas que quedan por insistir en maximizar al arrancar (como chuspa):
    /// winit no acepta el comando `Maximized` en el frame 0, así que se repite unos
    /// pocos. Arrancar la ventana en estado NO maximizado y maximizar por comando
    /// (en vez de `with_maximized(true)`) es lo que hace la transición de verdad en
    /// Windows — el estado-ya-true dejaría el comando en no-op con la ventana chica.
    frames_maximizar: u8,
}

impl App {
    fn new() -> Self {
        let cuchavira = cliente_cuchavira();
        let mut app = Self {
            unidades: unidades_montadas().unwrap_or_default(),
            particiones: Vec::new(),
            aviso_particiones: None,
            cuchavira,
            origen: Fuente::Ninguna,
            destino: Fuente::Ninguna,
            verificar: true,
            excluir_caches: true,
            reverificar_contenido: false,
            sobrescritura: 0,
            estado: Estado::Listo,
            confirmar_rw: false,
            copiando: false,
            rx: None,
            cuchavira_ocupada: false,
            rx_cuchavira: None,
            hay_preparados: false,
            cerrando: false,
            cierre_libero: false,
            cuchavira_fase: String::new(),
            linux_montados: Vec::new(),
            remontar_label: None,
            frames_maximizar: 3,
        };
        // Al ARRANCAR no se toca cuchavira (ni se lista ni se monta): la detección y
        // el montaje los hace el botón «Preparar discos Linux», en un solo paso. Así
        // no hay flash de consola ni trabajo doble al iniciar (pedido de Juan).
        app.aviso_particiones = Some(
            "Los discos Linux (ext4/btrfs/xfs) no se montan al arrancar. Pulsa «Preparar discos \
             Linux»: detecta, excluye los que Windows puede leer, y monta los demás como unidades."
                .to_string(),
        );
        app
    }

    /// (Re)lista las particiones Linux de cuchavira. Distingue TRES estados y los
    /// dice (D20/D30: un «no hay discos» tiene que distinguirse de «cuchavira no
    /// respondió»): error al invocar, respondió pero no hay nada conectado, o hay
    /// particiones (algunas quizá sin etiqueta, que no son montables por nombre).
    fn refrescar_particiones(&mut self) {
        match self.cuchavira.particiones() {
            Ok(v) => {
                // Para el camino RW-efímero (escribir a un backup Linux) solo sirven
                // las particiones CON etiqueta y AÚN SIN MONTAR: una ya montada por
                // «Preparar» aparece como letra de Windows (arriba), no aquí, para no
                // duplicarla ni chocar con el montaje RO (montar RW lo que ya está
                // RO fallaría).
                let montables: Vec<DiscoMontable> = v
                    .iter()
                    .filter(|d| d.label.is_some() && d.montado_en.is_none())
                    .cloned()
                    .collect();
                let montadas = v.iter().filter(|d| d.montado_en.is_some()).count();
                self.particiones = montables;
                // Hay discos Linux montados por cuchavira → al cerrar boza se liberan
                // (y se muestran como unidades). Cubre también los sobrantes de una
                // sesión anterior que terminó sin liberar.
                self.hay_preparados = montadas > 0;
                self.aviso_particiones = if montadas > 0 {
                    Some(format!(
                        "{montadas} partición(es) Linux montada(s) y disponibles como unidades (solo \
                         lectura). Se liberan al cerrar boza, o con «Liberar»."
                    ))
                } else if self.particiones.is_empty() {
                    Some(
                        "cuchavira responde. Los discos Linux se montan SOLO al pulsar «Preparar \
                         discos Linux» (no al arrancar); aparecerán como unidades."
                            .to_string(),
                    )
                } else {
                    None
                };
            }
            Err(e) => {
                self.particiones.clear();
                self.hay_preparados = false;
                self.aviso_particiones = Some(format!(
                    "No se pudo invocar cuchavira ({e}). Ponlo en el PATH, junto a boza.exe, o \
                     en BOZA_CUCHAVIRA. Mientras tanto, solo se copia entre unidades de Windows."
                ));
            }
        }
    }

    /// Lanza `cuchavira preparar` en un hilo aparte (es LENTO: adjunta discos por
    /// WSL y dispara UAC, así que no puede bloquear la interfaz). Al terminar,
    /// refresca las unidades para que las particiones Linux montadas aparezcan como
    /// letras.
    fn preparar_discos(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel::<CuchaviraMsg>();
        self.rx_cuchavira = Some(rx);
        self.cuchavira_ocupada = true;
        self.cuchavira_fase = "Preparando discos Linux…".into();
        self.aviso_particiones =
            Some("Preparando discos Linux… autoriza el UAC de cuchavira (puede tardar).".into());
        let cuchavira = self.cuchavira.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let msg = match cuchavira.preparar() {
                Ok(v) => CuchaviraMsg::Preparado(v),
                Err(e) => CuchaviraMsg::Error(format!("{e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Lanza `cuchavira liberar` en un hilo aparte (lento + UAC): desmonta y
    /// desconecta lo que `preparar` dejó listo.
    fn liberar_discos(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel::<CuchaviraMsg>();
        self.rx_cuchavira = Some(rx);
        self.cuchavira_ocupada = true;
        self.cuchavira_fase = "Liberando discos Linux…".into();
        self.aviso_particiones =
            Some("Liberando discos Linux… autoriza el UAC de cuchavira.".into());
        let cuchavira = self.cuchavira.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let msg = match cuchavira.liberar() {
                Ok(()) => CuchaviraMsg::Liberado,
                Err(e) => CuchaviraMsg::Error(format!("{e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Recibe el resultado de la tarea de cuchavira y refresca el estado (unidades
    /// de Windows —donde aparecen las particiones ya montadas como letra— y la lista
    /// de particiones aún sin montar para el camino RW).
    fn recibir_cuchavira(&mut self) {
        let Some(rx) = self.rx_cuchavira.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(msg) => {
                self.cuchavira_ocupada = false;
                match msg {
                    CuchaviraMsg::Preparado(v) => {
                        self.unidades = unidades_montadas().unwrap_or_default();
                        self.refrescar_particiones();
                        // Mapa letra→label de lo montado, para la escritura efímera:
                        // al copiar HACIA una de estas letras hay que remontar RW.
                        self.linux_montados = v
                            .iter()
                            .filter_map(|d| match (&d.montado_en, &d.label) {
                                (Some(letra), Some(label)) => Some((letra.clone(), label.clone())),
                                _ => None,
                            })
                            .collect();
                        self.aviso_particiones = Some(if v.is_empty() {
                            "«Preparar» no montó nada: no hay discos Linux puros (los que Windows \
                             puede leer se excluyen por seguridad)."
                                .into()
                        } else {
                            let letras: Vec<String> =
                                v.iter().filter_map(|d| d.montado_en.clone()).collect();
                            format!(
                                "Discos Linux listos en: {}. Aparecen como unidades arriba: elígelos como \
                                 ORIGEN para restaurar, o como DESTINO para hacer backup (se escriben en \
                                 ESCRITURA solo durante la copia). Al terminar, «Liberar discos Linux».",
                                letras.join(", ")
                            )
                        });
                    }
                    CuchaviraMsg::Liberado => {
                        self.unidades = unidades_montadas().unwrap_or_default();
                        self.refrescar_particiones();
                        self.linux_montados.clear();
                        self.aviso_particiones =
                            Some("Discos Linux liberados (desmontados y desconectados).".into());
                    }
                    CuchaviraMsg::Error(e) => {
                        self.aviso_particiones = Some(format!("cuchavira: {e}"));
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => self.rx_cuchavira = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.cuchavira_ocupada = false,
        }
    }

    fn opciones(&self) -> Opciones {
        Opciones {
            verificar: self.verificar,
            preservar_tiempo: true,
            preservar_permisos: true,
            deteccion: if self.reverificar_contenido {
                Deteccion::Contenido
            } else {
                Deteccion::TamanoFecha
            },
            sobrescribir: match self.sobrescritura {
                1 => Sobrescritura::Siempre,
                2 => Sobrescritura::Nunca,
                _ => Sobrescritura::SiDistinto,
            },
            exclusiones: if self.excluir_caches {
                exclusiones_por_defecto()
            } else {
                Vec::new()
            },
        }
    }

    /// Valida la elección y, o bien pide confirmación RW (destino = partición), o
    /// arranca la copia. Devuelve un mensaje de error si la elección no es válida.
    fn intentar_copiar(&mut self, ctx: &egui::Context) {
        if self.origen.es_ninguna() || self.destino.es_ninguna() {
            self.estado = Estado::Error("Elige primero origen y destino.".into());
            return;
        }
        // Partición ↔ partición no está soportado (montaje monta UN lado): se
        // rechaza RUIDOSAMENTE en vez de hacer media copia (D20).
        if self.origen.es_particion() && self.destino.es_particion() {
            self.estado = Estado::Error(
                "Copiar entre dos discos Linux aún no está soportado: elige al menos un lado en Windows."
                    .into(),
            );
            return;
        }
        // Archivos sueltos → partición sin montar no está soportado (el camino de
        // partición copia un árbol, no una lista). Lo normal: preparar el disco y
        // elegirlo como unidad.
        if matches!(self.origen, Fuente::Items(_)) && self.destino.es_particion() {
            self.estado = Estado::Error(
                "Para copiar archivos sueltos a un disco Linux, primero «Preparar discos Linux» y \
                 elígelo como destino (aparece como unidad)."
                    .into(),
            );
            return;
        }
        // ¿El destino es un disco Linux montado por `preparar` (RO)? Entonces la
        // copia tiene que remontarlo RW efímero; se guarda su label para el hilo.
        self.remontar_label = self.label_linux_destino();

        // Escribir a un disco Linux es la única operación que puede dañarlo: se
        // confirma antes — tanto si es una partición (camino montaje) como si es una
        // letra ya preparada que hay que remontar RW. El origen Linux se lee en SOLO
        // LECTURA, sin diálogo (restaurar no escribe en el backup).
        if self.destino.es_particion() || self.remontar_label.is_some() {
            self.confirmar_rw = true;
            return;
        }
        self.arrancar_copia(ctx);
    }

    /// Si el destino es una letra de un disco Linux montado por `preparar`, su LABEL
    /// (hay que remontarlo RW para escribir). Compara la letra de unidad del destino
    /// con el mapa `linux_montados`.
    fn label_linux_destino(&self) -> Option<String> {
        let Fuente::Ruta(d) = &self.destino else {
            return None;
        };
        let s = d.to_string_lossy();
        let letra: String = s.chars().take(2).collect(); // "Y:"
        self.linux_montados
            .iter()
            .find(|(l, _)| l.eq_ignore_ascii_case(&letra))
            .map(|(_, label)| label.clone())
    }

    /// Lanza el hilo de copia. El motor que se usa depende de qué lados son
    /// particiones Linux; todos devuelven el mismo `Resultado`.
    fn arrancar_copia(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel::<Mensaje>();
        self.rx = Some(rx);
        self.copiando = true;
        self.confirmar_rw = false;
        self.estado = Estado::Copiando(Progreso::default());

        let origen = self.origen.clone();
        let destino = self.destino.clone();
        let opts = self.opciones();
        let cuchavira = self.cuchavira.clone();
        let remontar = self.remontar_label.clone();
        let ctx = ctx.clone();

        thread::spawn(move || {
            let ctx_cb = ctx.clone();
            let tx_cb = tx.clone();
            let mut cb = move |p: &Progreso| {
                let _ = tx_cb.send(Mensaje::Avance(p.clone()));
                ctx_cb.request_repaint();
            };

            let resultado = match (&origen, &destino) {
                (Fuente::Ruta(o), Fuente::Ruta(d)) => {
                    // Copiar la CARPETA elegida DENTRO del destino (como un gestor de
                    // archivos): destino/<nombre>. Una raíz de unidad (X:\) no tiene
                    // nombre → se clona su CONTENIDO directo en el destino.
                    let dest = subcarpeta_destino(o, d);
                    match &remontar {
                        // Escritura EFÍMERA a un disco Linux: remontar RW solo para la
                        // copia y volver a RO al terminar (también si falla, D1).
                        Some(label) => match cuchavira.remontar(label, true) {
                            Ok(()) => {
                                let r = copiar_arbol(o, &dest, &opts, &mut cb);
                                let _ = cuchavira.remontar(label, false);
                                r
                            }
                            Err(e) => Err(e),
                        },
                        None => copiar_arbol(o, &dest, &opts, &mut cb),
                    }
                }
                (Fuente::Items(items), Fuente::Ruta(d)) => {
                    // Archivos/carpetas sueltos → destino (cada uno por su nombre).
                    // Si el destino es un disco Linux preparado, remonte RW efímero.
                    match &remontar {
                        Some(label) => match cuchavira.remontar(label, true) {
                            Ok(()) => {
                                let r = copiar_items(items, d, &opts, &mut cb);
                                let _ = cuchavira.remontar(label, false);
                                r
                            }
                            Err(e) => Err(e),
                        },
                        None => copiar_items(items, d, &opts, &mut cb),
                    }
                }
                (Fuente::Ruta(o), Fuente::Particion(p)) => {
                    // Windows → partición Linux (crear/actualizar un backup): RW
                    // efímero. `label` existe: solo se ofrecen particiones con él. La
                    // carpeta de origen se recrea por nombre dentro de la partición.
                    let label = p.label.clone().unwrap_or_default();
                    let sub = o.file_name().map(Path::new);
                    copiar_a_particion(&cuchavira, &label, o, sub, &opts, &mut cb)
                }
                (Fuente::Particion(p), Fuente::Ruta(d)) => {
                    // Partición Linux → Windows (restaurar): SOLO LECTURA. El origen
                    // es la RAÍZ de la partición (sin nombre), así que su contenido va
                    // directo al destino, como clonar una unidad.
                    let label = p.label.clone().unwrap_or_default();
                    copiar_desde_particion(&cuchavira, &label, None, d, &opts, &mut cb)
                }
                _ => Err(anyhow::anyhow!(
                    "combinación de origen/destino no soportada"
                )),
            };

            let msg = match resultado {
                Ok(r) => Mensaje::Terminado {
                    copiados: r.copiados,
                    omitidos: r.omitidos,
                    verificados: r.verificados,
                    excluidos: r.excluidos,
                    rutas_excluidas: r.rutas_excluidas,
                    fallos: r.fallos,
                },
                Err(e) => Mensaje::Error(format!("{e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Vacía el canal del hilo de copia y actualiza el estado de la pantalla. Se
    /// SACA el receptor de `self` para poder mutar `self.estado` sin chocar con su
    /// préstamo; se vuelve a guardar si la copia sigue en curso.
    fn recibir(&mut self) {
        let Some(rx) = self.rx.take() else { return };
        let mut sigue = true;
        loop {
            match rx.try_recv() {
                Ok(Mensaje::Avance(p)) => self.estado = Estado::Copiando(p),
                Ok(Mensaje::Terminado {
                    copiados,
                    omitidos,
                    verificados,
                    excluidos,
                    rutas_excluidas,
                    fallos,
                }) => {
                    self.estado = Estado::Hecho(formatear_parte(
                        copiados,
                        omitidos,
                        verificados,
                        excluidos,
                        &rutas_excluidas,
                        &fallos,
                    ));
                    self.copiando = false;
                    sigue = false;
                    break;
                }
                Ok(Mensaje::Error(e)) => {
                    self.estado = Estado::Error(e);
                    self.copiando = false;
                    sigue = false;
                    break;
                }
                Err(_) => break, // canal vacío (o cerrado): nada más por ahora
            }
        }
        // Mientras no haya llegado el fin, se conserva el receptor para el próximo
        // fotograma; al terminar o fallar se suelta (el hilo ya acabó).
        if sigue {
            self.rx = Some(rx);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        // Maximizar al arrancar (como chuspa): la ventana nace sin maximizar (ver
        // `main`) y el comando de los primeros fotogramas la maximiza de verdad.
        if self.frames_maximizar > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.frames_maximizar -= 1;
            ctx.request_repaint();
        }
        huitaca::aplicar_tema(ctx, huitaca::Modo::Oscuro);
        let p = huitaca::paleta(huitaca::Modo::Oscuro);

        self.recibir();
        self.recibir_cuchavira();

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("boza")
                        .size(30.0)
                        .strong()
                        .color(p.acento),
                );
                ui.label(
                    RichText::new("Copiar datos entre dos unidades — rápido y seguro")
                        .size(13.0)
                        .color(p.suave),
                );
                ui.add_space(16.0);

                // --- Origen y destino ---
                ui.add_enabled_ui(!self.copiando, |ui| {
                    tarjeta_fuente(ui, &p, "Origen", "origen", &mut self.origen, &self.unidades, &self.particiones, true);
                    ui.add_space(10.0);
                    tarjeta_fuente(ui, &p, "Destino", "destino", &mut self.destino, &self.unidades, &self.particiones, false);
                });

                if let Some(aviso) = &self.aviso_particiones {
                    ui.add_space(6.0);
                    ui.label(RichText::new(aviso).size(11.0).color(p.tenue));
                }
                ui.add_space(6.0);
                let libre = !self.copiando && !self.cuchavira_ocupada;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            libre,
                            egui::Button::new(
                                RichText::new("Preparar discos Linux").size(12.0).color(p.acento_texto),
                            )
                            .fill(p.acento)
                            .stroke(Stroke::NONE),
                        )
                        .on_hover_text("Conecta y monta automáticamente los discos Linux (RO). Dispara UAC.")
                        .clicked()
                    {
                        self.preparar_discos(ctx);
                    }
                    if ui
                        .add_enabled(
                            libre,
                            egui::Button::new(RichText::new("Liberar").size(12.0)).fill(p.tarjeta),
                        )
                        .on_hover_text("Desmonta y desconecta lo que «Preparar» dejó listo. Dispara UAC.")
                        .clicked()
                    {
                        self.liberar_discos(ctx);
                    }
                    if ui
                        .add_enabled(
                            libre,
                            egui::Button::new(RichText::new("Actualizar").size(12.0)).fill(p.tarjeta),
                        )
                        .clicked()
                    {
                        self.refrescar_particiones();
                    }
                    if self.cuchavira_ocupada {
                        ui.label(RichText::new("trabajando…").size(12.0).color(p.suave));
                    }
                });

                ui.add_space(14.0);

                // --- Opciones ---
                ui.add_enabled_ui(!self.copiando, |ui| {
                    egui::Frame::new()
                        .fill(p.tarjeta)
                        .corner_radius(CornerRadius::same(huitaca::R as u8))
                        .inner_margin(Margin::same(12))
                        .stroke(Stroke::new(1.0_f32, p.borde))
                        .show(ui, |ui| {
                            ui.checkbox(
                                &mut self.verificar,
                                RichText::new("Verificar por hash tras copiar (más seguro)").size(13.0),
                            );
                            ui.checkbox(
                                &mut self.excluir_caches,
                                RichText::new("Excluir cachés regenerables (target, node_modules, .venv…)").size(13.0),
                            );
                            ui.checkbox(
                                &mut self.reverificar_contenido,
                                RichText::new(
                                    "Re-verificar contenido del destino existente (lento: relee todo; detecta bit-rot en sitio)",
                                )
                                .size(13.0),
                            );
                            ui.add_space(8.0);
                            ui.label(RichText::new("Si el archivo ya existe en el destino:").size(12.0).color(p.suave));
                            egui::ComboBox::from_id_salt("sobrescritura")
                                .selected_text(ETIQUETAS_SOBRE[self.sobrescritura])
                                .show_ui(ui, |ui| {
                                    for (i, t) in ETIQUETAS_SOBRE.iter().enumerate() {
                                        ui.selectable_value(&mut self.sobrescritura, i, *t);
                                    }
                                });
                        });
                });

                ui.add_space(16.0);

                // Transparencia: decir QUÉ va a quedar en el destino, para que no
                // sorprenda (el origen se recrea por su nombre dentro del destino).
                if !self.destino.es_ninguna() {
                    let hint = match &self.origen {
                        Fuente::Ruta(o) => o
                            .file_name()
                            .map(|n| format!("Se creará «{}» dentro del destino.", n.to_string_lossy())),
                        Fuente::Items(v) => Some(format!(
                            "Se copiarán {} elemento(s) dentro del destino (cada uno por su nombre).",
                            v.len()
                        )),
                        _ => None,
                    };
                    if let Some(hint) = hint {
                        ui.label(RichText::new(hint).size(12.0).color(p.suave));
                        ui.add_space(6.0);
                    }
                }

                // --- Botón copiar ---
                let puede = !self.copiando && !self.origen.es_ninguna() && !self.destino.es_ninguna();
                if ui
                    .add_enabled(
                        puede,
                        egui::Button::new(
                            RichText::new(if self.copiando { "Copiando…" } else { "Copiar" })
                                .size(15.0)
                                .color(p.acento_texto),
                        )
                        .fill(p.acento)
                        .stroke(Stroke::NONE)
                        .min_size(vec2(ui.available_width(), 38.0)),
                    )
                    .clicked()
                {
                    self.intentar_copiar(ctx);
                }

                ui.add_space(14.0);

                // --- Progreso / parte ---
                dibujar_estado(ui, &p, &self.estado);
            });
        });

        // Modal de confirmación RW.
        if self.confirmar_rw {
            self.ui_confirmar_rw(ctx, &p);
        }
        // Pantalla de progreso de cuchavira (preparar/liberar), ENCIMA de todo.
        if self.cuchavira_ocupada {
            self.ui_progreso_cuchavira(ctx, &p);
        }

        // Intercepción del cierre: si hay una operación en curso o discos montados,
        // se VETA el cierre, se libera (con su pantalla de progreso) y se cierra al
        // terminar — en vez de cerrar y liberar en silencio. Si no hay nada
        // pendiente, el cierre es normal.
        let quiere_cerrar = ctx.input(|i| i.viewport().close_requested());
        if quiere_cerrar && !self.cerrando && (self.cuchavira_ocupada || self.hay_preparados) {
            self.cerrando = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.cerrando && !self.cuchavira_ocupada {
            if self.hay_preparados && !self.cierre_libero {
                self.cierre_libero = true;
                self.liberar_discos(ctx);
            } else {
                // Liberado (o ya se intentó, o nada que liberar): cerrar de verdad.
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Mientras copia, cuchavira trabaja, o estamos cerrando, repintar de continuo.
        if self.copiando || self.cuchavira_ocupada || self.cerrando {
            ctx.request_repaint();
        }
    }
}

impl App {
    /// Pantalla de progreso mientras cuchavira trabaja (preparar/liberar): un velo
    /// oscuro + tarjeta centrada con spinner, barra animada y el texto de la fase.
    /// Reemplaza la ventana de consola que antes parpadeaba. No es interactiva (el
    /// velo se traga los clics): la operación va en un hilo y no se puede cancelar a
    /// medias sin dejar el disco a medio montar.
    fn ui_progreso_cuchavira(&self, ctx: &egui::Context, p: &Paleta) {
        egui::Area::new(egui::Id::new("cuch_fondo"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                let r = ctx.input(|i| i.content_rect());
                ui.painter()
                    .rect_filled(r, 0.0, egui::Color32::from_black_alpha(180));
                ui.allocate_rect(r, egui::Sense::click_and_drag());
            });
        egui::Window::new("cuch_prog")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::window(&ctx.style_of(egui::Theme::Dark))
                    .fill(p.tarjeta)
                    .stroke(Stroke::new(1.0_f32, p.acento)),
            )
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(14.0);
                    ui.add(egui::Spinner::new().size(40.0).color(p.acento));
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new(&self.cuchavira_fase)
                            .size(16.0)
                            .strong()
                            .color(p.texto),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "Autoriza el UAC de cuchavira si aparece. Puede tardar; no cierres la ventana.",
                        )
                        .size(11.0)
                        .color(p.suave),
                    );
                    ui.add_space(14.0);
                });
            });
    }

    /// Diálogo que confirma montar el destino (partición Linux) en ESCRITURA. Es
    /// la única puerta a una operación que puede dañar un disco; sin este sí, no se
    /// monta en RW. El montaje es efímero: se desmonta al acabar la copia (D1).
    fn ui_confirmar_rw(&mut self, ctx: &egui::Context, p: &Paleta) {
        // Oscurecer el fondo y tragarse sus clics.
        egui::Area::new(egui::Id::new("rw_fondo"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                let r = ctx.input(|i| i.content_rect());
                ui.painter()
                    .rect_filled(r, 0.0, egui::Color32::from_black_alpha(170));
                ui.allocate_rect(r, egui::Sense::click_and_drag());
            });

        let mut confirmar = false;
        let mut cancelar = false;
        egui::Window::new("Montar en escritura")
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::window(&ctx.style_of(egui::Theme::Dark))
                    .fill(p.tarjeta)
                    .stroke(Stroke::new(1.0_f32, p.acento)),
            )
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(
                    RichText::new("Vas a escribir en un disco Linux")
                        .size(16.0)
                        .strong()
                        .color(p.texto),
                );
                ui.add_space(6.0);
                let desc = match &self.remontar_label {
                    Some(label) => format!("{}   (disco Linux «{label}»)", self.destino.descripcion()),
                    None => self.destino.descripcion(),
                };
                ui.label(RichText::new(desc).size(13.0).color(p.acento).monospace());
                ui.add_space(10.0);
                ui.label(
                    RichText::new(
                        "Se pondrá en ESCRITURA solo durante la copia y volverá a solo lectura al \
                         terminar (también si falla). El origen no se toca.",
                    )
                    .size(12.0)
                    .color(p.suave),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Montar y copiar").size(13.0).color(p.acento_texto),
                            )
                            .fill(p.acento)
                            .stroke(Stroke::NONE),
                        )
                        .clicked()
                    {
                        confirmar = true;
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("Cancelar").size(13.0)).fill(p.tarjeta))
                        .clicked()
                    {
                        cancelar = true;
                    }
                });
            });

        if confirmar {
            self.arrancar_copia(ctx);
        }
        if cancelar {
            self.confirmar_rw = false;
        }
    }
}

const ETIQUETAS_SOBRE: [&str; 3] = [
    "Sobrescribir solo si difiere",
    "Sobrescribir siempre",
    "No sobrescribir",
];

/// Una tarjeta "etiqueta + selector de fuente + Examinar…". El selector ofrece las
/// unidades de Windows y las particiones Linux de cuchavira; "Examinar carpeta…" y
/// (solo en el origen) "Examinar archivos…" eligen rutas concretas.
#[allow(clippy::too_many_arguments)]
fn tarjeta_fuente(
    ui: &mut egui::Ui,
    p: &Paleta,
    etiqueta: &str,
    id: &str,
    actual: &mut Fuente,
    unidades: &[Unidad],
    particiones: &[DiscoMontable],
    // Solo el ORIGEN permite elegir archivos sueltos (varios a la vez). El destino
    // es siempre una carpeta/unidad.
    permite_archivos: bool,
) {
    egui::Frame::new()
        .fill(p.tarjeta)
        .corner_radius(CornerRadius::same(huitaca::R as u8))
        .inner_margin(Margin::same(12))
        .stroke(Stroke::new(1.0_f32, p.borde))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(etiqueta).size(14.0).strong().color(p.texto));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("Examinar carpeta…").size(12.0)).fill(p.tarjeta))
                        .clicked()
                    {
                        if let Some(carpeta) = rfd::FileDialog::new()
                            .set_title(format!("Elegir carpeta de {}", etiqueta.to_lowercase()))
                            .pick_folder()
                        {
                            *actual = Fuente::Ruta(carpeta);
                        }
                    }
                    if permite_archivos
                        && ui
                            .add(egui::Button::new(RichText::new("Examinar archivos…").size(12.0)).fill(p.tarjeta))
                            .clicked()
                    {
                        if let Some(archivos) = rfd::FileDialog::new()
                            .set_title("Elegir archivos sueltos (varios con Ctrl/Shift)")
                            .pick_files()
                        {
                            if !archivos.is_empty() {
                                *actual = Fuente::Items(archivos);
                            }
                        }
                    }
                });
            });
            ui.add_space(6.0);
            egui::ComboBox::from_id_salt(id)
                .width(ui.available_width())
                .selected_text(actual.descripcion())
                .show_ui(ui, |ui| {
                    ui.selectable_value(actual, Fuente::Ninguna, "(sin elegir)");
                    if !unidades.is_empty() {
                        ui.label(RichText::new("Unidades de Windows").size(11.0).color(p.tenue));
                        for u in unidades {
                            let etiq = u.etiqueta.as_deref().unwrap_or(&u.nombre);
                            let extra = if u.extraible { " (extraíble)" } else { "" };
                            let fs = u
                                .sistema_archivos
                                .as_deref()
                                .map(|f| format!(" · {f}"))
                                .unwrap_or_default();
                            let texto = format!(
                                "{} — {}{extra} · {}{fs}",
                                u.nombre,
                                etiq,
                                u.tamano,
                            );
                            ui.selectable_value(
                                actual,
                                Fuente::Ruta(u.punto_montaje.clone()),
                                texto,
                            );
                        }
                    }
                    if !particiones.is_empty() {
                        ui.label(RichText::new("Discos Linux (cuchavira)").size(11.0).color(p.tenue));
                        for d in particiones {
                            let label = d.label.as_deref().unwrap_or("(sin etiqueta)");
                            let texto = format!("[Linux] {label} — {} · {}", d.fs, d.dev);
                            ui.selectable_value(actual, Fuente::Particion(d.clone()), texto);
                        }
                    }
                });
            ui.add_space(4.0);
            let (texto, color) = if actual.es_ninguna() {
                (actual.descripcion(), p.tenue)
            } else {
                (actual.descripcion(), p.suave)
            };
            ui.label(RichText::new(texto).size(11.0).color(color).monospace());
        });
}

/// Dibuja la barra de progreso (por fase y bytes) o el parte final.
fn dibujar_estado(ui: &mut egui::Ui, p: &Paleta, estado: &Estado) {
    match estado {
        Estado::Listo => {
            ui.label(RichText::new("Listo.").size(13.0).color(p.suave));
        }
        Estado::Copiando(prog) => {
            match prog.fase {
                Fase::Midiendo => {
                    ui.add(egui::ProgressBar::new(0.0).text("Midiendo el origen…").fill(p.acento));
                    ui.add_space(4.0);
                    ui.label(RichText::new("Midiendo el origen…").size(12.0).color(p.suave));
                }
                _ => {
                    let frac = if prog.bytes_totales > 0 {
                        (prog.bytes_hechos as f64 / prog.bytes_totales as f64).clamp(0.0, 1.0) as f32
                    } else {
                        0.0
                    };
                    let barra = format!(
                        "{:.0}% — {} / {} ({}/{} arch.)",
                        frac * 100.0,
                        tam(prog.bytes_hechos),
                        tam(prog.bytes_totales),
                        prog.archivos_hechos,
                        prog.archivos_totales,
                    );
                    ui.add(egui::ProgressBar::new(frac).text(barra).fill(p.acento));
                    ui.add_space(4.0);
                    let verbo = match prog.fase {
                        Fase::Copiando => "Copiando",
                        Fase::Verificando => "Verificando",
                        Fase::Sincronizando => "Sincronizando al disco…",
                        _ => "Trabajando",
                    };
                    let detalle = if matches!(prog.fase, Fase::Sincronizando) {
                        verbo.to_string()
                    } else {
                        let arch = prog
                            .actual
                            .as_ref()
                            .map(|r| r.display().to_string())
                            .unwrap_or_default();
                        format!("{verbo}: {arch}")
                    };
                    ui.label(RichText::new(detalle).size(12.0).color(p.suave));
                }
            }
        }
        Estado::Hecho(parte) => {
            egui::Frame::new()
                .fill(p.tarjeta)
                .corner_radius(CornerRadius::same(huitaca::R as u8))
                .inner_margin(Margin::same(12))
                .stroke(Stroke::new(1.0_f32, p.borde))
                .show(ui, |ui| {
                    ui.label(RichText::new(parte).size(13.0).color(p.texto));
                });
        }
        Estado::Error(e) => {
            ui.label(
                RichText::new(format!("Error: {e}"))
                    .size(13.0)
                    .color(egui::Color32::from_rgb(0xE0, 0x7B, 0x7B)),
            );
        }
    }
}

/// Arma el parte final: copiados/omitidos/verificados, la poda (sin callarla) y los
/// fallos. Misma información que daba la GTK (contrato D20).
fn formatear_parte(
    copiados: usize,
    omitidos: usize,
    verificados: usize,
    excluidos: usize,
    rutas_excluidas: &[PathBuf],
    fallos: &[(PathBuf, String)],
) -> String {
    let mut s = format!("Listo: {copiados} copiados, {omitidos} omitidos, {verificados} verificados.");
    if excluidos > 0 {
        let muestra: String = rutas_excluidas
            .iter()
            .take(8)
            .map(|r| format!("\n  · {}", r.display()))
            .collect();
        let mas = if rutas_excluidas.len() < excluidos {
            format!("\n  … y {} más", excluidos - rutas_excluidas.len())
        } else {
            String::new()
        };
        s.push_str(&format!(
            "\n{excluidos} ruta(s) EXCLUIDA(S) por filtro:{muestra}{mas}"
        ));
    }
    if !fallos.is_empty() {
        let detalle: String = fallos
            .iter()
            .take(10)
            .map(|(r, m)| format!("\n• {}: {m}", r.display()))
            .collect();
        s.push_str(&format!("\n{} FALLO(S):{detalle}", fallos.len()));
    }
    s
}

/// Construye el cliente de cuchavira apuntando al binario que se encuentre (ver
/// [`ruta_cuchavira`]); si no se encuentra ninguno, se deja `cuchavira` del PATH.
/// Falla con gracia: si al final no está, `particiones()` devuelve error y la
/// pantalla lo DICE (D20), sin impedir la copia entre unidades de Windows.
fn cliente_cuchavira() -> Cuchavira {
    match ruta_cuchavira() {
        Some(ruta) => Cuchavira::nuevo(APP).con_exe(ruta),
        None => Cuchavira::nuevo(APP),
    }
}

/// Localiza `cuchavira.exe`, en orden: (1) la variable `BOZA_CUCHAVIRA` (ruta
/// explícita); (2) JUNTO al propio ejecutable —la disposición de instalación:
/// cuchavira se despliega al lado de boza—; (3) conveniencia de DESARROLLO: el
/// repo hermano `cuchavira/target/{release,debug}` en el disco (en el producto
/// este paso no encuentra nada y se cae al PATH). Devuelve `None` si no hay
/// ninguno: entonces se usa `cuchavira` del PATH.
fn ruta_cuchavira() -> Option<PathBuf> {
    if let Some(ruta) = std::env::var_os("BOZA_CUCHAVIRA") {
        let p = PathBuf::from(ruta);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok();
    let dir = exe.as_deref().and_then(|e| e.parent());
    let nombre = if cfg!(windows) { "cuchavira.exe" } else { "cuchavira" };
    if let Some(dir) = dir {
        let al_lado = dir.join(nombre);
        if al_lado.is_file() {
            return Some(al_lado);
        }
        // Repo hermano en desarrollo (boza/target/<perfil> → ../../../cuchavira/…).
        for rel in [
            "../../../cuchavira/target/release",
            "../../../cuchavira/target/debug",
        ] {
            let candidato = dir.join(rel).join(nombre);
            if candidato.is_file() {
                return Some(candidato);
            }
        }
    }
    None
}

/// Formatea bytes en unidades legibles (base 1024): 1536 → "1,5 MiB".
fn tam(bytes: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", U[i]).replace('.', ",")
    }
}

fn main() -> eframe::Result {
    let mut vp = egui::ViewportBuilder::default()
        .with_inner_size([620.0, 560.0])
        .with_min_inner_size([520.0, 420.0])
        .with_title("boza");
    // Icono de la ventana y la barra de tareas (el del .exe lo pone build.rs).
    if let Ok(icono) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/boza.png")) {
        vp = vp.with_icon(std::sync::Arc::new(icono));
    }
    let opts = eframe::NativeOptions {
        viewport: vp,
        ..Default::default()
    };
    eframe::run_native(
        "boza",
        opts,
        Box::new(|cc| {
            huitaca::aplicar_fuentes(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}
