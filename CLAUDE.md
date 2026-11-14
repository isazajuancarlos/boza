# CLAUDE.md — boza

> **Configuración general: `~/.claude/CLAUDE.md`** — reglas, directivas y
> máquinas. Se carga en toda sesión y **no se copia aquí**; este archivo lleva
> SOLO lo propio de boza.

## Qué es

Aplicación de **escritorio** que clona/copia datos entre **dos unidades**, rápida y
segura, con **dos frontales, uno por plataforma** (Juan, 2026-10-11): `boza-gtk`
(GTK4 nativo) en **Linux** —da el `.deb`— y `boza-egui` (portable, un solo `.exe`)
en **Windows**. `boza` = «dos» en la numeración muisca.
Nombre provisional (lo confirma Juan). Nació el 2026-09-09.
`LicenseRef-Xiliux-Propietario`.

## Arquitectura

Cuatro crates —un motor, dos frontales (uno por plataforma) y el pegamento de
cuchavira—, y la separación es el punto:

| Crate | Qué | Se prueba |
|---|---|---|
| `crates/nucleo` | motor: `copiar_arbol`, `copiar_items`/`copiar_un_archivo` (archivos sueltos), `verificar_arbol`, `subcarpeta_destino` (regla «copiar la carpeta dentro del destino», compartida por los dos frontales). Rust puro, sin UI; capa por plataforma en `plataforma.rs` (Linux/Windows) | sí, con directorios temporales (`tests/items.rs` es cross-platform; `subcarpeta_destino` prueba las dos plataformas; el mod unit de `src` usa symlinks unix) |
| `crates/gtk` | el frontal de **Linux**: GTK4 nativo (`gtk4`), binario `boza`. Da el `.deb` (metadata cargo-deb). Depende de `libgtk-4-dev` en el sistema. En Linux los discos ya están montados (`lsblk`): NO usa cuchavira | GUI se prueba dibujándola (D28, `cargo run -p boza-gtk`); `subcarpeta_destino` y `copiar_items` están probados en el núcleo |
| `crates/egui` | el frontal de **Windows**: `eframe` + tema `huitaca`, **portable**, un solo `.exe` sin runtime. UX completa + selector de discos Linux de cuchavira (preparar/liberar) + escritura efímera + archivos sueltos. Binario `boza`; icono embebido (`build.rs`+`winresource`). En Linux compila pero no se empaqueta (su sitio es Windows) | GUI se prueba dibujándola (D28); humo: lanzar y que sobreviva |
| `crates/montaje` | orquesta el montaje de discos **Linux** (ext4/btrfs/xfs) en Windows vía **cuchavira**: monta (RW efímero RAII), copia con el núcleo, desmonta. Depende de `cuchavira-cliente` (repo cuchavira). Solo lo usa el frontal de Windows | lógica pura (ruta) testeada; flujo e2e con `examples/copiar.rs` (requiere cuchavira+WSL) |

**Historia de los frontales** (crónica en los commits): nacieron un GTK4 (Linux) y un
Win32 nativo (Windows); durante el interludio Windows (2026-10) se escribió `crates/egui`
y el 2026-10-05 se retiraron los dos primeros (`crates/gtk`, `crates/win`). Al volver a
Linux (2026-10-11) Juan fijó el reparto por plataforma: **`boza-gtk` se restauró como el
frontal de Linux y `boza-egui` quedó como el de Windows** (igual que chuspa). `crates/win`
sigue retirado (su pantalla la cubre egui). `BUILD-WINDOWS.md` (receta GTK/MSYS2) está
obsoleto: el frontal de Windows es egui, portable.

**El motor no sabe nada de la UI**: por eso se prueba solo y la lógica de riesgo
(no borrar el origen, verificar, rechazar rutas anidadas) vive donde hay pruebas.

## Integración con cuchavira — discos Linux en Windows (backend, 2026-10-05)

