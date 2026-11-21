#!/usr/bin/env bash
# Bundle macOS autónomo a partir de los binarios instalados por install.sh.
set -euo pipefail
usage() {
  cat <<'EOF'
Uso: scripts/make-app.sh [--bin-dir DIRECTORIO] [--output RUTA/galgo.app]

Empaqueta galgo, galgod, galgo-mcp y galgo-gui en un bundle autónomo.
Por defecto usa ${PREFIX:-$HOME/.galgo/bin} y crea ./galgo.app.
No reemplaza un bundle existente. Requiere macOS.
EOF
}
: "${HOME:?HOME debe estar definido}"
BIN_DIR=${PREFIX:-"$HOME/.galgo/bin"}
OUTPUT="$PWD/galgo.app"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --bin-dir) [[ $# -ge 2 ]] || { usage >&2; exit 2; }; BIN_DIR=$2; shift 2 ;;
    --output) [[ $# -ge 2 ]] || { usage >&2; exit 2; }; OUTPUT=$2; shift 2 ;;
    *) usage >&2; exit 2 ;;
  esac
done
[[ $(uname -s) == Darwin ]] || { echo 'make-app.sh requiere macOS' >&2; exit 1; }
[[ "$OUTPUT" == *.app ]] || { echo 'La ruta de salida debe terminar en .app' >&2; exit 1; }
for bin in galgo galgod galgo-mcp galgo-gui; do
  [[ -f "$BIN_DIR/$bin" && -x "$BIN_DIR/$bin" ]] || { echo "Falta el ejecutable $BIN_DIR/$bin; ejecuta scripts/install.sh" >&2; exit 1; }
done
PARENT=$(dirname "$OUTPUT")
NAME=$(basename "$OUTPUT")
mkdir -p "$PARENT"
PARENT=$(cd "$PARENT" && pwd -P)
OUTPUT="$PARENT/$NAME"
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || { echo "Ya existe $OUTPUT; no se reemplazó" >&2; exit 1; }
STAGE=$(mktemp -d "$PARENT/.galgo-app.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/$NAME"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
for bin in galgo galgod galgo-mcp galgo-gui; do
  install -m 755 "$BIN_DIR/$bin" "$APP/Contents/MacOS/$bin"
done
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>es</string>
    <key>CFBundleExecutable</key><string>galgo-launcher</string>
    <key>CFBundleIdentifier</key><string>dev.galgo.app</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>galgo</string>
    <key>CFBundleDisplayName</key><string>galgo</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
cat > "$APP/Contents/MacOS/galgo-launcher" <<'LAUNCHER'
#!/bin/sh
set -eu
DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd -P)
# CLI y daemon hermanos: la CLI conecta o arranca el daemon si no existe.
export PATH="$DIR:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
"$DIR/galgo" ls >/dev/null
exec "$DIR/galgo-gui"
LAUNCHER
chmod 755 "$APP/Contents/MacOS/galgo-launcher"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
mv "$APP" "$OUTPUT"
printf 'Bundle creado: %s\n' "$OUTPUT"
echo 'Puedes moverlo a /Applications o ~/Applications y abrirlo desde Finder o Spotlight.'
