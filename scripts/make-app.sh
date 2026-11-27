#!/usr/bin/env bash
# Bundle macOS autónomo a partir de los binarios instalados por install.sh.
set -euo pipefail
usage() {
  cat <<'EOF'
Uso: scripts/make-app.sh [--bin-dir DIRECTORIO] [--output RUTA/galgo.app]

Empaqueta galgo, galgod, galgo-mcp y galgo-gui en un bundle autónomo.
Por defecto usa ${PREFIX:-$HOME/.galgo/bin} y crea ./galgo.app.
No reemplaza un bundle existente. Requiere macOS y python3 (Command Line Tools).
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
    <key>CFBundleIconFile</key><string>galgo</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
cat > "$APP/Contents/MacOS/galgo-launcher" <<'LAUNCHER'
#!/bin/sh
set -eu
DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd -P)
# Finder suele aportar un PATH mínimo. No ejecutar archivos de inicio de la shell:
# agregar solo directorios habituales; si ya corre un daemon, este conserva su entorno.
# El PATH de un login shell (.zprofile, brew, etc.) sin cargar .zshrc: rápido y como en Terminal.app.
LOGIN_PATH=$(/bin/zsh -l -c 'printf %s "$PATH"' 2>/dev/null || true)
export PATH="$DIR:${LOGIN_PATH:+$LOGIN_PATH:}$HOME/.local/bin:$HOME/.opencode/bin:$HOME/.cargo/bin:$HOME/.npm-global/bin:$HOME/.bun/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}"
# CLI y daemon hermanos: la CLI conecta o arranca el daemon si no existe.
if ! "$DIR/galgo" ls >/dev/null; then
    /usr/bin/osascript -e 'display alert "Galgo no pudo iniciar el daemon" message "Revisa galgod.log junto al socket de galgo y vuelve a intentarlo." as critical' || true
    exit 1
fi
exec "$DIR/galgo-gui"
LAUNCHER
chmod 755 "$APP/Contents/MacOS/galgo-launcher"
# Icono: se dibuja con scripts/make-icon.swift (colores del tema) y se empaqueta como .icns.
SCRIPTS=$(cd "$(dirname "$0")" && pwd -P)
ICONSET="$STAGE/galgo.iconset"
mkdir -p "$ICONSET"
swift "$SCRIPTS/make-icon.swift" "$STAGE/icon-1024.png"
for n in 16 32 128 256 512; do
  sips -z "$n" "$n" "$STAGE/icon-1024.png" --out "$ICONSET/icon_${n}x${n}.png" >/dev/null
  sips -z "$((n * 2))" "$((n * 2))" "$STAGE/icon-1024.png" --out "$ICONSET/icon_${n}x${n}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/galgo.icns"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
# renamex_np(RENAME_EXCL) realiza la comprobación y el rename en una sola operación.
# A diferencia de mv, no introduce el bundle dentro de un directorio aparecido en la carrera.
python3 - "$APP" "$OUTPUT" <<'PY'
import ctypes, errno, os, sys
libc = ctypes.CDLL(None, use_errno=True)
rename = libc.renamex_np
rename.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
rename.restype = ctypes.c_int
if rename(os.fsencode(sys.argv[1]), os.fsencode(sys.argv[2]), 0x00000004) != 0:
    err = ctypes.get_errno()
    if err in (errno.EEXIST, errno.ENOTEMPTY):
        sys.exit(f"Ya existe {sys.argv[2]}; no se reemplazó")
    sys.exit(f"No se pudo crear {sys.argv[2]}: {os.strerror(err)}")
PY
printf 'Bundle creado: %s\n' "$OUTPUT"
echo 'Puedes moverlo a /Applications o ~/Applications y abrirlo desde Finder o Spotlight.'
