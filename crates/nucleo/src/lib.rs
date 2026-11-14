// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Motor de copia de boza — clonar/copiar un árbol de archivos entre dos
//! unidades, **rápido** (reflink Copy-on-Write cuando se puede) y **seguro**
//! (verificación por hash, nunca borra el origen, rechaza rutas anidadas).
//!
//! El motor no sabe nada de GTK: es Rust puro y se prueba con directorios
//! temporales. La cáscara gráfica lo llama y dibuja el progreso.

pub mod copia;
pub mod perfil;
/// Primitivos de bajo nivel que difieren entre Linux y Windows (durabilidad,
/// lectura del plato, identidad de archivo, enlaces). El resto del motor es igual.
pub mod plataforma;
pub mod unidades;
pub mod verificacion;

pub use copia::{
    copiar_arbol, copiar_items, copiar_un_archivo, exclusiones_por_defecto, subcarpeta_destino,
    Deteccion, Fase, Opciones, Progreso, Resultado, Sobrescritura,
};
pub use perfil::{planificar, Perfil, PlanCopia};
pub use unidades::{parsear_lsblk, unidades_montadas, Unidad};
pub use verificacion::{verificar_arbol, verificar_arbol_con, Diferencia};