En Windows, un disco ext4/btrfs/xfs (p. ej. el disco de backups) **no es una letra**:
Windows no entiende esos FS. **cuchavira** (`/mnt/data/cuchavira`) lo monta por el kernel
de WSL y lo expone como letra; una vez montado, el motor de boza lo copia como
cualquier unidad — **el motor no cambia**.

- **`crates/montaje`** es el pegamento: `copiar_a_particion` (crear/actualizar un
  backup, monta **RW**) y `copiar_desde_particion` (restaurar, monta **RO**). El
  **RW es efímero por construcción** (D1): el `Montaje` es RAII y se desmonta al
  salir del scope, también si la copia falla — la ventana de escritura dura lo que
  dura la copia, sin depender de acordarse de desmontar.
- **`cuchavira-cliente`** (crate en el repo cuchavira, consumido por path) envuelve
  el CLI de cuchavira. Vive allí a propósito (D36): el parseo depende del contrato
  de salida de cuchavira, y un cambio de contrato debe romper la **compilación**,
  no a boza en silencio.
- **VALIDADO e2e (2026-10-05):** `examples/copiar.rs` copió un árbol Windows a una
  imagen ext4 (RW efímero) con el motor real → hashes idénticos al origen, `e2fsck`
  limpio, desmontado solo. Es el banco del backend (requiere cuchavira+WSL; no corre
  en `cargo test` puro).
- **Seguridad**: cuchavira pide UAC para conectar el disco físico; el montaje RW
  exige confirmación (en la GUI, el diálogo de boza; `cuchavira-cliente` pasa
  `--rw-confirmado` porque la app ya confirmó). Todo montaje queda en el log de
  auditoría de cuchavira con la columna `app=boza`.

## Rápida y segura — las dos promesas, y dónde viven

- **Rápida**: `perfil.rs` deriva el **plan** del par de dispositivos
  (`/sys/dev/block/MAJ:MIN/queue/rotational`): `Reflink` (mismo fs, CoW
  `FICLONE`), `Secuencial` (algún extremo mecánico → 1 hilo, buffer 8 MiB,
  nunca thrashea un plato) o `Paralelo` (ambos SSD, distinto fs → seam listo,
  ejecutor concurrente pendiente D7). Durabilidad por **lotes**: `rename`
  atómico sin `fsync` por archivo, y un `syncfs` por lote (`vaciar_lote`) —
  esto es lo que iguala a `rsync` en árboles de muchos archivos. En modo
  verificar, el hash del origen se calcula **al vuelo** (`copiar_en_flujo`),
  ahorrando una relectura completa del origen.
  **Dos ejes ORTOGONALES** (antes atados en un solo flag, D32): `verificar`
  gobierna si lo que SE COPIA se re-lee del plato y se hashea; `Deteccion`
  gobierna cómo se decide OMITIR lo que ya está. `Deteccion::TamanoFecha`
  (default) = salto incremental por tamaño+fecha como `rsync -a`, con **ventana
  de tolerancia** `VENTANA_MTIME` (2 s, el `--modify-window` de rsync) que
  absorbe el redondeo del mtime al cruzar de FS —ext4 1 ns → NTFS 100 ns, medido
  77 ns; sin ella el salto NO dispara cross-FS y la re-copia rehashea todo—. Lo
  omitido se cuenta como `omitidos`, NO como `verificados` (el parte no afirma
  haberlo comprobado, D20). `Deteccion::Contenido` = compara por hash de ambos
  lados (destino del plato): detecta bit-rot en sitio, a costa de releer el
  destino; es `rsync -c` + auditoría. El scrub dedicado del destino es
  `boza-scrub`.
- **Segura**: verificación por **hash blake3** (no por tamaño/fecha: un byte
  cambiado con el mismo tamaño tiene que verse), leída **del PLATO**
  (`verificar_del_plato` usa `posix_fadvise`/`FADV_DONTNEED` tras el `syncfs`
  para no comparar la RAM consigo misma — la lección del respaldo cifrado);
  **nunca toca el origen**; copia a temporal + `rename` atómico;
  `validar()` rechaza destino⊂origen, origen⊂destino y origen==destino;
  preserva permisos y `mtime`. Los fallos se ACUMULAN y se informan, no
  abortan la copia (un parte que calla lo que falló es falso — directiva 20).
  Un fallo de `syncfs` sí se propaga (es durabilidad real).

