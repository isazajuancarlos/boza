// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Interfaz GTK4 de boza — el frontal de **Linux** (en Windows el frontal es
//! `boza-egui`). Elegir origen y destino (dos unidades, o varios archivos/carpetas
//! sueltos como origen), opciones, y copiar con barra de progreso. La copia corre
//! en un HILO aparte para no congelar la ventana; el avance viaja por un canal al
//! hilo de la interfaz.
//!
//! Necesita `libgtk-4-dev` para compilar (ver README). Toda la lógica de copia
//! vive en `boza-nucleo`; aquí solo está la pantalla. En Linux los discos ya están
//! montados por el sistema (se listan con `lsblk`): NO se usa cuchavira, que es el
//! puente para montar FS Linux en Windows y solo lo necesita `boza-egui`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;

use boza_nucleo::{
    copiar_arbol, copiar_items, exclusiones_por_defecto, subcarpeta_destino, unidades_montadas,
    Deteccion, Fase, Opciones, Progreso, Sobrescritura, Unidad,
};
use gtk::prelude::*;
use gtk::{glib, Application, ApplicationWindow};

const APP_ID: &str = "com.xiliux.boza";

/// De dónde se copia: nada, una carpeta/unidad (`Ruta`), o varios archivos y/o
/// carpetas sueltos (`Items`, solo como ORIGEN). El tipo hace imposible pedir una
/// copia a medio elegir. (En Linux no hay variante `Particion`: los discos ya están
/// montados; esa es la rama de cuchavira que solo tiene `boza-egui` en Windows.)
#[derive(Clone)]
enum Fuente {
    Ninguna,
    Ruta(PathBuf),
    Items(Vec<PathBuf>),
}

impl Fuente {
    fn es_ninguna(&self) -> bool {
        matches!(self, Fuente::Ninguna) || matches!(self, Fuente::Items(v) if v.is_empty())
    }

