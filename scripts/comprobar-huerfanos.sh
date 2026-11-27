#!/bin/bash
# Comprueba que no queda ningún galgod arrancado con un socket bajo el directorio dado (el $TMP de e2e, o el
# $TMPDIR de una prueba). Los que encuentra los para con `galgo stop` por su socket y, si no responden, con SIGTERM
# a su PID. Compara rutas reales (realpath, con symlinks): un socket cuenta si su ruta real está bajo la del
# directorio. Se niega a actuar con /, con tu HOME (ni sus ancestros), con ~/.galgo (ni nada dentro o encima de él),
# o con un directorio que no existe. El daemon de uso normal (~/.galgo) queda intacto.
# Uso: scripts/comprobar-huerfanos.sh <directorio>   (sale con 0 si no queda ninguno, 1 si quedó alguno, 2 si rechaza)
set -u
DIR="${1:?uso: comprobar-huerfanos.sh <directorio>}"
GALGO="${GALGO:-galgo}"
RDIR=$(python3 - "$DIR" <<'PY'
import os, sys
entrada = sys.argv[1]
def dentro(a, b):  # a está en b, o es b
    return a == b or a.startswith(b.rstrip("/") + "/")
if not os.path.isdir(entrada):
    print("ERR no es un directorio existente"); sys.exit()
d = os.path.realpath(entrada)
casa = os.path.realpath(os.path.expanduser("~"))
galgo_casa = os.path.realpath(os.path.expanduser("~/.galgo"))
if d == "/":
    print("ERR es la raíz del disco"); sys.exit()
if dentro(casa, d):
    print("ERR contiene tu HOME (o es tu HOME)"); sys.exit()
if dentro(galgo_casa, d) or dentro(d, galgo_casa):
    print("ERR se solapa con ~/.galgo"); sys.exit()
print(d)
PY
)
case "$RDIR" in
  ERR*) echo "rechazado: ${RDIR#ERR }: $DIR"; exit 2 ;;
esac
quedan() { # líneas "pid socket" de los galgod cuyo socket real está bajo RDIR
  for P in $(pgrep -x galgod 2>/dev/null); do
    s=$(ps eww -o command= -p "$P" 2>/dev/null | tr ' ' '\n' | grep '^GALGO_SOCKET=' | head -1 | cut -d= -f2-)
    [ -n "$s" ] || continue
    python3 -c 'import os,sys; sys.exit(0 if os.path.realpath(sys.argv[1]).startswith(sys.argv[2].rstrip("/") + "/") else 1)' "$s" "$RDIR" && echo "$P $s"
  done
}
for intento in 1 2 3; do
  restos=$(quedan)
  [ -z "$restos" ] && break
  while read -r P s; do
    [ -n "$P" ] || continue
    GALGO_SOCKET="$s" "$GALGO" stop >/dev/null 2>&1
    [ "$intento" = 3 ] && kill "$P" 2>/dev/null
  done <<< "$restos"
  sleep 1
done
restos=$(quedan)
if [ -n "$restos" ]; then
  echo "HUÉRFANOS bajo $RDIR:"; echo "$restos"; exit 1
fi
echo "sin huérfanos bajo $RDIR"