**Medido (2026-09-10, nvme→HDD 5400rpm, 1562 MB / 3565 archivos):** `cp`
reporta ~2700 MB/s pero MIENTE (solo caché, no durable); `cp`+`sync` durable =
94 MB/s; boza sin verificar (durable) = 70 MB/s; boza con verificar-de-plato =
33 MB/s, 0 fallos, copia fiel. Que verificar baje de 70 a 33 es el positivo de
control (D30) de que la relectura es del plato, no de caché. (Medido sobre destino
VACÍO = primera copia; en re-corridas el salto rápido cambia el perfil.)

**Concilio 2026-09-10 (4 modelos: opus/sonnet/fable/haiku), 2 hallazgos ≥8
corregidos:** (1) en modo verificar, el salto rápido omitía por tamaño+fecha sin
comprobar contenido → una corrupción en sitio de igual tamaño+mtime pasaba como
«verde» — arreglado: `verificar=true` fuerza comparación por HASH del contenido
(el salto por fecha solo aplica sin verificar); (2) un error ajeno a mitad de
árbol (`create_dir_all` sobre un destino que ya es archivo) abortaba con `?` y se
saltaba el vaciado final del lote, dejando renombrados sin `syncfs` — arreglado:
el fallo se acumula y se sigue (contrato D20). Pares discriminantes con mutante
verificado. `salto_rapido` sigue cableado a `true` en la GTK, pero ya es inocuo
con la casilla «Verificar» activa (default).

**SUPERADO el 2026-10-03 por el eje `Deteccion` (rama `incremental/deteccion-ortogonal`).**
El arreglo del hallazgo 1 ataba la velocidad de la re-copia a `verificar`: con
`verificar=true` (default), decidir si omitir rehasheaba TODO el destino (medido:
re-copia de 2,2 GB a ~8 s releyendo, aunque no reescribía un byte; en un árbol de
400 archivos, 6,45 s vs 0,44 s). Era un flag con dos significados (D32). Ahora
son dos ejes: `verificar` (verificar lo COPIADO) y `Deteccion` (cómo omitir lo
que YA ESTÁ). El default pasa a `TamanoFecha` + `VENTANA_MTIME` (incremental como
rsync, re-copia ext4→NTFS **~32× más rápida**, medido 0,44 s). La preocupación
del concilio (corrupción en sitio de igual tamaño+fecha) **no se pierde**: vive
en `Deteccion::Contenido` y en `boza-scrub`, y lo omitido ya no se reporta como
`verificados` («verde» honesto, D20). Par D33 reescrito con mutante verificado en
las dos vetas (detección de contenido y ventana de mtime).

**Concilio multi-modelo corrido (2026-10-03: sonnet/fable/haiku, coord. opus).**
2 hallazgos ≥8 confirmados y corregidos, + 1 de test:
1. **(fable, 8) Regresión de copia torcida.** Un archivo VIVO reescrito en sitio
   DURANTE la copia (VM, sqlite, contenedor) dejaba el temporal mezclado; como
   `copiar_metadatos` estampaba el mtime de DESPUÉS, con el nuevo default la
   re-copia lo omitía para siempre con parte limpio (el default viejo lo sanaba
   por contenido). Arreglo: snapshot de `fs::metadata(orig)` ANTES, comparar
   tamaño+mtime DESPUÉS → `bail!` (fallo ruidoso, no publica; D20); y se estampa
   el mtime del snapshot PREVIO. Probado e2e: árbol estable 0 fallos; mutante que
   quita la guarda pone rojo el test.
