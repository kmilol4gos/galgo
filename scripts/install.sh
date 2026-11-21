#!/usr/bin/env bash
# Compila e instala galgo sin sudo. PREFIX es el directorio de binarios, no su padre.
set -euo pipefail

usage() {
  cat <<'EOF'
Uso: scripts/install.sh [--help]

Compila en release e instala galgo, galgod, galgo-mcp y galgo-gui.
  PREFIX                 Directorio exacto de binarios (por defecto: $HOME/.galgo/bin).
                         No uses '~' entre comillas: usa "$HOME".
  CARGO_TARGET_DIR       Directorio de build del workspace (por defecto: <repo>/target).
  GALGO_GUI_TARGET_DIR   Directorio independiente de build de la GUI
                         (por defecto: <CARGO_TARGET_DIR>/galgo-gui).
No usa sudo ni modifica archivos de configuración de la shell.
EOF
}
if [[ ${1:-} == --help || ${1:-} == -h ]]; then usage; exit 0; fi
if [[ $# -ne 0 ]]; then usage >&2; exit 2; fi
: "${HOME:?HOME debe estar definido}"
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
PREFIX=${PREFIX:-"$HOME/.galgo/bin"}
if [[ "$PREFIX" == \~* ]]; then
  echo 'PREFIX no admite ~ literal; usa "$HOME" en su lugar' >&2
  exit 1
fi
WORK_TARGET=${CARGO_TARGET_DIR:-"$ROOT/target"}
GUI_TARGET=${GALGO_GUI_TARGET_DIR:-"$WORK_TARGET/galgo-gui"}
# Normalizar rutas relativas para que Cargo y la copia resuelvan el mismo directorio.
mkdir -p "$WORK_TARGET" "$GUI_TARGET" "$PREFIX"
WORK_TARGET=$(cd "$WORK_TARGET" && pwd -P)
GUI_TARGET=$(cd "$GUI_TARGET" && pwd -P)
PREFIX=$(cd "$PREFIX" && pwd -P)
if [[ "$WORK_TARGET" == "$GUI_TARGET" ]]; then
  echo 'GALGO_GUI_TARGET_DIR debe ser distinto de CARGO_TARGET_DIR' >&2
  exit 1
fi
cd "$ROOT"
echo 'Compilando galgo, galgod y galgo-mcp (release)...'
CARGO_TARGET_DIR="$WORK_TARGET" cargo build --release --locked -p galgo-cli -p galgo-daemon -p galgo-mcp
echo 'Compilando galgo-gui (release, target separado)...'
CARGO_TARGET_DIR="$GUI_TARGET" cargo build --release --locked --manifest-path "$ROOT/crates/galgo-gui/Cargo.toml"
for bin in galgo galgod galgo-mcp; do
  install -m 755 "$WORK_TARGET/release/$bin" "$PREFIX/$bin"
done
install -m 755 "$GUI_TARGET/release/galgo-gui" "$PREFIX/galgo-gui"
printf '\nBinarios instalados en %s\n' "$PREFIX"
printf 'Para esta sesión, añade al PATH:\n  export PATH=%q:"$PATH"\n' "$PREFIX"
echo 'Añade esa línea manualmente a la configuración de tu shell si quieres hacerla permanente.'
echo 'Si ya hay un daemon en ejecución, seguirá usando la versión y entorno anteriores.'
echo 'Para actualizarlo, cierra o guarda el trabajo de sus paneles y ejecuta galgo stop; esto detiene todos sus paneles.'