    /// Texto para la etiqueta de la fila y el resumen.
    fn descripcion(&self) -> String {
        match self {
            Fuente::Ninguna => "(sin elegir)".to_string(),
            Fuente::Ruta(p) => p.display().to_string(),
            Fuente::Items(v) if v.len() == 1 => v[0].display().to_string(),
            Fuente::Items(v) => {
                let nombres: Vec<String> = v
                    .iter()
                    .take(3)
                    .map(|p| {
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    })
                    .collect();
                let mas = if v.len() > 3 {
                    format!(" … (+{})", v.len() - 3)
                } else {
                    String::new()
                };
                format!("{} elementos: {}{mas}", v.len(), nombres.join(", "))
            }
        }
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

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(construir_ui);
    app.run()
}

fn construir_ui(app: &Application) {
    let origen: Rc<RefCell<Fuente>> = Rc::new(RefCell::new(Fuente::Ninguna));
    let destino: Rc<RefCell<Fuente>> = Rc::new(RefCell::new(Fuente::Ninguna));

    // Unidades montadas detectadas (lsblk): para elegir de un desplegable en vez
    // de teclear rutas. Si lsblk falla, la lista queda vacía y solo se usa "Elegir…".
    let unidades: Rc<Vec<Unidad>> = Rc::new(unidades_montadas().unwrap_or_default());

    let raiz = gtk::Box::new(gtk::Orientation::Vertical, 12);
    raiz.set_margin_top(16);
    raiz.set_margin_bottom(16);
    raiz.set_margin_start(16);
    raiz.set_margin_end(16);

    let titulo = gtk::Label::new(Some("Copiar datos entre dos unidades"));
    titulo.add_css_class("title-2");
    raiz.append(&titulo);

    // --- Origen --- (carpeta/unidad, o varios archivos sueltos)
    let (fila_o, lbl_o, btn_o_carpeta, btn_o_archivos) = fila_origen();
    let dd_o = selector_unidades(&unidades, &lbl_o, origen.clone());
    raiz.append(&dd_o);
    raiz.append(&fila_o);
    // --- Destino --- (siempre una carpeta/unidad)
    let (fila_d, lbl_d, btn_d) = fila_ruta("Destino:", "Elegir destino…");
    let dd_d = selector_unidades(&unidades, &lbl_d, destino.clone());
    raiz.append(&dd_d);
    raiz.append(&fila_d);

    // --- Opciones ---
    let chk_verificar = gtk::CheckButton::with_label("Verificar por hash tras copiar (más seguro)");
    chk_verificar.set_active(true);
    raiz.append(&chk_verificar);

    let chk_exclusiones =
        gtk::CheckButton::with_label("Excluir cachés regenerables (target, node_modules, .venv…)");
    chk_exclusiones.set_active(true);
    raiz.append(&chk_exclusiones);

    // Re-verificar el contenido de lo que YA está en el destino (Deteccion::
    // Contenido). Apagado por defecto: la re-copia decide por tamaño+fecha (como
    // rsync), que es barato. Encenderlo audita bit-rot en sitio a costa de releer
    // el destino entero — para auditorías puntuales; el scrub dedicado es boza-scrub.
    let chk_reverif = gtk::CheckButton::with_label(
        "Re-verificar contenido del destino existente (lento: relee todo; detecta bit-rot en sitio)",
    );
    chk_reverif.set_active(false);
    raiz.append(&chk_reverif);

    let combo = gtk::DropDown::from_strings(&[
        "Sobrescribir solo si difiere",
        "Sobrescribir siempre",
        "No sobrescribir",
    ]);
    raiz.append(&combo);

    // Transparencia: decir QUÉ va a quedar en el destino, para que no sorprenda (una
    // carpeta se recrea por su nombre dentro del destino; los archivos sueltos van
    // cada uno por su nombre). Mismo contrato que boza-egui.
    let resumen = gtk::Label::new(None);
    resumen.set_xalign(0.0);
    resumen.set_wrap(true);
    resumen.add_css_class("dim-label");
    raiz.append(&resumen);

    // --- Progreso ---
    let barra = gtk::ProgressBar::new();
    barra.set_show_text(true);
    raiz.append(&barra);
    let estado = gtk::Label::new(Some("Listo."));
    estado.set_xalign(0.0);
    estado.set_wrap(true);
    raiz.append(&estado);

    let btn_copiar = gtk::Button::with_label("Copiar");
    btn_copiar.add_css_class("suggested-action");
    raiz.append(&btn_copiar);

    let ventana = ApplicationWindow::builder()
        .application(app)
        .title("boza")
        .default_width(560)
        .default_height(400)
        .child(&raiz)
        .build();

    // Cierre que recalcula el resumen de transparencia a partir de origen+destino.
    // Se invoca desde cada sitio que cambia una selección (D20: el resumen nunca
    // miente sobre lo que se va a hacer).
    let refrescar: Rc<dyn Fn()> = {
        let origen = origen.clone();
        let destino = destino.clone();
        Rc::new(move || {
            resumen.set_text(&texto_resumen(&origen.borrow(), &destino.borrow()));
        })
    };

    // Selección de carpetas y archivos sueltos.
    conectar_selector_carpeta(
        &btn_o_carpeta,
        &ventana,
        &lbl_o,
        origen.clone(),
        "Elegir la carpeta/unidad de ORIGEN",
        refrescar.clone(),
    );
    conectar_selector_archivos(
        &btn_o_archivos,
        &ventana,
        &lbl_o,
        origen.clone(),
        refrescar.clone(),
    );
    conectar_selector_carpeta(
        &btn_d,
        &ventana,
        &lbl_d,
        destino.clone(),
        "Elegir la carpeta/unidad de DESTINO",
        refrescar.clone(),
    );
    // Los desplegables de unidades también cambian la selección → refrescar.
    conectar_refresco_dropdown(&dd_o, refrescar.clone());
    conectar_refresco_dropdown(&dd_d, refrescar.clone());

    // Botón copiar.
    btn_copiar.connect_clicked(glib::clone!(
        #[weak] barra,
        #[weak] estado,
        #[weak] btn_copiar,
        #[weak] chk_verificar,
        #[weak] chk_exclusiones,
        #[weak] chk_reverif,
        #[weak] combo,
        #[strong] origen,
        #[strong] destino,
        move |_| {
            let o = origen.borrow().clone();
            let Fuente::Ruta(d) = destino.borrow().clone() else {
                estado.set_text("Elige primero origen y un destino (carpeta/unidad).");
                return;
            };
            if o.es_ninguna() {
                estado.set_text("Elige primero el origen.");
                return;
            }
            let opts = Opciones {
                verificar: chk_verificar.is_active(),
                preservar_tiempo: true,
                preservar_permisos: true,
                deteccion: if chk_reverif.is_active() {
                    Deteccion::Contenido
                } else {
                    Deteccion::TamanoFecha
                },
                sobrescribir: match combo.selected() {
                    1 => Sobrescritura::Siempre,
                    2 => Sobrescritura::Nunca,
                    _ => Sobrescritura::SiDistinto,
                },
                exclusiones: if chk_exclusiones.is_active() {
                    exclusiones_por_defecto()
                } else {
                    Vec::new()
                },
            };

            btn_copiar.set_sensitive(false);
            estado.set_text("Copiando…");
            let (tx, rx) = async_channel::unbounded::<Mensaje>();

            // Hilo de trabajo: la copia no bloquea la interfaz.
            thread::spawn(move || {
                let tx2 = tx.clone();
                let mut cb = move |p: &Progreso| {
                    let _ = tx2.send_blocking(Mensaje::Avance(p.clone()));
                };
                let resultado = match &o {
                    // Una carpeta elegida se recrea DENTRO del destino (destino/<nombre>),
                    // como un gestor de archivos; una raíz de unidad clona su contenido
                    // (subcarpeta_destino, probado en el núcleo).
                    Fuente::Ruta(o) => {
                        let dest = subcarpeta_destino(o, &d);
                        copiar_arbol(o, &dest, &opts, &mut cb)
                    }
                    // Varios archivos/carpetas sueltos → destino (cada uno por su nombre).
                    Fuente::Items(items) => copiar_items(items, &d, &opts, &mut cb),
                    // Inalcanzable: `es_ninguna()` se comprueba antes de lanzar el hilo.
                    Fuente::Ninguna => {
                        let _ = tx.send_blocking(Mensaje::Error("origen vacío".into()));
                        return;
                    }
                };
                match resultado {
                    Ok(r) => {
                        let _ = tx.send_blocking(Mensaje::Terminado {
                            copiados: r.copiados,
                            omitidos: r.omitidos,
                            verificados: r.verificados,
                            excluidos: r.excluidos,
                            rutas_excluidas: r.rutas_excluidas,
                            fallos: r.fallos,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send_blocking(Mensaje::Error(e.to_string()));
                    }
                }
            });

            // Recepción en el hilo de la interfaz.
            glib::spawn_future_local(glib::clone!(
                #[weak] barra,
                #[weak] estado,
                #[weak] btn_copiar,
                async move {
                    while let Ok(msg) = rx.recv().await {
                        match msg {
                            Mensaje::Avance(p) => {
                                match p.fase {
                                    Fase::Midiendo => {
                                        barra.pulse();
                                        barra.set_text(Some("Midiendo el origen…"));
                                        estado.set_text("Midiendo el origen…");
                                    }
                                    _ => {
                                        let frac = if p.bytes_totales > 0 {
                                            p.bytes_hechos as f64 / p.bytes_totales as f64
                                        } else {
                                            0.0
                                        };
                                        barra.set_fraction(frac.clamp(0.0, 1.0));
                                        // La barra: % + volumen real; el estado: la
                                        // FASE y el archivo, para que "por proceso" se
                                        // vea y nunca parezca colgada.
                                        barra.set_text(Some(&format!(
                                            "{:.0}% — {} / {} ({}/{} arch.)",
                                            frac * 100.0,
                                            tam(p.bytes_hechos),
                                            tam(p.bytes_totales),
                                            p.archivos_hechos,
                                            p.archivos_totales,
                                        )));
                                        let arch = p
                                            .actual
                                            .as_ref()
                                            .map(|r| r.display().to_string())
                                            .unwrap_or_default();
                                        let verbo = match p.fase {
                                            Fase::Copiando => "Copiando",
                                            Fase::Verificando => "Verificando",
                                            Fase::Sincronizando => "Sincronizando al disco…",
                                            _ => "Trabajando",
                                        };
                                        if matches!(p.fase, Fase::Sincronizando) {
                                            estado.set_text(verbo);
                                        } else {
                                            estado.set_text(&format!("{verbo}: {arch}"));
                                        }
                                    }
                                }
                            }
                            Mensaje::Terminado { copiados, omitidos, verificados, excluidos, rutas_excluidas, fallos } => {
                                barra.set_fraction(1.0);
                                let mut base = format!(
                                    "Listo: {copiados} copiados, {omitidos} omitidos, {verificados} verificados."
                                );
                                // La poda NO se calla: se dice cuánto se excluyó y
                                // se muestran las primeras rutas, para que una
                                // exclusión errónea salte a la vista.
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
                                    base.push_str(&format!(
                                        "\n{excluidos} ruta(s) EXCLUIDA(S) por filtro:{muestra}{mas}"
                                    ));
                                }
                                if fallos.is_empty() {
                                    estado.set_text(&base);
                                } else {
                                    let detalle: String = fallos
                                        .iter()
                                        .take(10)
                                        .map(|(r, m)| format!("\n• {}: {m}", r.display()))
                                        .collect();
                                    estado.set_text(&format!(
                                        "{base}\n{} FALLO(S):{detalle}",
                                        fallos.len()
                                    ));
                                }
                                btn_copiar.set_sensitive(true);
                            }
                            Mensaje::Error(e) => {
                                estado.set_text(&format!("Error: {e}"));
                                btn_copiar.set_sensitive(true);
                            }
                        }
                    }
                }
            ));
        }
    ));

    // Arrancar maximizada (paridad con boza-egui y chuspa).
    ventana.maximize();
    ventana.present();
}

/// Resumen de transparencia a partir del origen y el destino elegidos.
fn texto_resumen(origen: &Fuente, destino: &Fuente) -> String {
    if destino.es_ninguna() {
        return String::new();
    }
    match origen {
        Fuente::Ruta(o) => o
            .file_name()
            .map(|n| format!("Se creará «{}» dentro del destino.", n.to_string_lossy()))
            .unwrap_or_else(|| "Se clonará el contenido del origen en el destino.".to_string()),
        Fuente::Items(v) if !v.is_empty() => format!(
            "Se copiarán {} elemento(s) dentro del destino (cada uno por su nombre).",
            v.len()
        ),
        _ => String::new(),
    }
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

/// Construye la fila del ORIGEN: "etiqueta | ruta elegida | Elegir carpeta… | Elegir
/// archivos…". El botón de archivos sueltos solo existe en el origen.
fn fila_origen() -> (gtk::Box, gtk::Label, gtk::Button, gtk::Button) {
    let fila = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let lbl_fijo = gtk::Label::new(Some("Origen:"));
    lbl_fijo.set_width_chars(9);
    lbl_fijo.set_xalign(0.0);
    let lbl_ruta = gtk::Label::new(Some("(sin elegir)"));
    lbl_ruta.set_hexpand(true);
    lbl_ruta.set_xalign(0.0);
    lbl_ruta.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let btn_carpeta = gtk::Button::with_label("Elegir carpeta…");
    let btn_archivos = gtk::Button::with_label("Elegir archivos…");
    fila.append(&lbl_fijo);
    fila.append(&lbl_ruta);
    fila.append(&btn_carpeta);
    fila.append(&btn_archivos);
    (fila, lbl_ruta, btn_carpeta, btn_archivos)
}

/// Construye una fila "etiqueta | ruta elegida | botón".
fn fila_ruta(etiqueta: &str, texto_boton: &str) -> (gtk::Box, gtk::Label, gtk::Button) {
    let fila = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let lbl_fijo = gtk::Label::new(Some(etiqueta));
    lbl_fijo.set_width_chars(9);
    lbl_fijo.set_xalign(0.0);
    let lbl_ruta = gtk::Label::new(Some("(sin elegir)"));
    lbl_ruta.set_hexpand(true);
    lbl_ruta.set_xalign(0.0);
    lbl_ruta.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let btn = gtk::Button::with_label(texto_boton);
    fila.append(&lbl_fijo);
    fila.append(&lbl_ruta);
    fila.append(&btn);
    (fila, lbl_ruta, btn)
}

/// Desplegable de unidades montadas. Al elegir una, fija la fuente a la RUTA de su
/// punto de montaje (el botón "Elegir…" sigue sirviendo para bajar a una subcarpeta,
/// o para elegir archivos sueltos en el origen).
fn selector_unidades(
    unidades: &Rc<Vec<Unidad>>,
    etiqueta_ruta: &gtk::Label,
    fuente: Rc<RefCell<Fuente>>,
) -> gtk::DropDown {
    let mut textos = vec!["— unidad montada —".to_string()];
    for u in unidades.iter() {
        let extra = if u.extraible { " (extraíble)" } else { "" };
        let etiq = u.etiqueta.as_deref().unwrap_or(&u.nombre);
        textos.push(format!(
            "{etiq}{extra} — {} · {}{}",
            u.punto_montaje.display(),
            u.tamano,
            u.sistema_archivos
                .as_deref()
                .map(|f| format!(" · {f}"))
                .unwrap_or_default()
        ));
    }
    let refs: Vec<&str> = textos.iter().map(|s| s.as_str()).collect();
    let dd = gtk::DropDown::from_strings(&refs);
    dd.connect_selected_notify(glib::clone!(
        #[weak]
        etiqueta_ruta,
        #[strong]
        fuente,
        #[strong]
        unidades,
        move |dd| {
            let idx = dd.selected() as usize;
            if idx == 0 {
                return; // el placeholder no cambia nada
            }
            if let Some(u) = unidades.get(idx - 1) {
                etiqueta_ruta.set_text(&u.punto_montaje.display().to_string());
                *fuente.borrow_mut() = Fuente::Ruta(u.punto_montaje.clone());
            }
        }
    ));
    dd
}

/// Llama a `refrescar` cuando cambia la selección de un desplegable de unidades (el
/// resumen de transparencia debe seguir a la elección).
fn conectar_refresco_dropdown(dd: &gtk::DropDown, refrescar: Rc<dyn Fn()>) {
    dd.connect_selected_notify(move |_| refrescar());
}

/// Conecta un botón a un `FileDialog` de selección de CARPETA que guarda la ruta.
fn conectar_selector_carpeta(
    boton: &gtk::Button,
    ventana: &ApplicationWindow,
    etiqueta: &gtk::Label,
    fuente: Rc<RefCell<Fuente>>,
    titulo: &str,
    refrescar: Rc<dyn Fn()>,
) {
    let titulo = titulo.to_string();
    boton.connect_clicked(glib::clone!(
        #[weak]
        ventana,
        #[weak]
        etiqueta,
        move |_| {
            let dialog = gtk::FileDialog::builder().title(&titulo).build();
            dialog.select_folder(
                Some(&ventana),
                gtk::gio::Cancellable::NONE,
                glib::clone!(
                    #[strong]
                    fuente,
                    #[weak]
                    etiqueta,
                    #[strong]
                    refrescar,
                    move |res| {
                        if let Ok(archivo) = res {
                            if let Some(ruta) = archivo.path() {
                                etiqueta.set_text(&ruta.display().to_string());
                                *fuente.borrow_mut() = Fuente::Ruta(ruta);
                                refrescar();
                            }
                        }
                    }
                ),
            );
        }
    ));
}

/// Conecta un botón a un `FileDialog` de selección MÚLTIPLE de archivos sueltos
/// (solo tiene sentido como origen). Guarda la lista como `Fuente::Items`.
fn conectar_selector_archivos(
    boton: &gtk::Button,
    ventana: &ApplicationWindow,
    etiqueta: &gtk::Label,
    fuente: Rc<RefCell<Fuente>>,
    refrescar: Rc<dyn Fn()>,
) {
    boton.connect_clicked(glib::clone!(
        #[weak]
        ventana,
        #[weak]
        etiqueta,
        move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Elegir archivos sueltos (varios con Ctrl/Shift)")
                .build();
            dialog.open_multiple(
                Some(&ventana),
                gtk::gio::Cancellable::NONE,
                glib::clone!(
                    #[strong]
                    fuente,
                    #[weak]
                    etiqueta,
                    #[strong]
                    refrescar,
                    move |res| {
                        if let Ok(lista) = res {
                            let rutas: Vec<PathBuf> = (0..lista.n_items())
                                .filter_map(|i| lista.item(i))
                                .filter_map(|o| o.downcast::<gtk::gio::File>().ok())
                                .filter_map(|f| f.path())
                                .collect();
                            if !rutas.is_empty() {
                                let nueva = Fuente::Items(rutas);
                                etiqueta.set_text(&nueva.descripcion());
                                *fuente.borrow_mut() = nueva;
                                refrescar();
                            }
                        }
                    }
                ),
            );
        }
    ));
}