2. **(fable, 8) / (haiku, 9) Banco D33 incompleto.** El par de la ventana solo
   probaba el destino-más-nuevo y no el borde: un mutante ciego a origen-más-nuevo
   (`td.duration_since(to)` → omite para siempre todo archivo editado de igual
   tamaño) y un `<`-por-`<=` pasaban 35/35. Arreglo: casos origen-más-nuevo
   (dentro/fuera de ventana) y borde exacto 2 s; los tres mutantes ahora caen.
3. Observaciones (<8) abiertas: la ventana de 2 s es ~2–4× la de rsync (que
   compara segundos enteros) — se podría estrechar a 1 s o solo-cross-FS; y falta
   test del `descartar_cache` de `no_difiere` (difícil: FADV es advisory).
36 tests verdes, clippy `-D warnings`, compila para Windows. **Mergeado** a
`windows/port-nucleo` (FF) y empujado al espejo el 2026-10-03. La GUI Windows ya
expone la casilla `Contenido` igual que la GTK (no testeable desde Linux, solo
`cargo check --target windows`; se ejerce al correr la GUI en Windows, D28).

## Compilar

**El verde de referencia es Linux** (D del centro). En Linux compilan los cuatro
crates, así que el gate y las pruebas corren sobre el workspace entero:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # el gate
```

El motor solo: `cargo test -p boza-nucleo`.

**Frontal de Linux (`boza-gtk`)** — GTK4 nativo; pide `libgtk-4-dev` en el sistema:

```
cargo build --release -p boza-gtk     # binario `boza`
cargo run -p boza-gtk                  # abrir la ventana (D28: dibujarla)
```

**El `.deb`** (desde Linux) lleva `boza` (de `boza-gtk`) y `boza-scrub` (de
`boza-nucleo`); se construyen antes y se empaqueta sin recompilar:

```
cargo build --release -p boza-gtk -p boza-nucleo --bin boza-scrub --bin boza
cargo deb -p boza-gtk --no-build      # → target/debian/boza_*.deb
```

**Frontal de Windows (`boza-egui`)** — portable, no pide libs de sistema; un solo
`.exe`. En Windows se excluye `boza-gtk` del workspace (arrastra el stack C de GTK):

```
cargo build -p boza-egui              # binario `boza.exe`
cargo clippy -p boza-egui --all-targets -- -D warnings
cargo run -p boza-egui                # abrir la ventana (D28)
```

Los discos Linux (ext4/btrfs/xfs) aparecen en el selector si **cuchavira** está en
el PATH; para apuntar a un binario concreto, `BOZA_CUCHAVIRA=<ruta al .exe>`. Sin
cuchavira la pantalla sigue copiando entre unidades de Windows y lo DICE en un
aviso (no se cuelga ni calla, D20). La copia hacia un disco Linux lo monta **RW
efímero** vía `boza-montaje` tras el diálogo de confirmación; desde un disco Linux
se monta **solo lectura**.

En egui la copia corre en un hilo aparte y el avance viaja por `std::sync::mpsc` +
`ctx.request_repaint()` (no congelar la ventana). El icono del `.exe` lo embebe
`build.rs` + `winresource` (usa `windres` del toolchain GNU).

## Port a Windows (2026-09-18, rama `windows/port-nucleo`)

Juan pidió `.deb` (Debian) **y** `.exe`/`.msi` (Windows). El `.deb` lo da el frontal
de Linux `boza-gtk` (ver «Compilar»); el `.exe` de Windows es `boza-egui`, portable.
Windows exigió portar el motor: los 6 puntos
Linux (`syncfs`, `posix_fadvise`, `st_dev/ino`, `symlink`, `/sys rotational`,
`lsblk`) se aislaron en **`crates/nucleo/src/plataforma.rs`** con `cfg`. Linux
queda IDÉNTICO (34+3 tests verdes); Windows usa el equivalente honesto:
durabilidad por ARCHIVO (`FlushFileBuffers`/`sync_all`) en vez de `syncfs` por
lote, descarte de caché no-op **documentado**, `GetFileInformationByHandle` para
identidad de archivo, `symlink_file/dir`, perfil Secuencial, y `GetLogicalDrives`
para listar unidades (dep `windows-sys`, solo target windows; `libc` pasó a
target unix). **Verificado que el núcleo compila para Windows**
(`cargo check -p boza-nucleo --target x86_64-pc-windows-gnu`, lib+bin). Lo que NO
se puede hacer desde Linux: construir la GUI GTK4 y el instalador — GTK4 arrastra
el stack C de Windows (MSYS2). La receta completa (MSYS2, `empaquetar.sh` del
runtime, Inno Setup `.iss` / WiX) está en **`BUILD-WINDOWS.md`** y `dist/windows/`.
**Falta**: ejecutar la GUI en Windows (D28, dibujar la pantalla) y pasar concilio
al diff del port antes de dar la versión Windows por cerrada. Los tests unix del
núcleo se gatearon con `#[cfg(unix)]`; el resto de la suite en Windows queda como
follow-up (no se puede correr desde aquí).

