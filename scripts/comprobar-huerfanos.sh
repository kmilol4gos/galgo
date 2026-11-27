#!/bin/bash
# Comprueba que no queda ningún galgod arrancado con un socket bajo el directorio dado (el $TMP de e2e, o el
# $TMPDIR de una prueba). Los que encuentra los para con `galgo stop` por su socket y, si no responden, con SIGTERM
# a su PID. Solo toca daemons cuyo GALGO_SOCKET está bajo ese directorio: el de uso normal (~/.galgo) queda intacto.
# Uso: scripts/comprobar-huerfanos.sh <directorio>   (sale con 0 si no queda ninguno, 1 si quedó alguno)
set -u
DIR="${1:?uso: comprobar-huerfanos.sh <directorio>}"
DIR="${DIR%/}"
GALGO="${GALGO:-galgo}"
quedan() { # líneas "pid socket" de los galgod cuyo socket está bajo DIR
  for P in $(pgrep -x galgod 2>/dev/null); do
    s=$(ps eww -o command= -p "$P" 2>/dev/null | tr ' ' '\n' | grep '^GALGO_SOCKET=' | head -1 | cut -d= -f2-)
    case "$s" in "$DIR"/*) echo "$P $s" ;; esac
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
  echo "HUÉRFANOS bajo $DIR:"; echo "$restos"; exit 1
fi
echo "sin huérfanos bajo $DIR"
