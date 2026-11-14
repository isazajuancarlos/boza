#!/usr/bin/env bash
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
#
# Empaqueta boza.exe + el runtime GTK4 en una carpeta AUTOCONTENIDA que arranca
# con doble clic. Se corre en la terminal MINGW64 de MSYS2, desde la raíz del repo:
#
#     bash dist/windows/empaquetar.sh
#
# Requiere: compilado ya `cargo build --release --target x86_64-pc-windows-gnu`.
set -euo pipefail

RAIZ="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$RAIZ/target/x86_64-pc-windows-gnu/release"
SALIDA="$RAIZ/dist/windows/boza-portable"
MINGW="${MINGW_PREFIX:-/mingw64}"

[ -f "$BIN/boza.exe" ] || { echo "ERROR: falta $BIN/boza.exe — compila primero"; exit 1; }

echo ">> carpeta limpia: $SALIDA"
rm -rf "$SALIDA"
mkdir -p "$SALIDA"

echo ">> binarios"
cp "$BIN/boza.exe" "$SALIDA/"
[ -f "$BIN/boza-scrub.exe" ] && cp "$BIN/boza-scrub.exe" "$SALIDA/"

echo ">> DLL de las que depende boza.exe (recursivo, solo las de $MINGW)"
# ldd sigue el árbol de dependencias; nos quedamos con las de MSYS2 (no las del
# sistema Windows). Se itera hasta punto fijo por si una DLL arrastra otra.
copiar_dlls() {
  local cambio=1
  while [ "$cambio" = 1 ]; do
    cambio=0
    while IFS= read -r dll; do
      base="$(basename "$dll")"
      if [ ! -f "$SALIDA/$base" ]; then
        cp "$dll" "$SALIDA/"; cambio=1
      fi
    done < <(ldd "$SALIDA"/*.exe "$SALIDA"/*.dll 2>/dev/null \
             | grep -oiE "$MINGW[^ ]*\.dll" | sort -u)
  done
}
copiar_dlls
echo "   $(ls "$SALIDA"/*.dll | wc -l) DLL copiadas"

echo ">> cargadores de imágenes (gdk-pixbuf: PNG/SVG del icono y la UI)"
mkdir -p "$SALIDA/lib"
cp -r "$MINGW/lib/gdk-pixbuf-2.0" "$SALIDA/lib/"

echo ">> esquemas de GSettings (compilados)"
mkdir -p "$SALIDA/share/glib-2.0/schemas"
cp "$MINGW/share/glib-2.0/schemas/gschemas.compiled" "$SALIDA/share/glib-2.0/schemas/" 2>/dev/null \
  || glib-compile-schemas "$MINGW/share/glib-2.0/schemas" \
       --targetdir "$SALIDA/share/glib-2.0/schemas"

echo ">> tema de iconos (Adwaita + hicolor) e icono propio"
mkdir -p "$SALIDA/share/icons"
cp -r "$MINGW/share/icons/Adwaita" "$SALIDA/share/icons/" 2>/dev/null || true
cp -r "$MINGW/share/icons/hicolor" "$SALIDA/share/icons/" 2>/dev/null || true
mkdir -p "$SALIDA/share/icons/hicolor/scalable/apps"
cp "$RAIZ/crates/gtk/dist/boza.svg" "$SALIDA/share/icons/hicolor/scalable/apps/boza.svg"

echo
echo "LISTO: $SALIDA"
echo "Pruébalo con doble clic en boza.exe. Si se queja de un icono o esquema,"
echo "falta una carpeta de share/ — revisa la salida de arriba."