## Pendiente

- Ejecutar y verificar la GUI en Windows (dibujar la pantalla, symlinks con/sin
  privilegio) — ver `BUILD-WINDOWS.md`.
- Posible modo futuro: clonado a NIVEL DE BLOQUE de una unidad entera (tipo `dd`),
  que es peligroso (pisa el destino completo) y necesita root — iría con su propia
  confirmación fuerte y su verificación. Hoy boza copia a nivel de ARCHIVO.

## Concilio (2026-09-09) — CERRADO a main

Fase 1 mergeada a `main` tras concilio multi-modelo. 2 hallazgos ≥8 corregidos:
symlink en destino desviaba escritura/pisaba origen (se quita el symlink
preexistente + guarda dev/ino); dir ilegible abortaba todo (WalkDir Err se
acumula). Endurecimiento (copia atómica temp+rename, fsync propagado) pasó un
segundo concilio limpio.

### Pendientes (observaciones <8, NO bloqueantes)
- Barrer `.<base>.boza-tmp.*` huérfanos (de un proceso muerto) del destino antes del pase 2.
- Guarda `(dev,ino)` a nivel de árbol (bind mount con Siempre); hoy cubierto a nivel de archivo.
- La prueba de sobrescritura atómica no discrimina bajo root (documentado en el test).

**Exclusiones + progreso real (rama `copia/exclusiones-y-progreso`, 2026-09-10).**
`Opciones.exclusiones` poda subárboles por nombre de componente (`excluido`/`recorrer`,
la MISMA poda en conteo, copia y verificación — `verificar_arbol_con`); por defecto
cachés regenerables (`exclusiones_por_defecto`: target, node_modules, .venv, __pycache__, .cache). `Progreso` gana `fase: Fase` (Midiendo/Copiando/Sincronizando/
Verificando/Hecho) y avisa DENTRO de un archivo grande (progreso intra-archivo en
`copiar_en_flujo`), con la barra por BYTES. La GTK: casilla de exclusiones + barra
por fase con tamaños legibles. Pares D33 con mutante verificado (exclusión y parcial
intra-archivo se ponen rojos).

**Concilio de la rama de exclusiones (2026-09-10, 4 modelos).** Convergente
(sonnet 8 / opus 7 / fable 6): la poda descartaba datos reales con nombre
coincidente SIN señal — «verde» callaba la omisión (D20). Arreglado: `Resultado`
gana `excluidos: usize` + `rutas_excluidas` (contadas en el bucle con
`skip_current_dir`), y la GTK las MUESTRA en el parte final. Además
`Opciones::default()` pasa a exclusiones VACÍAS (fidelidad total en la librería;
la poda es opt-in del llamante — la GTK la enciende con su casilla), cerrando la
mina del `..Default::default()` (obs. opus). Caso rojo de `medir` cerrado
(`medir_no_cuenta_lo_excluido`). Mutantes verificados. Cosmético no arreglado
(consenso): `bytes_base` usa metadata y la barra puede saltar si un archivo
cambia de tamaño durante la copia — no es pérdida de datos (copia/verificación
usan bytes reales).

**Fix temporal de nombre largo (2026-09-10, rama `fix/temporal-nombre-largo`).**
Un origen con nombre casi máximo (~255 B) desbordaba el temporal
`.<base>.boza-tmp.<pid>.<n>` (ENAMETOOLONG en ext4) y NO se copiaba — visto en una
copia real de 153 GiB (imagen de hacktricks con nombre patológico), reportado como
fallo (no silencioso). `base_temporal()` recorta la base a NOMBRE_MAX(255)-1-RESERVA(48)
bytes, respetando fronteras UTF-8, y la usan TANTO `ruta_temporal` como
`prefijo_temporal` (si divergieran, un huérfano de nombre largo no se barrería).
Par D33 con mutante verificado (revertir la base recortada reproduce el error) +
test de que el barrido sigue casando.

**`boza-scrub` — detector de bit-rot por manifiesto (2026-09-10).** Binario en
`crates/nucleo/src/bin/boza-scrub.rs` que reusa `hash_archivo` (blake3) y
`exclusiones_por_defecto` de boza-nucleo. `sellar <dir> <manif>` graba el hash de
cada archivo (leído DEL PLATO con FADV_DONTNEED, mismas exclusiones que la copia);
`verificar <dir> <manif>` re-hashea del plato y compara → CAMBIADO (rot/edición),
FALTA, NUEVO. Modelo de manifiesto (no contra el origen vivo) para que una
discrepancia sea corrupción del disco, no que el origen cambió. Sale exit 1 si hay
CAMBIADO/FALTA/ilegible (D20). El manifiesto se guarda FUERA del árbol (se salta a
sí mismo si está dentro). 3 pruebas (íntegro / byte podrido→rojo / falta→rojo) +
humo real.

**Reanudable (`--reanudar`, 2026-10-31, característica de mindalá #30 vía precizer).**
Hashear del plato un árbol de respaldo grande tarda horas; un barrido cancelado o
cuyo proceso muere no debe reempezar de cero. El checkpoint se vuelca POR LOTES
(`LOTE_PROGRESO=256`; una muerte pierde a lo sumo el último lote, que se re-hashea):
al **sellar**, el manifiesto parcial ES el checkpoint —`--reanudar` salta lo ya
sellado (append); sin el flag sobrescribe, semántica de hoy intacta—; al
**verificar**, un `<manif>.verif-progreso` efímero guarda el veredicto de cada
archivo comprobado y `--reanudar` lo retoma, reconstruyendo el parte. El progreso se
BORRA al terminar el barrido (éxito o rojo): solo sobrevive si el proceso muere. El
cálculo se separó de la presentación (`calcular_verificacion`→`Parte`) para medir el
barrido, no solo el exit code (D16). El banco destapó y cerró un bug real: el
`.verif-progreso` dentro del árbol se verificaba a sí mismo y salía como «nuevo»
espurio (ahora se auto-excluye como el manifiesto).

**El detector no se ciega (puerta de seguridad, 3 hallazgos cerrados el 2026-10-31).**
La revisión automática marcó tres formas de engañar al detector por el checkpoint o
por un nombre hostil; las tres se cerraron (reales, no falsos positivos — boza es
producto de integridad): (1) al retomar solo se confía en registros BIEN FORMADOS
(hash 64-hex / estado ∈ ok|rot|nuevo) y el checkpoint se reescribe limpio, así una
línea torcida por una muerte del proceso no contamina por `append`; un estado
DESCONOCIDO no es «limpio» —se re-verifica— (cerró el `_ => {}` que lo daba por
bueno); (2) un nombre con `\n`/`\t`/`\r` no se puede representar por línea: se REPORTA
como problema (rojo) en vez de partir/inyectar un registro falso; (3) el
`.verif-progreso` se crea con `O_NOFOLLOW` (unix), para no truncar un archivo
apuntado por un symlink plantado en esa ruta. 11 pruebas, **6 mutantes verificados**
por la vía real (sellar-retoma, carga-del-checkpoint, salto-de-re-hasheo,
estado-inválido, nombre-con-control, symlink).
