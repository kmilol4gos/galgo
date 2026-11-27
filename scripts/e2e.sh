#!/usr/bin/env bash
# Prueba de extremo a extremo: daemon real, PTYs reales.
# Uso: cargo build && scripts/e2e.sh
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export PATH="$ROOT/target/debug:$PATH"
TMP=$(mktemp -d)
export GALGO_SOCKET="$TMP/galgo.sock"
export GALGO_ORPHAN_SECS=1 GALGO_RETRY_SECS=2
pass=0
fail=0
skipped=0

check() { # descripción, código de salida
  if [ "$2" = "0" ]; then echo "  ok    $1"; pass=$((pass + 1));
  elif [ "$2" = "77" ]; then echo "  omitido $1 (falta el programa)"; skipped=$((skipped + 1));
  else echo "  FALLA $1"; fail=$((fail + 1)); fi
}
# B6: cada daemon que el script arranca (uno por socket de $TMP) se detiene antes de borrar $TMP. Un
# `galgo ls` posterior vuelve a arrancar uno, así que se para cada socket que exista, no solo el de la sección actual.
cleanup() {
  for sock in $(find "$TMP" -name '*.sock' 2>/dev/null) "$GALGO_SOCKET"; do
    GALGO_SOCKET="$sock" galgo stop >/dev/null 2>&1
  done
  rm -rf "$TMP"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

echo "1. wait recibe el resultado del worker apenas termina"
galgo spawn mgr -- bash --norc >/dev/null
galgo spawn w1 --manager mgr -- bash --norc >/dev/null
( galgo wait w1 --pane mgr --timeout 10 > "$TMP/wait1" 2>&1 ) &
WPID=$!
sleep 0.5
galgo send w1 'galgo done --summary "listo: 2 comentarios"'
wait $WPID
grep -q "w1: listo: 2 comentarios" "$TMP/wait1"; check "wait devuelve el resumen" $?

echo "2. wait sobre un worker que ya terminó vuelve de inmediato"
galgo wait w1 --pane mgr --timeout 2 >/dev/null 2>&1; check "no se cuelga" $?

echo "3. wait con tiempo agotado falla con mensaje claro"
galgo spawn w3 -- bash --norc >/dev/null
galgo wait w3 --pane mgr --timeout 1 2>&1 | grep -q "tiempo agotado"; check "informa timeout" $?

echo "4. entrega sin confirmar se marca como huérfana, y ack la limpia"
galgo spawn mgr4 -- bash --norc >/dev/null
galgo spawn w4 --manager mgr4 -- bash --norc >/dev/null
galgo send w4 'galgo done --summary "resultado4"'
sleep 2.5
galgo ls | grep mgr4 | grep -q "ENTREGA SIN CONFIRMAR"; check "ls marca la espera huérfana" $?
ID=$(galgo inbox --pane mgr4 | sed -n 's/^#\([0-9]*\) .*/\1/p' | head -1)
galgo inbox --pane mgr4 | grep -q "resultado4"; check "inbox muestra el resultado" $?
galgo ack "$ID" --pane mgr4 >/dev/null
galgo ls | grep mgr4 | grep -q "ENTREGA"; [ $? -ne 0 ]; check "tras ack ya no es huérfana" $?

echo "5. inject escribe el resultado en el manager cuando está idle"
galgo spawn mgr5 --inject -- cat >/dev/null
galgo spawn w5 --manager mgr5 -- bash --norc >/dev/null
galgo send w5 'galgo done --summary "hecho5"'
sleep 1
galgo read mgr5 | grep -q "\[galgo\] w5 terminó: hecho5"; check "mensaje inyectado" $?

echo "6. inject espera si el manager está trabajando y entrega al quedar idle"
galgo spawn w6 --manager mgr5 -- bash --norc >/dev/null
galgo status working --pane mgr5
galgo send w6 'galgo done --summary "hecho6"'
sleep 1
galgo read mgr5 | grep -q "hecho6"; [ $? -ne 0 ]; check "no interrumpe mientras trabaja" $?
galgo status idle --pane mgr5
sleep 1
galgo read mgr5 | grep -q "\[galgo\] w6 terminó: hecho6"; check "se entrega al quedar idle" $?

echo "7. un worker que muere sin reportar avisa al manager"
galgo spawn mgr7 -- bash --norc >/dev/null
galgo spawn w7 --manager mgr7 -- bash --norc >/dev/null
galgo send w7 'exit'
sleep 1
galgo inbox --pane mgr7 | grep -q "w7 \[exited\]"; check "mensaje exited en el inbox" $?
galgo ls | grep w7 | grep -q exited; check "estado exited" $?

echo "8. read devuelve la pantalla del panel"
galgo send mgr 'echo hola-galgo'
sleep 0.5
galgo read mgr | grep -q "hola-galgo"; check "lee la salida" $?

echo "9. fallo explícito y errores de uso"
galgo spawn w9 --manager mgr -- bash --norc >/dev/null
galgo send w9 'galgo done --failed --summary "no compila"'
galgo wait w9 --pane mgr --timeout 2 | grep -q "no compila"; check "done --failed llega" $?
galgo spawn mgr -- bash 2>&1 | grep -q "ya existe"; check "rechaza nombre duplicado" $?
galgo read no-existe 2>&1 | grep -q "no encontrado"; check "error por panel inexistente" $?

hook() { GALGO_PANE="$1" galgo hook claude; }   # el JSON del evento llega por stdin, como en Claude Code

echo "10. hooks de Claude Code: estado y entrega al manager"
galgo spawn mgr10 -- bash --norc >/dev/null
galgo spawn w10 --manager mgr10 -- bash --norc >/dev/null
echo '{"hook_event_name":"UserPromptSubmit","prompt":"haz X"}' | hook w10
galgo ls | grep w10 | grep -q working; check "UserPromptSubmit -> working" $?
echo '{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash"}' | hook w10
galgo ls | grep w10 | grep -q blocked; check "permission_prompt -> blocked" $?
galgo wait w10 --pane mgr10 --timeout 2 | grep -q "permission"; check "el manager se entera del bloqueo (wait vuelve)" $?
echo '{"hook_event_name":"PostToolUse","tool_name":"Bash"}' | hook w10
galgo ls | grep w10 | grep -q working; check "PostToolUse tras aprobar -> working" $?
echo '{"hook_event_name":"Stop","last_assistant_message":"terminé la tarea 10: 3 archivos"}' | hook w10
galgo ls | grep w10 | grep -q done; check "Stop -> done" $?
galgo wait w10 --pane mgr10 --timeout 2 | grep -q "terminé la tarea 10: 3 archivos"; check "el manager recibe lo último que dijo el worker" $?

echo "11. Stop tras un galgo done explícito no duplica el mensaje"
galgo spawn mgr11 -- bash --norc >/dev/null
galgo spawn w11 --manager mgr11 -- bash --norc >/dev/null
galgo send w11 'galgo done --summary "explícito"'
sleep 0.7
echo '{"hook_event_name":"Stop","last_assistant_message":"otro texto"}' | hook w11
[ "$(galgo inbox --pane mgr11 | grep -c '^#')" = "1" ]; check "un solo mensaje en el inbox" $?

echo "12. StopFailure llega como fallo"
galgo spawn mgr12 -- bash --norc >/dev/null
galgo spawn w12 --manager mgr12 -- bash --norc >/dev/null
echo '{"hook_event_name":"StopFailure","error":"rate_limit"}' | hook w12
galgo inbox --pane mgr12 | grep -q "w12 \[failed\].*rate_limit"; check "mensaje failed con el motivo" $?

echo "13. el hook nunca hace ruido ni rompe a Claude"
out=$(echo '{"hook_event_name":"Stop"}' | env -u GALGO_PANE galgo hook claude 2>&1); rc=$?
[ "$rc" = "0" ] && [ -z "$out" ]; check "fuera de galgo: exit 0 y sin salida" $?
out=$(echo '{"hook_event_name":"Stop"}' | GALGO_PANE=no-existe galgo hook claude 2>&1); rc=$?
[ "$rc" = "0" ] && [ -z "$out" ]; check "panel desconocido: exit 0 y sin salida" $?
out=$(echo 'esto no es json' | hook w10 2>&1); rc=$?
[ "$rc" = "0" ] && [ -z "$out" ]; check "entrada inválida: exit 0 y sin salida" $?

echo "14. instalar y quitar los hooks en settings.json"
export CLAUDE_CONFIG_DIR="$TMP/cc"
mkdir -p "$CLAUDE_CONFIG_DIR"
S="$CLAUDE_CONFIG_DIR/settings.json"
echo '{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"say listo"}]}]}}' > "$S"
cp "$S" "$TMP/original.json"
galgo hooks install claude --dry-run >/dev/null; cmp -s "$S" "$TMP/original.json"; check "--dry-run no escribe nada" $?
galgo hooks install claude </dev/null 2>&1 | grep -q -- "--yes"; check "sin terminal pide --yes en vez de escribir" $?
cmp -s "$S" "$TMP/original.json"; check "y el archivo sigue intacto" $?
galgo hooks install claude --yes >/dev/null
grep -q "hook claude" "$S" && grep -q "say listo" "$S" && grep -q '"model": "opus"' "$S"; check "instala y conserva lo del usuario" $?
[ -f "$S.galgo-bak" ] || [ -f "$CLAUDE_CONFIG_DIR/settings.json.galgo-bak" ]; check "deja un respaldo" $?
galgo hooks install claude --yes | grep -q "Ya está instalado"; check "segunda instalación no cambia nada" $?
galgo hooks status claude | grep -q "✓ Stop"; check "status reconoce la instalación" $?
galgo hooks uninstall claude --yes >/dev/null
! grep -q "hook claude" "$S" && grep -q "say listo" "$S"; check "uninstall quita solo lo de galgo" $?
echo '{oops' > "$S"
galgo hooks install claude --yes 2>&1 | grep -q "no es JSON válido"; check "JSON inválido: avisa" $?
[ "$(cat "$S")" = '{oops' ]; check "y no toca el archivo" $?
unset CLAUDE_CONFIG_DIR

echo "15. sin daemon, el hook sigue en silencio y no arranca uno"
galgo stop >/dev/null 2>&1
sleep 0.5
out=$(echo '{"hook_event_name":"Stop","last_assistant_message":"x"}' | hook w10 2>&1); rc=$?
[ "$rc" = "0" ] && [ -z "$out" ]; check "exit 0 y sin salida" $?
[ ! -S "$GALGO_SOCKET" ]; check "no levantó un daemon" $?

echo "16. scripts/team.sh levanta el equipo con los modelos pedidos (agentes falsos)"
mkdir -p "$TMP/bin"
for b in claude pi; do
  printf '#!/bin/sh\necho "%s $*"\nsleep 30\n' "$b" > "$TMP/bin/$b"
  chmod +x "$TMP/bin/$b"
done
CLAUDE_BIN="$TMP/bin/claude" PI_BIN="$TMP/bin/pi" PLANNER_MODEL=openai/terra \
  "$ROOT/scripts/team.sh" eq "$TMP" >/dev/null
# Espera a que los agentes falsos escriban su línea (no un tiempo fijo: con la máquina cargada tardan más).
for i in $(seq 1 50); do galgo read eq-reviewer 2>/dev/null | grep -q "claude --model opus" && break; sleep 0.2; done
for i in $(seq 1 50); do galgo read eq-planner 2>/dev/null | grep -q "pi --model openai/terra" && break; sleep 0.2; done
[ "$(galgo ls | grep -c ' eq-')" = "4" ]; check "crea los 4 paneles" $?
galgo read eq-programmer | grep -q "claude --model haiku"; check "programmer usa haiku" $?
galgo read eq-reviewer | grep -q "claude --model opus"; check "reviewer usa opus" $?
galgo read eq-planner | grep -q "pi --model openai/terra"; check "planner usa PI con el modelo indicado" $?
galgo ls | grep eq-planner | grep -q "reporta a eq-manager"; check "el planner reporta al manager" $?
galgo ls | awk '$2 == "eq-manager"' | grep -q "reporta"; [ $? -ne 0 ]; check "el manager no reporta a nadie" $?

echo "17. galgo-mcp: servidor MCP por stdio contra el daemon real"
MCP="$TMP/mcp17.out"
{
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"galgo_spawn","arguments":{"name":"mcp17","cmd":["bash","--norc"]}}}'
  echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"galgo_send","arguments":{"pane":"mcp17","text":"echo hola-mcp-17"}}}'
  sleep 1
  echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"galgo_read","arguments":{"pane":"mcp17"}}}'
  echo '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"galgo_read","arguments":{"pane":"no-existe"}}}'
  echo '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"galgo_nada","arguments":{}}}'
} | galgo-mcp > "$MCP" 2> "$TMP/mcp17.err"
[ "$(wc -l < "$MCP" | tr -d ' ')" = "7" ]; check "una respuesta por solicitud (la notificación no responde)" $?
[ ! -s "$TMP/mcp17.err" ]; check "stderr vacío" $?
grep '"id":1,' "$MCP" | grep -q '"protocolVersion":"2025-06-18"'; check "initialize devuelve la versión pedida" $?
grep '"id":1,' "$MCP" | grep -q '"name":"galgo"'; check "initialize identifica el servidor como galgo" $?
grep '"id":1,' "$MCP" | grep -q '"tools":{}'; check "initialize anuncia tools" $?
grep '"id":2,' "$MCP" | grep -q '"name":"galgo_spawn"'; check "tools/list incluye galgo_spawn" $?
grep '"id":2,' "$MCP" | grep -q '"name":"galgo_send"'; check "tools/list incluye galgo_send" $?
grep '"id":2,' "$MCP" | grep -q '"inputSchema"'; check "tools/list trae inputSchema" $?
grep '"id":3,' "$MCP" | grep -q '"isError":false'; check "galgo_spawn sin error" $?
grep '"id":3,' "$MCP" | grep -q 'mcp17'; check "galgo_spawn devuelve el panel creado" $?
grep '"id":4,' "$MCP" | grep -q '"isError":false'; check "galgo_send sin error" $?
grep '"id":5,' "$MCP" | grep -q '"isError":false'; check "galgo_read sin error" $?
grep '"id":5,' "$MCP" | grep -q 'hola-mcp-17'; check "galgo_read devuelve la salida del panel" $?
grep '"id":6,' "$MCP" | grep -q '"isError":true'; check "panel inexistente: isError, no error JSON-RPC" $?
grep '"id":6,' "$MCP" | grep -q '"error"'; [ $? -ne 0 ]; check "y sin campo error" $?
grep '"id":7,' "$MCP" | grep -q '"code":-32602'; check "herramienta desconocida: -32602" $?

echo
echo "30. cwd relativo se resuelve en el cliente"
mkdir -p "$TMP/cwd30/sub"
(cd "$TMP/cwd30" && galgo spawn cwd30 --cwd sub -- pwd >/dev/null)
sleep 0.5
galgo read cwd30 | grep -Fq "$(cd "$TMP/cwd30/sub" && pwd -P)"; check "cwd relativo al cliente" $?

echo "31. error de arranque del daemon incluye stderr"
LONG31="$TMP/$(printf '%0120d' 0)/galgo.sock"
GALGO_SOCKET="$LONG31" galgo ls >"$TMP/out31" 2>&1
[ $? -ne 0 ] && grep -qiE 'long|largo|sun_len|invalid' "$TMP/out31"; check "causa de error visible" $?

echo "32. team respeta GALGO_INJECT y cwd absoluto"
mkdir -p "$TMP/mock32" "$TMP/dir32"
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$ARGS32"\n' > "$TMP/mock32/galgo"
chmod +x "$TMP/mock32/galgo"
(cd "$TMP" && ARGS32="$TMP/args32" PATH="$TMP/mock32:$PATH" GALGO_INJECT=0 "$ROOT/scripts/team.sh" team32 dir32 >/dev/null)
grep -q -- --inject "$TMP/args32"; [ $? -ne 0 ]; check "GALGO_INJECT=0 no inyecta" $?
grep -Fq -- "--cwd $(cd "$TMP/dir32" && pwd -P)" "$TMP/args32"; check "team usa cwd absoluto" $?
ARGS32="$TMP/args321" PATH="$TMP/mock32:$PATH" GALGO_INJECT=1 "$ROOT/scripts/team.sh" team321 "$TMP" >/dev/null
grep -q -- --inject "$TMP/args321"; check "GALGO_INJECT=1 inyecta" $?

echo "18. resultado duplicado: galgo done explícito + PostToolUse + Stop entregan una sola vez"
galgo spawn mgr18 -- bash --norc >/dev/null
galgo spawn w18 --manager mgr18 -- bash --norc >/dev/null
galgo send w18 'galgo done --summary "explicito18"'
sleep 0.7
echo '{"hook_event_name":"PostToolUse","tool_name":"Bash"}' | hook w18
echo '{"hook_event_name":"Stop","last_assistant_message":"ya reporté"}' | hook w18
[ "$(galgo inbox --pane mgr18 | grep -c '^#')" = "1" ]; check "exactamente 1 mensaje en el inbox del manager" $?
galgo ls | grep w18 | grep -q " done "; check "PostToolUse no saca al worker de done" $?
echo '{"hook_event_name":"UserPromptSubmit","prompt":"otra tarea"}' | hook w18
echo '{"hook_event_name":"Stop","last_assistant_message":"segundo turno"}' | hook w18
[ "$(galgo inbox --pane mgr18 | grep -c '^#')" = "2" ]; check "un turno nuevo (UserPromptSubmit) sí vuelve a reportar" $?

echo "19. un wait abandonado no confirma el resultado"
galgo spawn m19 -- bash --norc >/dev/null
galgo spawn w19 --manager m19 -- bash --norc >/dev/null
galgo wait w19 --pane m19 --timeout 30 >/dev/null 2>&1 &
WPID=$!
sleep 0.5
kill "$WPID"; wait "$WPID" 2>/dev/null
galgo send w19 'galgo done --summary "resultado19"'
sleep 1.5
galgo inbox --pane m19 | grep -q "resultado19"; check "el mensaje sigue pendiente en el inbox" $?
galgo ls | grep m19 | grep -q "ENTREGA SIN CONFIRMAR"; check "ls lo marca como entrega sin confirmar" $?
galgo ls | grep m19 | grep -q "espera a"; [ $? -ne 0 ]; check "ya no figura como esperando" $?

echo "20. la inyección no pasa caracteres de control al manager"
galgo spawn m20 --inject -- cat -v >/dev/null
galgo spawn w20 --manager m20 -- bash --norc >/dev/null
echo '{"hook_event_name":"Stop","last_assistant_message":"uno\rdos\u0003\u001b[2J"}' | hook w20
sleep 1
galgo ls | grep m20 | grep -q exited; [ $? -ne 0 ]; check "el manager sigue vivo" $?
galgo read m20 | grep -q "w20 terminó"; check "el resumen llegó al manager" $?

echo "21. un panel que no lee su entrada no congela al daemon"
galgo spawn b21 -- bash -c 'stty raw -echo; sleep 300' >/dev/null
sleep 0.3
BIG=$(head -c 100000 /dev/zero | tr '\0' 'x')
{ galgo send b21 "$BIG"; for _ in $(seq 12); do galgo send b21 x; done; } >/dev/null 2>&1 &
FLOOD=$!
sleep 0.5
galgo ls >/dev/null 2>&1 &
LS=$!
responded=1
for _ in $(seq 20); do kill -0 "$LS" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$LS" 2>/dev/null; then responded=0; kill "$LS"; fi
wait "$LS" 2>/dev/null
[ "$responded" = "1" ]; check "galgo ls responde en menos de 2 s" $?
pkill -P "$FLOOD" 2>/dev/null; kill "$FLOOD" 2>/dev/null; wait "$FLOOD" 2>/dev/null
galgo kill b21 >/dev/null 2>&1

echo "22. spawns concurrentes con el mismo nombre: uno solo gana"
galgo ls >/dev/null
PIDS=""
for i in 1 2 3 4 5 6; do galgo spawn dup22 -- bash --norc > "$TMP/dup22.$i" 2>&1 & PIDS="$PIDS $!"; done
wait $PIDS
[ "$(cat "$TMP"/dup22.* | grep -c 'creado')" = "1" ]; check "exactamente un éxito" $?
[ "$(cat "$TMP"/dup22.* | grep -c 'ya existe')" = "5" ]; check "los otros cinco: ya existe" $?
[ "$(galgo ls | grep -c ' dup22 ')" = "1" ]; check "un solo panel llamado dup22" $?

echo "23. un worker terminado no deja zombis y el aviso trae el código de salida"
galgo spawn m23 -- bash --norc >/dev/null
galgo spawn z23 --manager m23 -- bash -c "echo \$PPID > $TMP/ppid23; exit 3" >/dev/null
sleep 1
galgo inbox --pane m23 | grep -q "z23 \[exited\].*código 3"; check "el aviso exited trae el código 3" $?
DPID=$(cat "$TMP/ppid23")
[ "$(ps -axo ppid=,stat= | awk -v d="$DPID" '$1 == d && $2 ~ /Z/' | wc -l | tr -d ' ')" = "0" ]; check "ningún zombi del daemon" $?

echo "24. spawn con un cwd que no existe se rechaza"
galgo spawn c24 --cwd "$TMP/no-existe" -- bash --norc 2>&1 | grep -q "no existe"; check "error claro" $?
galgo ls | grep -q " c24 "; [ $? -ne 0 ]; check "no crea el panel" $?

echo "50. MCP cancela wait sin bloquear ping ni consumir el resultado"
galgo spawn m50 -- bash --norc >/dev/null
galgo spawn w50 --manager m50 -- bash --norc >/dev/null
GALGO_PANE=m50 python3 - <<'PY50'
import json, os, select, subprocess, time
p = subprocess.Popen(['galgo-mcp'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def send(o):
    p.stdin.write((json.dumps(o)+'\n').encode()); p.stdin.flush()
try:
    send({'jsonrpc':'2.0','id':50,'method':'tools/call','params':{'name':'galgo_wait','arguments':{'target':'w50','timeout_secs':20}}})
    time.sleep(.5)
    send({'jsonrpc':'2.0','method':'notifications/cancelled','params':{'requestId':50}})
    send({'jsonrpc':'2.0','id':51,'method':'ping'})
    assert select.select([p.stdout],[],[],2)[0], 'ping bloqueado'
    assert json.loads(p.stdout.readline())['id'] == 51
    subprocess.run(['galgo','done','--pane','w50','--summary','resultado50'], check=True)
    time.sleep(.6)
    assert b'resultado50' in subprocess.check_output(['galgo','inbox','--pane','m50'])
    assert not select.select([p.stdout],[],[],.3)[0], 'respuesta a llamada cancelada'
finally:
    p.kill(); p.wait()
PY50
check "cancelación y ping" $?

echo "51. temporal huérfano no impide instalar"
mkdir -p "$TMP/cc51"
echo '{}' > "$TMP/cc51/settings.json"
touch "$TMP/cc51/settings.json.galgo-tmp"
CLAUDE_CONFIG_DIR="$TMP/cc51" galgo hooks install claude --yes >/dev/null && CLAUDE_CONFIG_DIR="$TMP/cc51" galgo hooks install claude --yes >/dev/null
check "dos instalaciones con temporal huérfano" $?

echo "52. MCP recupera UTF-8 inválido y negocia versión"
printf '\377\n%s\n%s\n' '{"jsonrpc":"2.0","id":52,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}' '{"jsonrpc":"2.0","id":53,"method":"ping"}' | galgo-mcp > "$TMP/mcp52" 2>/dev/null
grep -q -- '"code":-32700' "$TMP/mcp52" && grep -q '"protocolVersion":"2025-06-18"' "$TMP/mcp52" && grep -q '"id":53' "$TMP/mcp52"; check "parse error, versión y ping" $?

echo "53. log privado y sin seguir symlinks"
mkdir -p "$TMP/log53"; echo original > "$TMP/log53/target"; ln -s target "$TMP/log53/galgod.log"
GALGO_SOCKET="$TMP/log53/sock" galgo ls >"$TMP/log53/out" 2>&1
[ $? -ne 0 ] && [ "$(cat "$TMP/log53/target")" = original ]; check "rechaza symlink" $?
rm "$TMP/log53/galgod.log"
GALGO_SOCKET="$TMP/log53/sock" galgo ls >/dev/null 2>&1
python3 -c 'import os,sys; sys.exit((os.stat(sys.argv[1]).st_mode & 0o777) != 0o600)' "$TMP/log53/galgod.log"; check "log 0600" $?
GALGO_SOCKET="$TMP/log53/sock" galgo stop >/dev/null 2>&1

echo "40. el EOF tras una solicitud no la cancela"
galgo spawn mgr40 -- bash --norc >/dev/null
galgo spawn w40 --manager mgr40 -- bash --norc >/dev/null
python3 -I - "$GALGO_SOCKET" 30 w40 <<'PY' >/dev/null 2>&1
import json, socket, sys
path, count, pane = sys.argv[1], int(sys.argv[2]), sys.argv[3]
ok = 0
for i in range(count):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.sendall((json.dumps({"op": "done", "pane": pane, "summary": f"r{i}", "failed": False}) + "\n").encode())
    s.shutdown(socket.SHUT_WR)
    data = b""
    while not data.endswith(b"\n"):
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
    s.close()
    if data and json.loads(data).get("ok"):
        ok += 1
sys.exit(0 if ok == count else 1)
PY
check "las 30 solicitudes reciben respuesta" $?
[ "$(galgo inbox --pane mgr40 | grep -c '^#')" = "30" ]; check "las 30 se ejecutaron" $?

echo "41. salida después de un resultado: se avisa y queda exited"
galgo spawn m41 -- bash --norc >/dev/null
galgo spawn w41 --manager m41 -- bash --norc >/dev/null
galgo send w41 'galgo done --summary "listo41"'
sleep 0.5
galgo send w41 $'exit 7\r' --no-enter
sleep 1
galgo inbox --pane m41 | grep -q "w41 \[exited\].*código 7"; check "el manager recibe la salida con código 7" $?
galgo ls | grep w41 | grep -q exited; check "ls muestra exited" $?
galgo inbox --pane m41 | grep -q "listo41"; check "el resultado anterior sigue en el inbox" $?

echo "42. dos wait simultáneos: solo uno recibe el mismo mensaje"
galgo spawn m42 -- bash --norc >/dev/null
galgo spawn w42 --manager m42 -- bash --norc >/dev/null
galgo --json wait w42 --pane m42 --timeout 10 > "$TMP/wa42" 2>&1 &
A=$!
galgo --json wait w42 --pane m42 --timeout 10 > "$TMP/wb42" 2>&1 &
B=$!
sleep 0.5
galgo send w42 'galgo done --summary "unico42"'
wait $A $B
[ "$(cat "$TMP/wa42" "$TMP/wb42" | grep -c '"delivered"')" = "1" ]; check "solo un wait recibe el mensaje" $?

echo "43. kill: SIGKILL si ignora SIGHUP; panel terminado, mensaje claro"
galgo spawn k43 -- bash -c "trap '' HUP; echo \$\$ > $TMP/kpid43; exec sleep 300" >/dev/null
sleep 0.5
K=$(cat "$TMP/kpid43")
galgo kill k43 >/dev/null 2>&1
sleep 0.5
! kill -0 "$K" 2>/dev/null; check "mata al proceso que ignora SIGHUP" $?
kill -9 "$K" 2>/dev/null
galgo spawn fin43 -- bash -c 'exit 0' >/dev/null
sleep 1
galgo kill fin43 > "$TMP/fk43" 2>&1
grep -q "ya terminó" "$TMP/fk43"; check "panel terminado: mensaje claro" $?
! grep -q "No such process" "$TMP/fk43"; check "sin 'No such process'" $?

echo "54. MCP informa errores al iniciar galgod"
LONG54="$TMP/$(printf '%0120d' 0)/galgo.sock"
printf '%s\n' '{"jsonrpc":"2.0","id":54,"method":"tools/call","params":{"name":"galgo_list","arguments":{}}}' | GALGO_SOCKET="$LONG54" galgo-mcp > "$TMP/mcp54"
grep -qiE 'sun_len|long|largo|invalid' "$TMP/mcp54"; check "error del daemon llega a MCP" $?

echo "55. MCP respeta el orden sin bloquearse en wait"
galgo spawn m55 -- bash --norc >/dev/null
galgo spawn w55 --manager m55 -- bash --norc >/dev/null
GALGO_PANE=m55 python3 - "$TMP/55-result" <<'PY55'
import json, os, select, subprocess, sys, time
p = subprocess.Popen(['galgo-mcp'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def call(i, name, args):
    return {'jsonrpc':'2.0','id':i,'method':'tools/call','params':{'name':name,'arguments':args}}
def ping(i): return {'jsonrpc':'2.0','id':i,'method':'ping'}
messages = [call(1,'galgo_spawn',{'name':'pane55','cmd':['bash','--norc']}),
            call(2,'galgo_send',{'pane':'pane55','text':'echo prueba55'}),
            call(3,'galgo_read',{'pane':'pane55'}), ping(4),
            call(5,'galgo_wait',{'target':'w55','timeout_secs':10}), ping(6),
            call(7,'galgo_read',{'pane':'pane55'})]
try:
    p.stdin.write((''.join(json.dumps(x)+'\n' for x in messages)).encode()); p.stdin.flush()
    buf = b''; responses = []; deadline = time.monotonic() + 4
    while len(responses) < 6 and time.monotonic() < deadline:
        if not select.select([p.stdout], [], [], max(0, deadline-time.monotonic()))[0]: break
        chunk = os.read(p.stdout.fileno(), 65536)
        if not chunk: break
        buf += chunk
        while b'\n' in buf:
            line, buf = buf.split(b'\n', 1); responses.append(json.loads(line))
    assert [r['id'] for r in responses] == [1,2,3,4,6,7], responses
    assert all('error' not in r and not r.get('result',{}).get('isError',False) for r in responses), responses
    with open(sys.argv[1], 'w') as f: f.write('rapidas\n')
    subprocess.run(['galgo','done','--pane','w55','--summary','resultado55'], check=True)
    deadline = time.monotonic() + 4
    while b'\n' not in buf and time.monotonic() < deadline:
        if not select.select([p.stdout], [], [], max(0, deadline-time.monotonic()))[0]: break
        buf += os.read(p.stdout.fileno(), 65536)
    reply = json.loads(buf.split(b'\n', 1)[0])
    assert reply['id'] == 5 and not reply['result']['isError'], reply
    with open(sys.argv[1], 'a') as f: f.write('wait\n')
finally:
    p.kill(); p.wait()
PY55
grep -q '^rapidas$' "$TMP/55-result" 2>/dev/null; check "respuestas rápidas en orden y antes del wait" $?
grep -q '^wait$' "$TMP/55-result" 2>/dev/null; check "wait responde tras completar el worker" $?

echo "70. EOF cancela wait y conserva el resultado"
galgo spawn m70 -- bash --norc >/dev/null
galgo spawn w70 --manager m70 -- bash --norc >/dev/null
GALGO_PANE=m70 python3 - <<'PY70'
import json, subprocess, time
p = subprocess.Popen(['galgo-mcp'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
p.stdin.write((json.dumps({'jsonrpc':'2.0','id':70,'method':'tools/call','params':{'name':'galgo_wait','arguments':{'target':'w70','timeout_secs':15}}})+'\n').encode()); p.stdin.flush()
time.sleep(.5); start = time.monotonic(); p.stdin.close()
try:
    p.wait(timeout=2)
    assert time.monotonic()-start < 2
    assert p.stdout.read() == b''
    subprocess.run(['galgo','done','--pane','w70','--summary','resultado70'], check=True)
    time.sleep(.3)
    assert b'resultado70' in subprocess.check_output(['galgo','inbox','--pane','m70'])
finally:
    if p.poll() is None: p.kill(); p.wait()
PY70
check "EOF cancela sin confirmar" $?

echo "71. id duplicado se rechaza sin cancelar el wait original"
galgo spawn m71 -- bash --norc >/dev/null
galgo spawn w71 --manager m71 -- bash --norc >/dev/null
GALGO_PANE=m71 python3 - <<'PY71'
import json, os, select, subprocess, time
p = subprocess.Popen(['galgo-mcp'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def send(x): p.stdin.write((json.dumps(x)+'\n').encode()); p.stdin.flush()
def recv():
    assert select.select([p.stdout],[],[],3)[0], 'sin respuesta'
    return json.loads(p.stdout.readline())
try:
    send({'jsonrpc':'2.0','id':71,'method':'tools/call','params':{'name':'galgo_wait','arguments':{'target':'w71','timeout_secs':15}}})
    time.sleep(.3)
    send({'jsonrpc':'2.0','id':71,'method':'ping'})
    assert recv()['error']['code'] == -32600
    subprocess.run(['galgo','done','--pane','w71','--summary','resultado71'], check=True)
    r = recv(); assert r['id'] == 71 and not r['result']['isError']
finally:
    p.kill(); p.wait()
PY71
check "duplicado rechazado y wait original responde" $?

echo "60. wait con shutdown(SHUT_WR): el cliente recibe el resultado"
galgo spawn m60 -- bash --norc >/dev/null
galgo spawn w60 --manager m60 -- bash --norc >/dev/null
( sleep 1; galgo send w60 'galgo done --summary "tarde60"' ) & P60=$!
python3 -I - "$GALGO_SOCKET" > "$TMP/r60" 2>&1 <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((json.dumps({"op": "wait", "waiter": "m60", "target": "w60", "timeout_secs": 10}) + "\n").encode())
s.shutdown(socket.SHUT_WR)
print(s.makefile().readline())
PY
wait $P60
grep -q '"ok":true' "$TMP/r60" && grep -q "tarde60" "$TMP/r60"; check "el cliente recibe el resultado tardío" $?

echo "61. wait y ack en la misma conexión con shutdown(SHUT_WR): ambas respondidas"
galgo spawn m61 -- bash --norc >/dev/null
galgo spawn w61 --manager m61 -- bash --norc >/dev/null
galgo spawn x61 --manager m61 -- bash --norc >/dev/null
galgo send w61 'galgo done --summary "previo61"'
sleep 0.5
M61=$(galgo --json inbox --pane m61 | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')
( sleep 1; galgo send x61 'galgo done --summary "tarde61"' ) & P61=$!
python3 -I - "$GALGO_SOCKET" "$M61" > "$TMP/r61" 2>&1 <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((json.dumps({"op": "wait", "waiter": "m61", "target": "x61", "timeout_secs": 10}) + "\n").encode())
s.sendall((json.dumps({"op": "ack", "pane": "m61", "msg": int(sys.argv[2])}) + "\n").encode())
s.shutdown(socket.SHUT_WR)
f = s.makefile()
print(f.readline())
print(f.readline())
PY
wait $P61
[ "$(grep -c '"ok":true' "$TMP/r61")" = "2" ]; check "ambas solicitudes respondidas" $?
galgo inbox --pane m61 | grep -q previo61; [ $? -ne 0 ]; check "el ack se ejecutó" $?

echo "62. kill mata los trabajos de la sesión que ignoran SIGHUP"
galgo spawn j62 -- bash --norc >/dev/null
galgo send j62 "trap '' HUP; sleep 301 & echo \$! > $TMP/sl62; wait"
sleep 0.7
S62=$(cat "$TMP/sl62")
galgo kill j62 >/dev/null 2>&1
sleep 1
! kill -0 "$S62" 2>/dev/null; check "ningún sleep 301 queda vivo" $?
kill -9 "$S62" 2>/dev/null

echo "63. respuesta que el cliente no lee: vence y libera el mensaje"
mkdir -p "$TMP/s63"
G63="$TMP/s63/g.sock"
GALGO_SOCKET=$G63 GALGO_WRITE_TIMEOUT_SECS=2 galgo spawn m63 -- bash --norc >/dev/null
GALGO_SOCKET=$G63 galgo spawn w63 --manager m63 -- bash --norc >/dev/null
python3 -I - "$G63" > /dev/null 2>&1 <<'PY' & P63=$!
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((json.dumps({"op": "wait", "waiter": "m63", "target": "w63", "timeout_secs": 20}) + "\n").encode())
time.sleep(6)
PY
sleep 0.5
python3 -I - "$G63" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((json.dumps({"op": "done", "pane": "w63", "summary": "x" * 1000000, "failed": False}) + "\n").encode())
s.makefile().readline()
PY
sleep 3
GALGO_SOCKET=$G63 galgo --json wait w63 --pane m63 --timeout 5 > "$TMP/o63" 2>&1
grep -q '"delivered"' "$TMP/o63"; check "tras vencer la escritura, el mensaje vuelve a entregarse" $?
kill "$P63" 2>/dev/null; wait "$P63" 2>/dev/null
GALGO_SOCKET=$G63 galgo stop >/dev/null 2>&1

echo "80. galgo env: pone variables sin pasar por el daemon ni por argv"
E80="$TMP/e80"; mkdir -p "$E80/a" "$E80/b"
printf '# nota\nFOO=1\n' > "$E80/a/.env"
printf 'sk-secreto 80\n' | galgo env set API_KEY --dir "$E80/a" --dir "$E80/b" >/dev/null
grep -q "^API_KEY='sk-secreto 80'\$" "$E80/a/.env" && grep -q '^FOO=1$' "$E80/a/.env"; check "escribe en varios directorios y conserva lo demás" $?
[ "$(stat -f %Lp "$E80/b/.env" 2>/dev/null || stat -c %a "$E80/b/.env")" = "600" ]; check "archivo nuevo con permisos 0600" $?
galgo env list --dir "$E80/a" | grep -q "API_KEY" && ! galgo env list --dir "$E80/a" | grep -q "secreto"; check "list muestra nombres, no valores" $?
galgo spawn m80 -- bash --norc >/dev/null
galgo spawn w80 --manager m80 -- bash --norc >/dev/null
galgo send w80 "cd $E80/b && galgo env request DB_URL --reason 'migraciones' --timeout 15"
sleep 1.5
galgo inbox --pane m80 | grep -q "necesita la variable DB_URL"; check "el manager recibe la petición" $?
galgo ls | grep w80 | grep -q blocked; check "el worker queda blocked mientras espera" $?
printf 'postgres://x\n' | galgo env set DB_URL --dir "$E80/b" >/dev/null
sleep 1.5
galgo read w80 | grep -q "DB_URL disponible"; check "el worker sigue al aparecer la variable" $?
galgo read w80 | grep -q "postgres://x"; [ $? -ne 0 ]; check "el valor nunca aparece en la conversación" $?
galgo inbox --pane m80 | grep -q "postgres://x"; [ $? -ne 0 ]; check "ni en el inbox del manager" $?

echo "81. galgo env: concurrencia, enlaces, multilínea y petición sin quedar blocked"
E81="$TMP/e81"; mkdir -p "$E81"
for i in $(seq 1 20); do ( printf 'v%s\n' "$i" | galgo env set "K$i" --dir "$E81" >/dev/null 2>&1 ) & done
wait
[ "$(grep -c '^K' "$E81/.env")" = "20" ]; check "20 escrituras simultáneas no pierden variables" $?
mkdir -p "$E81/l"; echo X=1 > "$E81/real"; ln -s ../real "$E81/l/.env"
printf 's\n' | galgo env set Y --dir "$E81/l" 2>&1 | grep -q "enlace simbólico"; check "rechaza escribir a través de un enlace" $?
[ "$(cat "$E81/real")" = "X=1" ]; check "el destino del enlace queda intacto" $?
printf 'PRIV="-----BEGIN\nabc/def+ghi=\n-----END"\nZ=1\n' > "$E81/.env2"
galgo env list --dir "$E81" --file .env2 | grep -q "abc"; [ $? -ne 0 ]; check "list no muestra líneas del cuerpo de un valor multilínea" $?
printf 'p`echo PWNED`\n' | galgo env set SAFE --dir "$E81" >/dev/null
( set -a; . "$E81/.env"; set +a; [ "$SAFE" = 'p`echo PWNED`' ] ); check "el valor con comillas invertidas se lee literal con source" $?
galgo spawn m81 -- bash --norc >/dev/null
galgo spawn w81 --manager m81 -- bash --norc >/dev/null
galgo send w81 'galgo env request NOPE --timeout 2 --dir . ; echo FIN81'
sleep 4
galgo ls | grep w81 | grep -q blocked; [ $? -ne 0 ]; check "tras vencer la petición el worker no queda blocked" $?
galgo inbox --pane m81 | grep -c "necesita la variable NOPE" | grep -q '^1$'; check "un solo aviso al manager" $?

echo "90. kill barre la sesión aunque el líder ya haya salido (#12)"
galgo spawn l90 -- bash -c "trap '' HUP; sleep 404 & echo \$! > $TMP/s90; exit 0" >/dev/null
sleep 1
galgo ls | grep l90 | grep -q exited; check "el líder salió" $?
S90=$(cat "$TMP/s90")
kill -0 "$S90" 2>/dev/null; check "el sleep de la sesión sigue vivo antes del kill" $?
galgo kill l90 >/dev/null 2>&1; check "kill con miembros vivos responde ok" $?
sleep 0.5
! kill -0 "$S90" 2>/dev/null; check "kill barre la sesión del líder salido" $?
kill -9 "$S90" 2>/dev/null
galgo kill l90 2>&1 | grep -q "ya terminó"; check "sin miembros, mensaje 'ya terminó'" $?

echo "95. snapshot con estilos y resize (GUI)"
galgo spawn s95 -- bash -c 'printf "\033[31mrojo\033[0m ok\n"; sleep 60' >/dev/null
sleep 1
python3 -I - "$GALGO_SOCKET" <<'PY95'
import socket, json, sys, time
def rq(m):
    s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
    s.send((json.dumps(m) + "\n").encode()); return json.loads(s.makefile().readline())
d = rq({"op": "snapshot", "pane": "s95"})["data"]
r0 = d["lines"][0][0]
assert r0["t"] == "rojo" and r0["fg"] == {"Idx": 1}, r0
assert rq({"op": "resize", "pane": "s95", "rows": 20, "cols": 60})["ok"]
d = rq({"op": "snapshot", "pane": "s95"})["data"]
assert (d["rows"], d["cols"]) == (20, 60), d
assert not rq({"op": "resize", "pane": "s95", "rows": 0, "cols": 60})["ok"]
PY95
check "snapshot trae color por tramo y resize cambia la pantalla" $?
galgo kill s95 >/dev/null 2>&1

echo "96. snapshot con offset: historial, fila desplazada y vuelta a la pantalla viva"
galgo spawn sb96 -- bash --norc >/dev/null
galgo send sb96 'for i in $(seq 1 100); do echo line$i; done'
sleep 1.5
python3 -I - "$GALGO_SOCKET" <<'PY96'
import json, re, socket, sys
def snap(offset):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.sendall((json.dumps({"op": "snapshot", "pane": "sb96", "offset": offset}) + "\n").encode())
    data = json.loads(s.makefile().readline())
    s.close()
    return data["data"]
def top(d):
    m = re.search(r"line(\d+)", "".join(r["t"] for r in d["lines"][0]))
    return int(m.group(1)) if m else None
live = snap(0)
back = snap(5)
clamp = snap(65535)
again = snap(0)
ok = (live["scrollback"] == 0
      and back["scrollback"] == 5
      and back["scrollback_max"] >= 5
      and clamp["scrollback"] == clamp["scrollback_max"]
      and top(live) is not None and top(back) is not None
      and top(live) - top(back) == 5
      and again["scrollback"] == 0 and top(again) == top(live))
sys.exit(0 if ok else 1)
PY96
check "offset 5 sube 5 líneas; un offset grande se recorta al historial; offset 0 vuelve en vivo" $?
galgo kill sb96 >/dev/null 2>&1

echo "97. tamaño de panel acotado: Resize y Spawn rechazan tamaños por encima del tope (M3)"
galgo spawn t97 -- bash --norc >/dev/null
python3 -I - "$GALGO_SOCKET" <<'PY97'
import json, socket, sys
def rq(m):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.sendall((json.dumps(m) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    return r
ok = (not rq({"op": "resize", "pane": "t97", "rows": 501, "cols": 80})["ok"]
      and not rq({"op": "resize", "pane": "t97", "rows": 24, "cols": 1001})["ok"]
      and rq({"op": "resize", "pane": "t97", "rows": 500, "cols": 1000})["ok"])
sp = rq({"op": "spawn", "name": "big97", "cmd": ["bash"], "cwd": None, "manager": None,
         "inject": False, "rows": 600, "cols": 80})
ok = ok and not sp["ok"] and "demasiado grande" in (sp.get("error") or "")
sys.exit(0 if ok else 1)
PY97
check "Resize y Spawn rechazan tamaños por encima del tope" $?

echo "98. ocho Resize concurrentes: el PTY y la pantalla del daemon quedan del mismo tamaño (B1)"
galgo spawn r98 -- bash --norc >/dev/null
sleep 0.5
python3 -I - "$GALGO_SOCKET" "$TMP/r98" <<'PY98'
import json, os, socket, sys, threading, time
path, out = sys.argv[1], sys.argv[2]
def rq(m):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.sendall((json.dumps(m) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    return r
bad = 0
N, ROUNDS = 8, 200
for i in range(ROUNDS):
    gate = threading.Barrier(N)
    def go(r, c):
        gate.wait()
        rq({"op": "resize", "pane": "r98", "rows": r, "cols": c})
    ts = [threading.Thread(target=go, args=(20 + (i * 7 + k) % 9, 60 + (i * 5 + k) % 11)) for k in range(N)]
    for t in ts: t.start()
    for t in ts: t.join()
    if os.path.exists(out): os.remove(out)
    rq({"op": "send", "pane": "r98", "text": f"stty size > {out}", "enter": True})
    for _ in range(100):
        if os.path.exists(out) and open(out).read().strip(): break
        time.sleep(0.02)
    pty = tuple(int(x) for x in open(out).read().split())
    d = rq({"op": "snapshot", "pane": "r98"})["data"]
    if pty != (d["rows"], d["cols"]): bad += 1
print(f"desajustes: {bad}/{ROUNDS}")
sys.exit(0 if bad == 0 else 1)
PY98
check "ningún Resize concurrente deja el PTY y la pantalla desalineados" $?
galgo kill r98 >/dev/null 2>&1

echo "99. snapshot indica bracketed paste mientras la aplicación lo tiene activo (M2)"
galgo spawn bp99 -- bash -c 'printf "\033[?2004h"; sleep 30' >/dev/null
galgo spawn bq99 -- bash -c 'printf "\033[?2004h\033[?2004l"; sleep 30' >/dev/null
sleep 0.7
python3 -I - "$GALGO_SOCKET" <<'PY99'
import json, socket, sys
def snap(p):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.sendall((json.dumps({"op": "snapshot", "pane": p}) + "\n").encode())
    d = json.loads(s.makefile().readline())["data"]
    s.close()
    return d
sys.exit(0 if snap("bp99")["bracketed_paste"] is True and snap("bq99")["bracketed_paste"] is False else 1)
PY99
check "bracketed_paste true con modo 2004 activo, false tras desactivarlo" $?
galgo kill bp99 >/dev/null 2>&1; galgo kill bq99 >/dev/null 2>&1

echo "97. hooks PI: instalación reversible y eventos"
HOME97="$TMP/home97"
mkdir -p "$HOME97/.pi/agent/extensions"
printf '%s\n' 'extensión anterior' > "$HOME97/.pi/agent/extensions/galgo.ts"
printf '%s\n' 'herdr intacto' > "$HOME97/.pi/agent/extensions/herdr-agent-state.ts"
HOME="$HOME97" galgo hooks install pi --yes >/dev/null && HOME="$HOME97" galgo hooks install pi --yes | grep -q 'Ya está instalado'; check "instala PI idempotentemente" $?
[ "$(cat "$HOME97/.pi/agent/extensions/galgo.ts.galgo-bak")" = 'extensión anterior' ] && [ "$(cat "$HOME97/.pi/agent/extensions/herdr-agent-state.ts")" = 'herdr intacto' ]; check "backup y extensión ajena conservados" $?
galgo spawn m97 -- bash --norc >/dev/null
galgo spawn w97 --manager m97 -- bash --norc >/dev/null
printf '%s\n' '{"type":"before_agent_start","messageType":"user"}' | GALGO_PANE=w97 galgo hook pi
printf '%s\n' '{"type":"ui_prompt_start","title":"permiso"}' | GALGO_PANE=w97 galgo hook pi
galgo ls | grep w97 | grep -q blocked; check "PI informa espera de permiso" $?
printf '%s\n' '{"type":"ui_prompt_end"}' | GALGO_PANE=w97 galgo hook pi
galgo ls | grep w97 | grep -q working; check "PI vuelve a working" $?
printf '%s\n' '{"type":"agent_settled","outcome":"completed","summary":"resultado PI"}' | GALGO_PANE=w97 galgo hook pi
galgo inbox --pane m97 | grep -q 'resultado PI'; check "PI entrega resultado al manager" $?
out=$(printf '%s\n' '{"type":"agent_start"}' | env -u GALGO_PANE GALGO_SOCKET="$TMP/no-pi.sock" galgo hook pi 2>&1); rc=$?
[ "$rc" = 0 ] && [ -z "$out" ] && [ ! -S "$TMP/no-pi.sock" ]; check "hook PI silencioso sin panel y daemon" $?
out=$(printf '%s\n' '{"type":"agent_start"}' | GALGO_PANE=w97 GALGO_SOCKET="$TMP/no-pi.sock" galgo hook pi 2>&1); rc=$?
[ "$rc" = 0 ] && [ -z "$out" ] && [ ! -S "$TMP/no-pi.sock" ]; check "hook PI silencioso con daemon ausente" $?
galgo status blocked --pane m97
if command -v pi >/dev/null 2>&1; then
GALGO_PANE=m97 PI_CODING_AGENT_DIR="$TMP/pi-config97" pi --mode rpc --no-session --offline --no-extensions -e "$HOME97/.pi/agent/extensions/galgo.ts" </dev/null >"$TMP/pi97.out" 2>"$TMP/pi97.err"
[ $? = 0 ] && [ ! -s "$TMP/pi97.out" ] && [ ! -s "$TMP/pi97.err" ] && galgo ls | grep m97 | grep -q idle; check "PI real carga extensión y reporta session_start" $?
else check "PI real carga extensión y reporta session_start" 77; fi
HOME="$HOME97" galgo hooks uninstall pi --yes >/dev/null
[ "$(cat "$HOME97/.pi/agent/extensions/galgo.ts")" = 'extensión anterior' ] && [ ! -e "$HOME97/.pi/agent/extensions/galgo.ts.galgo-bak" ]; check "uninstall restaura extensión previa" $?

echo "98. hooks Codex: instalación reversible y eventos reales"
mkdir -p "$TMP/codex-empty-home98" "$TMP/codex-cwd98"
(cd "$TMP/codex-cwd98" && CODEX_HOME= HOME="$TMP/codex-empty-home98" galgo hooks install codex --yes >/dev/null) && [ -f "$TMP/codex-empty-home98/.codex/hooks.json" ] && [ ! -e "$TMP/codex-cwd98/hooks.json" ]; check "CODEX_HOME vacío usa HOME, no cwd" $?
(cd "$TMP/codex-cwd98" && ! CODEX_HOME=rel HOME="$TMP/codex-empty-home98" galgo hooks install codex --yes >/dev/null 2>&1 && [ ! -e rel/hooks.json ]); check "CODEX_HOME relativo se rechaza" $?
HOME98="$TMP/home98"
mkdir -p "$HOME98/.codex"
printf '%s\n' '{"description":"preservar","hooks":{"Stop":[{"hooks":[{"type":"command","command":"true","timeout":2}]}]}}' > "$HOME98/.codex/hooks.json"
cp "$HOME98/.codex/hooks.json" "$TMP/original98.json"
env -u CODEX_HOME HOME="$HOME98" galgo hooks install codex --yes >/dev/null && env -u CODEX_HOME HOME="$HOME98" galgo hooks install codex --yes | grep -q 'ya está instalado'; check "instala Codex idempotentemente" $?
cmp -s "$HOME98/.codex/hooks.json.galgo-bak" "$TMP/original98.json"; check "Codex respalda configuración previa" $?
galgo spawn m98 -- bash --norc >/dev/null
galgo spawn w98 --manager m98 -- bash --norc >/dev/null
printf '%s\n' '{"hook_event_name":"UserPromptSubmit"}' | GALGO_PANE=w98 galgo hook codex
galgo ls | grep w98 | grep -q working; check "Codex informa prompt_submitted" $?
printf '%s\n' '{"hook_event_name":"SessionStart","source":"compact"}' | GALGO_PANE=w98 galgo hook codex
galgo ls | grep w98 | grep -q working; check "Codex compact no cambia working a idle" $?
printf '%s\n' '{"hook_event_name":"Stop","last_assistant_message":"resultado Codex"}' | GALGO_PANE=w98 galgo hook codex
galgo inbox --pane m98 | grep -q 'resultado Codex'; check "Codex entrega resultado al manager" $?
out=$(printf '%s\n' '{"hook_event_name":"SessionStart"}' | env -u GALGO_PANE GALGO_SOCKET="$TMP/no-codex.sock" galgo hook codex 2>&1); rc=$?
[ "$rc" = 0 ] && [ -z "$out" ] && [ ! -S "$TMP/no-codex.sock" ]; check "hook Codex silencioso sin panel ni daemon" $?
galgo spawn z98 --manager m98 -- bash --norc >/dev/null
printf '%s\n' '{"hook_event_name":"UserPromptSubmit"}' | GALGO_PANE=z98 galgo hook codex
galgo done --pane z98 --summary 'done explícito' >/dev/null
printf '%s\n' '{"hook_event_name":"Stop","last_assistant_message":"duplicado inesperado"}' | GALGO_PANE=z98 galgo hook codex
! galgo inbox --pane m98 | grep -q 'duplicado inesperado'; check "Codex Stop no duplica galgo done" $?
galgo status blocked --pane z98
HOME98="$HOME98" python3 -I - "$GALGO_SOCKET" <<'PY98'
import shutil, sys
if not shutil.which('codex'): sys.exit(77)
import http.server, json, os, pathlib, subprocess, sys, threading
home = pathlib.Path(os.environ['HOME98'])
class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length', '0')))
        msg = {'id':'msg_mock','type':'message','status':'completed','role':'assistant','content':[{'type':'output_text','text':'respuesta real Codex','annotations':[]}]}
        response = {'id':'resp_mock','object':'response','status':'completed','model':'mock','output':[msg],'usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}
        events = [
            {'type':'response.created','response':dict(response,status='in_progress',output=[])},
            {'type':'response.output_item.added','output_index':0,'item':dict(msg,status='in_progress',content=[])},
            {'type':'response.output_text.delta','item_id':'msg_mock','output_index':0,'content_index':0,'delta':'respuesta real Codex'},
            {'type':'response.output_text.done','item_id':'msg_mock','output_index':0,'content_index':0,'text':'respuesta real Codex'},
            {'type':'response.output_item.done','output_index':0,'item':msg},
            {'type':'response.completed','response':response},
        ]
        body = (''.join('event: '+e['type']+'\ndata: '+json.dumps(e)+'\n\n' for e in events)+'data: [DONE]\n\n').encode()
        self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args): pass
server = http.server.HTTPServer(('127.0.0.1',0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
(home/'.codex/config.toml').write_text('model = "mock"\nmodel_provider = "mock"\n[model_providers.mock]\nname = "mock"\nbase_url = "http://127.0.0.1:%d/v1"\nwire_api = "responses"\nrequires_openai_auth = false\nsupports_websockets = false\n' % server.server_port)
env = dict(os.environ, HOME=str(home), CODEX_HOME=str(home/'.codex'), GALGO_PANE='z98', GALGO_SOCKET=sys.argv[1])
try:
    run = subprocess.run(['codex','exec','--skip-git-repo-check','--dangerously-bypass-hook-trust','--json','test'], cwd=home, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=35)
    inbox = subprocess.check_output(['galgo','inbox','--pane','m98'], env=env).decode()
    if run.returncode != 0 or 'respuesta real Codex' not in inbox:
        print('Codex exit:', run.returncode, 'stdout:', run.stdout.decode(errors='replace')[-3000:], 'stderr:', run.stderr.decode(errors='replace')[-3000:], 'inbox:', inbox)
        sys.exit(1)
finally:
    server.shutdown()
PY98
check "Codex real carga hooks y entrega Stop con servidor simulado" $?
env -u CODEX_HOME HOME="$HOME98" galgo hooks uninstall codex --yes >/dev/null
python3 -I - "$HOME98/.codex/hooks.json" "$TMP/original98.json" <<'PY98CHECK'
import json,sys
with open(sys.argv[1]) as a, open(sys.argv[2]) as b: sys.exit(0 if json.load(a) == json.load(b) else 1)
PY98CHECK
check "uninstall preserva hooks originales" $?
env -u CODEX_HOME HOME="$HOME98" galgo hooks uninstall codex --yes | grep -q 'No hay hooks'; check "uninstall Codex idempotente" $?

echo "99. skill Claude: instalación reversible y protección de cambios"
HOME99="$TMP/home-skill99"
SKILL99="$HOME99/.claude/skills/galgo/SKILL.md"
mkdir -p "$HOME99/.claude/skills/galgo" "$HOME99/.claude/skills/otra"
printf '%s\n' 'skill anterior' > "$SKILL99"
printf '%s\n' 'otra intacta' > "$HOME99/.claude/skills/otra/SKILL.md"
HOME="$HOME99" galgo skill install claude --yes >/dev/null && HOME="$HOME99" galgo skill install claude --yes | grep -q 'Ya está instalada'; check "instala skill Claude idempotentemente" $?
[ "$(cat "$SKILL99.galgo-bak")" = 'skill anterior' ] && [ "$(cat "$HOME99/.claude/skills/otra/SKILL.md")" = 'otra intacta' ]; check "respaldo y skill ajena conservados" $?
HOME="$HOME99" galgo skill status claude | grep -q 'instalada' && grep -q '^name: galgo$' "$SKILL99" && grep -q 'galgo_wait' "$SKILL99" && grep -q 'galgo env request' "$SKILL99"; check "status y contenido de skill" $?
cp "$SKILL99" "$TMP/skill99-installed"
printf '\n%s\n' 'edición del usuario' >> "$SKILL99"
HOME="$HOME99" galgo skill status claude | grep -q modificada && ! HOME="$HOME99" galgo skill uninstall claude --yes >/dev/null 2>&1 && grep -q 'edición del usuario' "$SKILL99" && [ "$(cat "$SKILL99.galgo-bak")" = 'skill anterior' ]; check "no sobrescribe skill editada ni respaldo" $?
cp "$TMP/skill99-installed" "$SKILL99"
HOME="$HOME99" galgo skill uninstall claude --yes >/dev/null && [ "$(cat "$SKILL99")" = 'skill anterior' ] && [ ! -e "$SKILL99.galgo-bak" ]; check "uninstall restaura el archivo previo" $?
HOME="$HOME99" galgo skill uninstall claude --yes | grep -q 'No hay skill gestionada'; check "uninstall idempotente preserva archivo ajeno" $?

echo "100. el daemon no hereda marcadores de Claude Code en el entorno de los paneles"
mkdir -p "$TMP/cc100"
CLAUDE_CODE_CHILD_SESSION=1 CLAUDECODE=1 GALGO_SOCKET="$TMP/cc100/g.sock" galgo ls >/dev/null 2>&1
GALGO_SOCKET="$TMP/cc100/g.sock" galgo spawn x100 -- sh -c "env > $TMP/env100"
sleep 0.5
! grep -q "CLAUDE_CODE_CHILD_SESSION\|^CLAUDECODE=" "$TMP/env100"; check "el hijo no ve CLAUDE_CODE_CHILD_SESSION ni CLAUDECODE" $?
grep -q "^HOME=" "$TMP/env100"; check "el resto del entorno sigue llegando" $?
GALGO_SOCKET="$TMP/cc100/g.sock" galgo stop >/dev/null 2>&1

echo "101. snapshot marca las filas que continúan (soft wrap) (M2)"
galgo spawn wr101 --cwd "$TMP" -- bash -c "printf '%0200d\n' 0; sleep 30" >/dev/null
sleep 0.7
python3 -I - "$GALGO_SOCKET" <<'PY101'
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((json.dumps({"op": "snapshot", "pane": "wr101"}) + "\n").encode())
d = json.loads(s.makefile().readline())["data"]
w = d["wrapped"]
sys.exit(0 if w[0] is True and w[1] is False and len(w) == d["rows"] else 1)
PY101
check "la fila de 200 ceros continúa en la siguiente; la siguiente no" $?
galgo kill wr101 >/dev/null 2>&1

echo "110. snapshot con generación: since devuelve unchanged solo si nada cambió; Resize y escritura la suben"
galgo spawn gp110 -- bash --norc >/dev/null
sleep 0.7
python3 -I - "$GALGO_SOCKET" <<'PY110'
import json, socket, sys
def rq(m):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.sendall((json.dumps(m) + "\n").encode())
    line = s.makefile("rb").readline()
    s.close()
    return line
full = json.loads(rq({"op": "snapshot", "pane": "gp110"}))["data"]
g1 = full["gen"]
same = json.loads(rq({"op": "snapshot", "pane": "gp110", "since": g1}))["data"]
ok = same == {"unchanged": True, "gen": g1}
rq({"op": "send", "pane": "gp110", "text": "echo hola110", "enter": True})
import time; time.sleep(0.5)
after_write = json.loads(rq({"op": "snapshot", "pane": "gp110", "since": g1}))["data"]
ok = ok and "rows" in after_write and after_write["gen"] > g1
g2 = after_write["gen"]
rq({"op": "resize", "pane": "gp110", "rows": 30, "cols": 100})
after_resize = json.loads(rq({"op": "snapshot", "pane": "gp110", "since": g2}))["data"]
ok = ok and "rows" in after_resize and after_resize["gen"] > g2 and after_resize["rows"] == 30
# Un tramo sin estilo no escribe fg/bg ni los booleanos en falso.
raw = rq({"op": "snapshot", "pane": "gp110"}).decode()
ok = ok and '"fg":"Default"' not in raw and '"bold":false' not in raw
sys.exit(0 if ok else 1)
PY110
check "unchanged mientras no hay cambios; escritura y resize suben la generación; tramos compactos" $?
galgo kill gp110 >/dev/null 2>&1

echo "111. el inbox sobrevive a un reinicio del daemon; un ack anterior confirma su mensaje, no uno nuevo"
galgo spawn m111 -- bash --norc >/dev/null
galgo spawn w111 --manager m111 -- bash --norc >/dev/null
galgo send w111 'galgo done --summary "viejo111"'
sleep 0.7
OLD=$(galgo --json inbox --pane m111 | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')
galgo stop >/dev/null 2>&1
sleep 0.5
galgo ls >/dev/null
galgo inbox --pane m111 | grep -q "viejo111"; check "el resultado sigue en el inbox tras reiniciar" $?
galgo inbox --pane m111 | grep -q "ya no existe"; check "el manager recibe el aviso de que w111 ya no existe" $?
galgo spawn w111b --manager m111 -- bash --norc >/dev/null
galgo send w111b 'galgo done --summary "nuevo111"'
sleep 0.7
galgo ack "$OLD" --pane m111 >/dev/null 2>&1; check "el ack de antes del reinicio se acepta" $?
galgo inbox --pane m111 | grep -q "nuevo111"; check "ese ack no confirmó el mensaje nuevo" $?
galgo inbox --pane m111 | grep -q "viejo111"; [ $? -ne 0 ]; check "el mensaje anterior quedó confirmado" $?
FOREIGN=$(( (7 << 32) + 5 ))
galgo ack "$FOREIGN" --pane m111 2>&1 | grep -q "otra ejecución"; check "un id de otra ejecución se rechaza sin confirmar" $?
galgo inbox --pane m111 | grep -q "nuevo111"; check "tras el rechazo el mensaje nuevo sigue pendiente" $?

echo "112. entorno de paneles: sin marcadores de sesión, con la configuración del usuario (M2, M3); tokens de vista (B1, B3)"
mkdir -p "$TMP/env112"
CLAUDE_CODE_USE_BEDROCK=1 CLAUDE_PID=99 HERDR_PANE_ID=5 HERDR_ENV=1 CLAUDE_CODE_CHILD_SESSION=1 GALGO_SOCKET="$TMP/env112/g.sock" galgo ls >/dev/null 2>&1
GALGO_SOCKET="$TMP/env112/g.sock" galgo spawn e112 -- sh -c "env > $TMP/env112.out; sleep 30" >/dev/null
sleep 0.7
grep -qx 'CLAUDE_CODE_USE_BEDROCK=1' "$TMP/env112.out"; check "el panel conserva la configuración del usuario (proveedor)" $?
! grep -q '^CLAUDE_PID=\|^HERDR_\|^CLAUDE_CODE_CHILD_SESSION=' "$TMP/env112.out"; check "el panel no hereda marcadores de sesión ni de herdr" $?
python3 -I - "$TMP/env112/g.sock" <<'PY112'
import json, socket, sys
def rq(m):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    s.sendall((json.dumps(m) + "\n").encode())
    line = s.makefile("rb").readline()
    s.close()
    return json.loads(line)["data"]
first = rq({"op": "snapshot", "pane": "e112"})
token = first["gen"]
ok = token >> 16 > 1000000
ok = ok and "rows" in rq({"op": "snapshot", "pane": "e112", "offset": 30, "since": token})
ok = ok and rq({"op": "snapshot", "pane": "e112", "since": token}) == {"unchanged": True, "gen": token}
sys.exit(0 if ok else 1)
PY112
check "el token de vista sale de la hora de arranque y un offset distinto no devuelve unchanged" $?
GALGO_SOCKET="$TMP/env112/g.sock" galgo stop >/dev/null 2>&1

echo "113. workspaces: spawn, ls agrupado y filtrado, deducción desde git, estado anterior sin el campo"
galgo spawn ws-a --workspace "inventia/cotizacion" -- bash --norc >/dev/null
galgo spawn ws-b --workspace "galgo/main" -- bash --norc >/dev/null
galgo ls | grep -q "── inventia/cotizacion (1)"; check "ls agrupa por workspace" $?
galgo ls --workspace "galgo/main" | grep -q "ws-b"; check "ls --workspace muestra los del workspace" $?
galgo ls --workspace "galgo/main" | grep -q "ws-a"; [ $? -ne 0 ]; check "ls --workspace oculta los demás" $?
galgo --json ls --workspace "inventia/cotizacion" | grep -q '"workspace": "inventia/cotizacion"'; check "--json trae el workspace" $?
REPO113="$TMP/repo113"
mkdir -p "$REPO113"
git -C "$REPO113" init -q -b feat-113
git -C "$REPO113" -c user.email=t@t -c user.name=t commit -q --allow-empty -m x
galgo spawn ws-git --cwd "$REPO113" -- bash --norc >/dev/null
galgo --json ls | python3 -c 'import json,sys; p=[x for x in json.load(sys.stdin) if x["name"]=="ws-git"][0]; sys.exit(0 if p["workspace"].endswith("/feat-113") and p["workspace"].startswith("repo113") else 1)'
check "sin --workspace se deduce <repo>/<rama> de git" $?
mkdir -p "$TMP/c113"
printf '%s' '{"version":1,"epoch":1,"next_pane":1,"next_msg":0,"panes":[{"id":1,"name":"viejo","cmd":["bash"],"manager":null,"alive":false,"inbox":[]}]}' > "$TMP/c113/g.state.json"
GALGO_SOCKET="$TMP/c113/g.sock" galgo ls | grep -q "viejo"; check "un estado anterior sin campo de workspace se carga" $?
GALGO_SOCKET="$TMP/c113/g.sock" galgo stop >/dev/null 2>&1

echo "120. M3: un Send no reescribe el estado; un cambio del inbox sí, agrupado"
galgo spawn mw120 -- bash --norc >/dev/null
galgo spawn sw120 --manager mw120 -- bash --norc >/dev/null
sleep 0.8
ST="$TMP/galgo.state.json"
M0=$(python3 -c "import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)" "$ST")
for i in $(seq 1 200); do galgo send sw120 "x" --no-enter >/dev/null; done
sleep 0.5
M1=$(python3 -c "import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)" "$ST")
[ "$M0" = "$M1" ]; check "200 Send seguidos no reescriben state.json" $?
galgo send sw120 $'\x15' --no-enter >/dev/null
galgo send sw120 'galgo done --summary "cambio120"' >/dev/null
sleep 0.8
M2=$(python3 -c "import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)" "$ST")
[ "$M1" != "$M2" ]; check "un resultado sí se guarda" $?

echo "121. M2: sin state.json (o corrupto) el epoch no vuelve a 1; un ack de antes no confirma uno nuevo"
mkdir -p "$TMP/c121" && chmod 700 "$TMP/c121"
export GALGO_SOCKET="$TMP/c121/g.sock"
galgo spawn m121 -- bash --norc >/dev/null
galgo spawn w121 --manager m121 -- bash --norc >/dev/null
galgo send w121 'galgo done --summary "viejo121"' >/dev/null
sleep 0.8
OLD121=$(galgo --json inbox --pane m121 | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')
galgo stop >/dev/null 2>&1; sleep 0.5
printf '%s' '{corrupto' > "$TMP/c121/g.state.json"
galgo ls >/dev/null
ls "$TMP/c121" | grep -q "corrupt"; check "el estado corrupto se aparta, no se sobrescribe" $?
galgo spawn m121b -- bash --norc >/dev/null 2>&1
galgo spawn w121b --manager m121b -- bash --norc >/dev/null
galgo send w121b 'galgo done --summary "nuevo121"' >/dev/null
sleep 0.8
galgo ack "$OLD121" --pane m121b >/dev/null 2>&1; [ $? -ne 0 ]; check "el ack de antes del corrupto se rechaza" $?
galgo inbox --pane m121b | grep -q "nuevo121"; check "el mensaje nuevo sigue pendiente" $?
unset GALGO_SOCKET
galgo stop >/dev/null 2>&1 || true
export GALGO_SOCKET="$TMP/galgo.sock"

echo "122. M1: un fantasma con mensajes pendientes deja reutilizar su nombre (hereda el inbox) y se puede quitar"
mkdir -p "$TMP/c122" && chmod 700 "$TMP/c122"
export GALGO_SOCKET="$TMP/c122/g.sock"
galgo spawn m122 -- bash --norc >/dev/null
galgo spawn g122 --manager m122 -- bash --norc >/dev/null
galgo spawn w122 --manager g122 -- bash --norc >/dev/null
galgo send w122 'galgo done --summary "resultado122"' >/dev/null
sleep 0.8
galgo stop >/dev/null 2>&1; sleep 0.5
galgo ls >/dev/null
galgo spawn g122 --manager m122 -- bash --norc >/dev/null 2>&1; check "un fantasma con mensajes no bloquea su nombre" $?
galgo inbox --pane g122 | grep -q "resultado122"; check "el nombre reutilizado hereda el inbox pendiente" $?
galgo stop >/dev/null 2>&1; sleep 0.5
galgo ls >/dev/null
galgo kill g122 >/dev/null 2>&1; check "kill quita un fantasma" $?
galgo inbox --pane m122 | grep -q "resultado122"; check "su resultado pasa al manager" $?
galgo ls | grep -q " g122 "; [ $? -ne 0 ]; check "el fantasma desaparece de ls" $?
unset GALGO_SOCKET
galgo stop >/dev/null 2>&1 || true
export GALGO_SOCKET="$TMP/galgo.sock"

echo "123. B2: dos daemons en el mismo directorio no comparten estado"
mkdir -p "$TMP/c123" && chmod 700 "$TMP/c123"
GALGO_SOCKET="$TMP/c123/a.sock" galgo spawn soloA -- bash --norc >/dev/null
GALGO_SOCKET="$TMP/c123/b.sock" galgo spawn soloB -- bash --norc >/dev/null
sleep 0.8
grep -q soloA "$TMP/c123/a.state.json" && ! grep -q soloB "$TMP/c123/a.state.json"; check "el estado de A solo tiene sus paneles" $?
grep -q soloB "$TMP/c123/b.state.json" && ! grep -q soloA "$TMP/c123/b.state.json"; check "el estado de B solo tiene sus paneles" $?
GALGO_SOCKET="$TMP/c123/a.sock" galgo stop >/dev/null 2>&1; GALGO_SOCKET="$TMP/c123/b.sock" galgo stop >/dev/null 2>&1

echo "124. B3: un state.json que no es un archivo regular no cuelga el arranque"
mkdir -p "$TMP/c124" && chmod 700 "$TMP/c124"
mkfifo "$TMP/c124/g.state.json"
GALGO_SOCKET="$TMP/c124/g.sock" galgo ls >/dev/null 2>&1 &
LS124=$!
sleep 3
kill -0 "$LS124" 2>/dev/null; [ $? -ne 0 ]; check "galgo ls responde con un FIFO en el lugar del estado" $?
kill "$LS124" 2>/dev/null; wait "$LS124" 2>/dev/null
GALGO_SOCKET="$TMP/c124/g.sock" galgo stop >/dev/null 2>&1

echo "125. uso vivo: un informe del agente (statusline) llega a ls y a galgo usage; sin rutas en la salida"
mkdir -p "$TMP/c125" && chmod 700 "$TMP/c125"
export GALGO_SOCKET="$TMP/c125/g.sock"
rpc() { python3 -c 'import json,socket,sys,os
s=socket.socket(socket.AF_UNIX); s.connect(os.environ["GALGO_SOCKET"]); s.sendall((sys.argv[1]+"\n").encode()); buf=b""
while not buf.endswith(b"\n"):
    d=s.recv(65536)
    if not d: break
    buf+=d
print(buf.decode().strip())' "$1"; }
galgo spawn u125 -- bash --norc >/dev/null
RESET=$(( $(date +%s) + 8040 ))
rpc "{\"op\":\"usage_report\",\"pane\":\"u125\",\"agent\":\"claude\",\"model\":\"opus-5.5\",\"context_tokens\":598000,\"context_window\":1000000,\"cost_usd\":7.07,\"limits\":[{\"name\":\"five_hour\",\"used_pct\":15.0,\"resets_at\":$RESET},{\"name\":\"seven_day\",\"used_pct\":14.0,\"resets_at\":null}]}" | grep -q '"ok":true'; check "el informe del agente se acepta" $?
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="u125"][0]["usage"]
assert abs(p["context_pct"]-59.8)<0.01 and p["cost_source"]=="statusline" and p["cost_usd"]==7.07 and p["model"]=="opus-5.5"'
check "ls trae contexto, coste y origen del informe" $?
OUT=$(galgo usage)
echo "$OUT" | grep -qF "60% 598k" && echo "$OUT" | grep -qF "7.07 \$ (statusline)"; check "galgo usage muestra contexto y coste con origen" $?
LIM=$(galgo usage --limits)
echo "$LIM" | grep -qF "5h 15%" && echo "$LIM" | grep -qF "7d 14%" && echo "$LIM" | grep -qF "reinicia en"; check "galgo usage --limits muestra cuotas con reinicio" $?
galgo usage --by agent | grep -q claude; check "galgo usage --by agent agrupa por agente" $?
galgo usage --by model | grep -qF "opus-5.5"; check "galgo usage --by model agrupa por modelo" $?
ALL=$(galgo usage; galgo --json usage; galgo usage --limits; galgo usage --by agent)
! echo "$ALL" | grep -qF "$TMP" && ! echo "$ALL" | grep -qF "cwd" && ! echo "$ALL" | grep -qF "/Users"; check "las salidas de galgo usage no contienen rutas" $?

echo "126. uso desde el transcript: un Hook con fuente se sigue y el sondeo recoge líneas nuevas; state.json no guarda uso"
mkdir -p "$TMP/c126" && chmod 700 "$TMP/c126"
export GALGO_SOCKET="$TMP/c126/g.sock"
TR="$TMP/c126/claude.jsonl"
printf '%s\n' '{"type":"assistant","isSidechain":false,"requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":0}}}' > "$TR"
galgo spawn t126 -- bash --norc >/dev/null
rpc "{\"op\":\"hook\",\"pane\":\"t126\",\"event\":\"working\",\"summary\":null,\"source\":{\"agent\":\"claude\",\"session_id\":\"s1\",\"transcript\":\"$TR\"}}" | grep -q '"ok":true'; check "el hook con fuente se acepta" $?
sleep 2.5
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="t126"][0]["usage"]
assert p["input"]==1000 and p["output"]==200 and p["cache_read"]==500'
check "el transcript alimenta el uso del panel" $?
printf '%s\n' '{"type":"assistant","isSidechain":false,"requestId":"r2","message":{"id":"m2","model":"claude-opus-5-5","usage":{"input_tokens":300,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}' >> "$TR"
sleep 2.5
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="t126"][0]["usage"]
assert p["input"]==1300'
check "el sondeo recoge líneas nuevas sin un hook nuevo" $?
rpc '{"op":"usage_report","pane":"t126","agent":"claude","cost_usd":9.99}' | grep -q '"ok":true'
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="t126"][0]["usage"]
assert p["cost_usd"]==9.99 and p["cost_source"]=="statusline"'
check "un informe del agente pisa el coste del transcript" $?
! grep -q "claude.jsonl" "$TMP/c126/g.state.json" && ! grep -q '"usage"' "$TMP/c126/g.state.json" && ! grep -q '"input"' "$TMP/c126/g.state.json"; check "state.json no guarda uso ni la ruta del transcript" $?

echo "127. galgo usage: errores claros; el uso vive solo en memoria y no vuelve tras reiniciar"
galgo usage --pane nope >/dev/null 2>&1; [ $? -ne 0 ]; check "--pane con un panel que no existe falla" $?
galgo usage --by agent --pane t126 >/dev/null 2>&1; [ $? -ne 0 ]; check "--by y --pane no se combinan" $?
galgo usage --by quien >/dev/null 2>&1; [ $? -ne 0 ]; check "--by solo acepta workspace, agent o model" $?
galgo stop >/dev/null 2>&1; sleep 0.5
galgo ls >/dev/null
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="t126"][0]
assert p["usage"] is None'
check "tras reiniciar el daemon, el uso no vuelve del disco" $?
galgo stop >/dev/null 2>&1
export GALGO_SOCKET="$TMP/galgo.sock"

# PID del daemon que escucha en un socket (lsof lista también a los clientes; se filtra por nombre).
daemon_pid() {
  for p in $(lsof -t "$1" 2>/dev/null); do
    if ps -o comm= -p "$p" 2>/dev/null | grep -q galgod; then echo "$p"; return; fi
  done
}

echo "130. M1: una confirmación (ack o wait) queda en disco; un SIGKILL después no la hace reaparecer"
mkdir -p "$TMP/c130" && chmod 700 "$TMP/c130"
export GALGO_SOCKET="$TMP/c130/g.sock"
galgo spawn m130 -- bash --norc >/dev/null
galgo spawn w130 --manager m130 -- bash --norc >/dev/null
galgo spawn w130b --manager m130 -- bash --norc >/dev/null
galgo done --pane w130 --summary "ackeado130" >/dev/null
galgo done --pane w130b --summary "waitres130" >/dev/null
( galgo wait w130b --pane m130 --timeout 10 > "$TMP/c130/wait" 2>&1 ) &
sleep 0.5
galgo ack "$(galgo --json inbox --pane m130 | python3 -c 'import json,sys; print([m["id"] for m in json.load(sys.stdin) if "ackeado130" in m["summary"]][0])')" --pane m130 >/dev/null
wait
grep -q "waitres130" "$TMP/c130/wait"; check "wait entrega el resultado" $?
sleep 0.5
kill -9 "$(daemon_pid "$GALGO_SOCKET")" 2>/dev/null; sleep 0.5
galgo ls >/dev/null
! galgo inbox --pane m130 | grep -q "ackeado130\|waitres130"; check "lo confirmado no reaparece tras SIGKILL" $?
galgo stop >/dev/null 2>&1; sleep 0.3

echo "131. M2: un done respondido con éxito sobrevive a un SIGTERM inmediato"
mkdir -p "$TMP/c131" && chmod 700 "$TMP/c131"
export GALGO_SOCKET="$TMP/c131/g.sock"
galgo spawn m131 -- bash --norc >/dev/null
galgo spawn w131 --manager m131 -- bash --norc >/dev/null
galgo done --pane w131 --summary "resultadoB131" >/dev/null
kill -TERM "$(daemon_pid "$GALGO_SOCKET")" 2>/dev/null; sleep 0.8
galgo ls >/dev/null
galgo inbox --pane m131 | grep -q "resultadoB131"; check "el resultado sigue tras SIGTERM justo después del done" $?
galgo stop >/dev/null 2>&1; sleep 0.3

echo "132. M3: un git que no responde no detiene al daemon: ls responde mientras el spawn espera"
mkdir -p "$TMP/c132/bin" && chmod 700 "$TMP/c132" && printf '#!/bin/sh\nexec sleep 30\n' > "$TMP/c132/bin/git" && chmod +x "$TMP/c132/bin/git"
export GALGO_SOCKET="$TMP/c132/g.sock"
PATH="$TMP/c132/bin:$PATH" galgo spawn s132 -- bash --norc > "$TMP/c132/spawn" 2>&1 &
SPAWN132=$!
sleep 0.5
MS=$(python3 -c 'import subprocess,time; t=time.time(); subprocess.run(["galgo","ls"],capture_output=True); print(int((time.time()-t)*1000))')
[ "$MS" -lt 1000 ]; check "ls responde en menos de 1 s con un git colgado ($MS ms)" $?
wait $SPAWN132; check "el spawn termina sin workspace tras el tiempo límite" $?
galgo stop >/dev/null 2>&1; sleep 0.3

echo "133. B2: un workspace con caracteres de control se rechaza"
mkdir -p "$TMP/c133" && chmod 700 "$TMP/c133"
export GALGO_SOCKET="$TMP/c133/g.sock"
galgo spawn x133 --workspace $'ws\e[31mROJO' -- bash --norc >/dev/null 2>"$TMP/c133/err"; [ $? -ne 0 ]; check "un workspace con ESC no se crea" $?
grep -q "caracteres de control" "$TMP/c133/err"; check "el error dice por qué" $?
galgo stop >/dev/null 2>&1; sleep 0.3

echo "134. B3: un fantasma reutilizado con el mismo manager hereda sin inyectar; con otro manager, sus pendientes van al suyo"
mkdir -p "$TMP/c134" && chmod 700 "$TMP/c134"
export GALGO_SOCKET="$TMP/c134/g.sock"
galgo spawn m134 -- bash --norc >/dev/null
galgo spawn w134 --manager m134 -- bash --norc >/dev/null
galgo spawn c134 --manager w134 -- bash --norc >/dev/null
galgo done --pane c134 --summary "resultado134" >/dev/null
galgo spawn w134b --manager m134 -- bash --norc >/dev/null
galgo spawn c134b --manager w134b -- bash --norc >/dev/null
galgo done --pane c134b --summary "resultado134b" >/dev/null
sleep 0.8
galgo stop >/dev/null 2>&1; sleep 0.5
galgo ls >/dev/null
galgo spawn w134 --manager m134 --inject -- bash --norc >"$TMP/c134/same" 2>&1
grep -q "mensaje(s) de la ejecución anterior de w134" "$TMP/c134/same"; check "el panel reutilizado avisa de lo heredado" $?
galgo inbox --pane w134 | grep -q "resultado134"; check "hereda el mensaje del mismo manager" $?
sleep 3
! galgo read w134 | grep -q "resultado134"; check "lo heredado no se escribe en el terminal" $?
galgo spawn m134x -- bash --norc >/dev/null
galgo spawn w134b --manager m134x -- bash --norc >/dev/null 2>&1
galgo inbox --pane m134 | grep -q "resultado134b"; check "con otro manager, el pendiente va a su manager anterior" $?
! galgo inbox --pane w134b | grep -q "resultado134b"; check "el panel nuevo no hereda lo de otro rol" $?
galgo stop >/dev/null 2>&1; sleep 0.3

echo "135. M4: un worker apuntado a un fantasma sigue entregando cuando el manager se recrea"
mkdir -p "$TMP/c135" && chmod 700 "$TMP/c135"
export GALGO_SOCKET="$TMP/c135/g.sock"
galgo spawn m135 -- bash --norc >/dev/null
galgo spawn w135 --manager m135 -- bash --norc >/dev/null
galgo stop >/dev/null 2>&1; sleep 0.5
galgo ls >/dev/null
galgo spawn w135b --manager m135 -- bash --norc >/dev/null 2>&1
galgo spawn m135 -- bash --norc >/dev/null 2>&1; check "el manager se recrea con su nombre" $?
galgo done --pane w135 --summary "resultado135" >/dev/null 2>"$TMP/c135/done"; [ $? -eq 0 ]; check "el done del worker se acepta" $?
galgo inbox --pane m135 | grep -q "resultado135"; check "el resultado llega al manager recreado" $?
galgo stop >/dev/null 2>&1; sleep 0.3

echo "136. M5: el estado de la versión anterior (state.json junto al socket) se migra sin perder pendientes"
mkdir -p "$TMP/c136" && chmod 700 "$TMP/c136"
export GALGO_SOCKET="$TMP/c136/g.sock"
galgo spawn m136 -- bash --norc >/dev/null
galgo spawn w136 --manager m136 -- bash --norc >/dev/null
galgo done --pane w136 --summary "pendiente136" >/dev/null
galgo stop >/dev/null 2>&1; sleep 0.5
mv "$TMP/c136/g.state.json" "$TMP/c136/state.json"
galgo ls >/dev/null
galgo inbox --pane m136 | grep -q "pendiente136"; check "el pendiente de la versión anterior se conserva" $?
ls "$TMP/c136" | grep -q "^state.json.migrated-"; check "el archivo anterior queda renombrado, no se vuelve a cargar" $?
[ ! -e "$TMP/c136/state.json" ]; check "state.json ya no está en su sitio" $?
galgo stop >/dev/null 2>&1; sleep 0.3
export GALGO_SOCKET="$TMP/galgo.sock"

echo "140. statusline Claude: encadenamiento, privacidad y restauración"
HOME130="$TMP/home130"; mkdir -p "$HOME130/.claude"
printf '%s\n' '{"statusLine":{"type":"command","command":"cat","padding":3},"model":"conservar"}' > "$HOME130/.claude/settings.json"
env -u CLAUDE_CONFIG_DIR HOME="$HOME130" galgo hooks install claude --statusline --yes >/dev/null
cp "$HOME130/.claude/settings.json" "$TMP/statusline130-installed"
env -u CLAUDE_CONFIG_DIR HOME="$HOME130" galgo hooks install claude --statusline --yes >/dev/null
cmp -s "$HOME130/.claude/settings.json" "$TMP/statusline130-installed"; check "statusline instalador idempotente" $?
printf '%s\n' '{"model":{"id":"claude-opus-5-5"},"cost":{"total_cost_usd":2.75},"context_window":{"context_window_size":1000,"current_usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3}},"rate_limits":{"five_hour":{"used_percentage":20,"resets_at":2000000000}},"private":"SECRETO"}' > "$TMP/statusline130-input"
env -u GALGO_PANE HOME="$HOME130" galgo statusline claude --backup "$HOME130/.claude/settings.json.galgo-statusline" < "$TMP/statusline130-input" > "$TMP/statusline130-output"
cmp -s "$TMP/statusline130-input" "$TMP/statusline130-output"; check "statusline previo recibe stdin y devuelve stdout exacto sin panel" $?
GALGO_SOCKET="$TMP/no-statusline.sock" GALGO_PANE=status130 HOME="$HOME130" galgo statusline claude --backup "$HOME130/.claude/settings.json.galgo-statusline" < "$TMP/statusline130-input" > "$TMP/statusline130-output"
cmp -s "$TMP/statusline130-input" "$TMP/statusline130-output" && [ ! -S "$TMP/no-statusline.sock" ]; check "statusline previo funciona sin arrancar daemon ausente" $?
galgo spawn status130 -- bash --norc >/dev/null
GALGO_PANE=status130 HOME="$HOME130" galgo statusline claude --backup "$HOME130/.claude/settings.json.galgo-statusline" < "$TMP/statusline130-input" > "$TMP/statusline130-output"
sleep 1
galgo --json usage --pane status130 > "$TMP/usage130.json"
python3 -I - "$TMP/usage130.json" <<'PY130'
import json,sys
u=json.load(open(sys.argv[1]))['panes'][0]['usage']
assert u['context_tokens']==33 and u['context_window']==1000 and u['cost_usd']==2.75
assert 'SECRETO' not in json.dumps(u)
PY130
check "statusline informa solo métricas al daemon" $?
galgo --json usage --limits > "$TMP/limits130.json"
python3 -I - "$TMP/limits130.json" <<'PY130B'
import json,sys
l=json.load(open(sys.argv[1]))['limits']; assert any(x['agent']=='claude' and any(w['minutes']==300 and w['used_pct']==20 for w in x['windows']) for x in l)
PY130B
check "statusline informa cuota cinco horas" $?
env -u CLAUDE_CONFIG_DIR HOME="$HOME130" galgo hooks uninstall claude --yes >/dev/null
python3 -I - "$HOME130/.claude/settings.json" <<'PY130C'
import json,sys
v=json.load(open(sys.argv[1])); assert v['statusLine']=={'type':'command','command':'cat','padding':3} and v['model']=='conservar'
PY130C
check "uninstall restaura statusline previo y configuración ajena" $?
[ ! -e "$HOME130/.claude/settings.json.galgo-statusline" ]; check "uninstall retira respaldo auxiliar restaurado" $?

echo "141. hooks de Claude, Codex y PI asocian fuentes de uso"
for agent in claude codex pi; do galgo spawn "source131-$agent" -- bash --norc >/dev/null; done
printf '%s\n' '{"type":"assistant","isSidechain":false,"message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":123,"output_tokens":5}}}' > "$TMP/source131-claude.jsonl"
printf '{"hook_event_name":"UserPromptSubmit","session_id":"s1","transcript_path":"%s"}\n' "$TMP/source131-claude.jsonl" | GALGO_PANE=source131-claude galgo hook claude
printf '%s\n' '{"type":"turn_context","payload":{"model":"gpt-5.6-terra"}}' '{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":456,"output_tokens":7},"last_token_usage":{"input_tokens":456,"output_tokens":7},"model_context_window":10000}}}' > "$TMP/source131-codex.jsonl"
printf '{"hook_event_name":"UserPromptSubmit","session_id":"s2","transcript_path":"%s"}\n' "$TMP/source131-codex.jsonl" | GALGO_PANE=source131-codex galgo hook codex
printf '%s\n' '{"type":"message","message":{"role":"assistant","usage":{"input":12,"output":3,"cost":{"total":0.1}}}}' > "$TMP/source131-pi.jsonl"
printf '{"type":"agent_start","sessionId":"s3","sessionFile":"%s","context":{"tokens":42,"contextWindow":1000}}\n' "$TMP/source131-pi.jsonl" | GALGO_PANE=source131-pi galgo hook pi
sleep 3
for agent in claude codex pi; do galgo --json usage --pane "source131-$agent" > "$TMP/usage131-$agent.json"; done
python3 -I - "$TMP" <<'PY131'
import json,os,sys
p=sys.argv[1]
def u(agent): return json.load(open(os.path.join(p,'usage131-'+agent+'.json')))['panes'][0]['usage']
assert u('claude')['input']==123
assert u('codex')['input']==456
assert u('pi')['input']==12 and u('pi')['context_tokens']==42 and u('pi')['context_window']==1000
assert p not in json.dumps([u('claude'),u('codex'),u('pi')])
PY131
check "fuentes llegan al tracker y contexto PI al daemon sin rutas en salida" $?

echo "142. statusline modificado por usuario no se sobrescribe"
HOME132="$TMP/home132"; mkdir -p "$HOME132/.claude"
printf '%s\n' '{"statusLine":{"type":"command","command":"cat"}}' > "$HOME132/.claude/settings.json"
env -u CLAUDE_CONFIG_DIR HOME="$HOME132" galgo hooks install claude --statusline --yes >/dev/null
python3 -I - "$HOME132/.claude/settings.json" <<'PY132'
import json,sys
p=sys.argv[1]; v=json.load(open(p)); v['statusLine']['command']='printf usuario'; open(p,'w').write(json.dumps(v))
PY132
env -u CLAUDE_CONFIG_DIR HOME="$HOME132" galgo hooks uninstall claude --yes >/dev/null
python3 -I - "$HOME132/.claude/settings.json" "$HOME132/.claude/settings.json.galgo-statusline" <<'PY132B'
import json,sys
assert json.load(open(sys.argv[1]))['statusLine']['command']=='printf usuario'
assert json.load(open(sys.argv[2]))['previous']['command']=='cat'
PY132B
check "uninstall conserva edición y respaldo previo" $?
env -u GALGO_PANE galgo statusline claude < "$TMP/statusline130-input" | grep -qx galgo; check "sin comando previo ofrece línea propia" $?
echo "137. uso de Claude con el id real claude-sonnet-5-5: modelo y coste de lista; contexto en tokens, sin porcentaje"
mkdir -p "$TMP/c137" && chmod 700 "$TMP/c137"
export GALGO_SOCKET="$TMP/c137/g.sock"
TR137="$TMP/c137/sonnet.jsonl"
printf '%s\n' '{"type":"assistant","isSidechain":false,"requestId":"r1","message":{"id":"m1","model":"claude-sonnet-5-5","usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0}}}}' > "$TR137"
galgo spawn t137 -- bash --norc >/dev/null
rpc() { python3 -c 'import json,socket,sys,os
s=socket.socket(socket.AF_UNIX); s.connect(os.environ["GALGO_SOCKET"]); s.sendall((sys.argv[1]+"\n").encode()); buf=b""
while not buf.endswith(b"\n"):
    d=s.recv(65536)
    if not d: break
    buf+=d
print(buf.decode().strip())' "$1"; }
rpc "{\"op\":\"hook\",\"pane\":\"t137\",\"event\":\"working\",\"summary\":null,\"source\":{\"agent\":\"claude\",\"session_id\":\"s137\",\"transcript\":\"$TR137\"}}" | grep -q '"ok":true'; check "el hook con el transcript sintético se acepta" $?
sleep 2.5
galgo --json ls | python3 -c 'import json,sys
u=[x for x in json.load(sys.stdin) if x["name"]=="t137"][0]["usage"]
assert u["model"]=="claude-sonnet-5-5", u["model"]
assert u["cost_source"]=="list_price" and abs(u["cost_usd"]-0.0043)<1e-9, (u["cost_source"], u["cost_usd"])
assert u["context_tokens"]==1600 and u["context_pct"] is None and u["context_window"] is None, u'
check "modelo, coste de lista (0,0043 \$) y contexto sin ventana inventada" $?
galgo usage --pane t137 | grep -qF "claude-sonnet-5-5"; check "galgo usage muestra el modelo" $?
galgo usage --pane t137 | grep -qF "1600"; [ $? -ne 0 ]; check "galgo usage muestra el contexto en tokens (1,6k), no en porcentaje" $?
galgo usage --by model | grep -qF "claude-sonnet-5-5"; check "galgo usage --by model agrupa por el id real" $?
galgo stop >/dev/null 2>&1; sleep 0.3
export GALGO_SOCKET="$TMP/galgo.sock"

echo "138. título de la tarea: galgo title llega a ls; vacío lo borra; caracteres de control se rechazan"
mkdir -p "$TMPDIR/c138" 2>/dev/null; mkdir -p "$TMP/c138" && chmod 700 "$TMP/c138"
export GALGO_SOCKET="$TMP/c138/g.sock"
galgo spawn t138 -- bash --norc >/dev/null
galgo title "bug de ventas" --pane t138 >/dev/null; check "galgo title acepta un título" $?
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="t138"][0]
assert p["title"]=="bug de ventas", p.get("title")'
check "el título aparece en ls" $?
galgo title "" --pane t138 >/dev/null
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="t138"][0]
assert p.get("title") is None'
check "un título vacío lo borra" $?
galgo title $'x\e[31m' --pane t138 >/dev/null 2>&1; [ $? -ne 0 ]; check "un título con caracteres de control se rechaza" $?

echo "139. worktree de tarea: spawn --worktree crea un worktree propio en una rama nueva y el panel arranca dentro"
mkdir -p "$TMP/c139" && chmod 700 "$TMP/c139"
export GALGO_SOCKET="$TMP/c139/g.sock"
H139="$TMP/home139"; mkdir -p "$H139"
R139="$TMP/c139/repo"; mkdir -p "$R139" && git -C "$R139" init -q && git -C "$R139" -c user.name=e2e -c user.email=e2e@test commit -q --allow-empty -m base
HOME="$H139" galgo spawn wt139 --cwd "$R139" --worktree tarea-e2e -- bash --norc >/dev/null; check "spawn con --worktree" $?
WT139=$(galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="wt139"][0]
print(p["cwd"])')
# El cwd vivo llega con la ruta real (macOS resuelve /var a /private/var): se compara con la ruta canónica.
RW139=$(cd "$H139" && pwd -P)
case "$WT139" in "$RW139/.galgo/worktrees/"*) true;; *) false;; esac; check "el panel arranca en ~/.galgo/worktrees" $?
[ "$(git -C "$WT139" rev-parse --abbrev-ref HEAD)" = tarea-e2e ]; check "el worktree está en la rama nueva" $?
galgo --json ls | python3 -c 'import json,sys
p=[x for x in json.load(sys.stdin) if x["name"]=="wt139"][0]
assert p["workspace"]=="repo/tarea-e2e", p.get("workspace")'
check "el workspace sale del repo original" $?
HOME="$H139" galgo spawn wt139b --cwd "$R139" --worktree tarea-e2e -- bash --norc >/dev/null 2>&1; [ $? -ne 0 ]; check "una rama que ya existe se rechaza" $?
HOME="$H139" galgo spawn wt139c --cwd "$TMP" --worktree otra -- bash --norc >/dev/null 2>&1; [ $? -ne 0 ]; check "fuera de un repo git no hay worktree" $?

echo "143. worktree de tarea: el resultado lleva los cambios; fusionar exige --yes y descartar borra sin fusionar"
export GALGO_SOCKET="$TMP/c139/g.sock"
galgo spawn mgr140 -- bash --norc >/dev/null
HOME="$H139" galgo spawn wt140 --cwd "$R139" --manager mgr140 --worktree tarea-140 -- bash --norc >/dev/null; check "worker con manager y worktree" $?
WT140=$(galgo --json ls | python3 -c 'import json,sys
print([x for x in json.load(sys.stdin) if x["name"]=="wt140"][0]["cwd"])')
galgo send wt140 "echo hola > nuevo140.txt && git add -A && git commit -qm trabajo140" >/dev/null
for i in $(seq 1 50); do [ -f "$WT140/nuevo140.txt" ] && git -C "$WT140" log -1 --format=%s 2>/dev/null | grep -q trabajo140 && break; sleep 0.2; done
[ "$(git -C "$WT140" log -1 --format=%s)" = trabajo140 ]; check "el worker hace su commit en el worktree" $?
galgo send wt140 "" >/dev/null 2>&1
GALGO_PANE=wt140 galgo done --summary "listo" >/dev/null; check "el worker reporta Done" $?
galgo inbox --pane mgr140 | grep -q "nuevo140.txt"; check "el resultado llega al manager con los cambios" $?
galgo worktree diff wt140 | grep -q "nuevo140.txt"; check "galgo worktree diff muestra el archivo" $?
galgo worktree accept wt140 >/dev/null 2>&1; [ $? -ne 0 ] && [ ! -f "$R139/nuevo140.txt" ]; check "fusionar sin --yes no toca el repo" $?
galgo worktree accept wt140 --yes >/dev/null; check "fusionar con --yes" $?
[ -f "$R139/nuevo140.txt" ] && [ ! -e "$WT140" ]; check "el archivo llega al repo principal y el worktree se borra" $?
HOME="$H139" galgo spawn wt140b --cwd "$R139" --worktree tarea-140b -- bash --norc >/dev/null; check "segundo worker con worktree" $?
WT140B=$(galgo --json ls | python3 -c 'import json,sys
print([x for x in json.load(sys.stdin) if x["name"]=="wt140b"][0]["cwd"])')
galgo worktree discard wt140b >/dev/null 2>&1; [ -d "$WT140B" ]; check "descartar sin --yes no borra" $?
galgo worktree discard wt140b --yes >/dev/null; check "descartar con --yes" $?
[ ! -e "$WT140B" ] && [ -z "$(git -C "$R139" branch --list tarea-140b)" ]; check "el worktree y la rama descartados desaparecen" $?
galgo stop >/dev/null 2>&1; sleep 0.3
export GALGO_SOCKET="$TMP/galgo.sock"

echo "144. suscripción empujada: un cambio de pantalla llega sin sondeo; el cierre del panel avisa"
export GALGO_SOCKET="$TMP/c144/g.sock"; mkdir -p "$TMP/c144" && chmod 700 "$TMP/c144"
galgo spawn w144 -- /bin/cat >/dev/null; check "panel para la suscripción" $?
python3 - "$GALGO_SOCKET" <<'PYEOF'
import json, socket, subprocess, sys, time
ruta = sys.argv[1]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(3)
s.connect(ruta)
s.sendall((json.dumps({"op": "watch", "pane": "w144"}) + "\n").encode())
f = s.makefile("r")
def leer():
    return json.loads(f.readline())
assert leer()["event"] == "watching"
assert leer()["event"] == "changed", "el estado actual se avisa al abrir"
# Un panel recién creado puede emitir un aviso más al arrancar: después del envío se espera el primer aviso
# posterior, saltando los que lleguen antes del texto.
subprocess.run(["galgo", "send", "w144", "hola-suscripcion"], check=True, capture_output=True)
t0 = time.time()
ev = leer()
while ev["event"] != "changed" and time.time() - t0 < 3:
    ev = leer()
assert ev["event"] == "changed" and time.time() - t0 < 2, ev
subprocess.run(["galgo", "kill", "w144"], check=True, capture_output=True)
t0 = time.time()
ev = leer()
while ev["event"] == "changed" and time.time() - t0 < 3:
    ev = leer()
assert ev["event"] == "closed", ev
PYEOF
check "un cambio llega como aviso y el cierre también" $?
galgo stop >/dev/null 2>&1
export GALGO_SOCKET="$TMP/galgo.sock"
echo "145. modo orquestador (daemon): una tarea solo se entrega a un agente con hooks y libre; el resultado vuelve al remitente"
export GALGO_SOCKET="$TMP/c145/g.sock"; mkdir -p "$TMP/c145" && chmod 700 "$TMP/c145"
galgo spawn jefe145 -- /bin/cat >/dev/null; galgo spawn codex145 -- /bin/cat >/dev/null; galgo spawn sinhooks145 -- /bin/cat >/dev/null
check "paneles de prueba" $?
python3 - "$GALGO_SOCKET" <<'PYEOF'
import json, socket, subprocess, sys, time
ruta = sys.argv[1]
def pedir(req):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(ruta)
    s.sendall((json.dumps(req) + "\n").encode())
    s.shutdown(socket.SHUT_WR)
    return json.loads(s.makefile("r").readline())
def pantalla(p):
    return subprocess.run(["galgo", "read", p], capture_output=True, text=True).stdout
# Sin hooks: la tarea queda pendiente y no llega al panel.
r = pedir({"op": "delegate", "from": "jefe145", "to": "sinhooks145", "task": "esto no debe llegar"})
assert r["ok"] and r["data"]["status"] == "pendiente", r
time.sleep(0.4)
assert "tarea #" not in pantalla("sinhooks145"), "sin hooks no se entrega"
# Con hook de idle: el destinatario está libre y la tarea se entrega, pegada y con Enter.
assert pedir({"op": "hook", "pane": "codex145", "event": "idle", "summary": None})["ok"]
r = pedir({"op": "delegate", "from": "jefe145", "to": "codex145", "task": "crea x.txt", "context": "en la raíz"})
assert r["ok"] and r["data"]["status"] == "entregada", r
tid = r["data"]["id"]
time.sleep(0.6)
assert "tarea #%d" % tid in pantalla("codex145"), pantalla("codex145")
# Solo el destinatario cierra la tarea; el resultado llega al remitente.
assert not pedir({"op": "task_done", "from": "jefe145", "id": tid, "summary": "no", "ok": True})["ok"]
assert pedir({"op": "task_done", "from": "codex145", "id": tid, "summary": "x.txt creado", "ok": True})["ok"]
inbox = subprocess.run(["galgo", "inbox", "--pane", "jefe145"], capture_output=True, text=True).stdout
assert "x.txt creado" in inbox, inbox
tareas = pedir({"op": "tasks", "pane": "jefe145"})["data"]
assert any(t["id"] == tid and t["status"] == "lista" for t in tareas), tareas
PYEOF
check "la tarea se entrega solo con hooks y el resultado vuelve al remitente" $?
echo "146. modo orquestador (CLI): galgo delegate y galgo task done desde el panel propio (GALGO_PANE)"
export GALGO_SOCKET="$TMP/c146/g.sock"; mkdir -p "$TMP/c146" && chmod 700 "$TMP/c146"
galgo spawn jefe146 -- /bin/cat >/dev/null; galgo spawn codex146 -- /bin/cat >/dev/null
check "paneles de prueba" $?
python3 -c 'import json,socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.connect(sys.argv[1])
s.sendall((json.dumps({"op":"hook","pane":"codex146","event":"idle","summary":None})+"\n").encode()); s.shutdown(socket.SHUT_WR); s.makefile("r").readline()' "$GALGO_SOCKET"
out=$(GALGO_PANE=jefe146 galgo delegate codex146 crea x.txt --context "en la raíz"); echo "$out" | grep -q "tarea #.*entregada"; check "galgo delegate entrega a un agente libre" $?
TID=$(echo "$out" | sed -n 's/^tarea #\([0-9]*\).*/\1/p')
sleep 0.6; galgo read codex146 | grep -q "tarea #$TID"; check "el destinatario recibe el texto" $?
GALGO_PANE=jefe146 galgo task done "$TID" "soy jefe" >/dev/null 2>&1; [ $? -ne 0 ]; check "el remitente no cierra la tarea" $?
GALGO_PANE=codex146 galgo task done "$TID" x.txt creado >/dev/null; check "el destinatario cierra la tarea" $?
galgo inbox --pane jefe146 | grep -q "x.txt creado"; check "el resultado llega al remitente" $?
galgo tasks --pane jefe146 | grep -q "lista"; check "galgo tasks muestra la tarea cerrada" $?
env -u GALGO_PANE galgo delegate codex146 algo >/dev/null 2>&1; [ $? -ne 0 ]; check "sin GALGO_PANE no se delega" $?
echo "147. modo orquestador (lanzamiento): claude y codex arrancan con el MCP de galgo y su rol; una tarea delegada se responde de punta a punta (agentes falsos con hooks)"
export GALGO_SOCKET="$TMP/c147/g.sock"; mkdir -p "$TMP/c147" "$TMP/fakebin" && chmod 700 "$TMP/c147"
cat > "$TMP/fakebin/claude" <<'FAKE_EOF'
#!/bin/sh
# Agente falso de prueba: registra sus argumentos, avisa que está libre por hook y responde las tareas que le llegan.
printf '%s\n' "$@" > "$TMP_ARGS_DIR/$GALGO_PANE_NAME.args"
echo '{"hook_event_name":"Notification","notification_type":"idle_prompt"}' | galgo hook claude
while IFS= read -r linea; do
  case "$linea" in
    *"tarea #"*)
      id=$(echo "$linea" | sed -n 's/.*tarea #\([0-9][0-9]*\).*/\1/p')
      galgo task done "$id" "hecho por $GALGO_PANE_NAME" >/dev/null 2>&1
      echo '{"hook_event_name":"Notification","notification_type":"idle_prompt"}' | galgo hook claude ;;
  esac
done
FAKE_EOF
cat > "$TMP/fakebin/codex" <<'FAKE_EOF'
#!/bin/sh
printf '%s\n' "$@" > "$TMP_ARGS_DIR/$GALGO_PANE_NAME.args"
sleep 30
FAKE_EOF
chmod +x "$TMP/fakebin/claude" "$TMP/fakebin/codex"
export TMP_ARGS_DIR="$TMP/c147"; ANTES_PATH="$PATH"; export PATH="$TMP/fakebin:$PATH"
galgo spawn jefe147 --orquestador --rol manager -- claude >/dev/null; galgo spawn w147 --orquestador --rol worker -- claude >/dev/null; galgo spawn cx147 --orquestador --rol reviewer -- codex >/dev/null
check "lanzados con --orquestador" $?
for i in $(seq 1 50); do [ -f "$TMP/c147/w147.args" ] && [ -f "$TMP/c147/cx147.args" ] && break; sleep 0.2; done
grep -q -- "--mcp-config" "$TMP/c147/jefe147.args" && grep -q 'GALGO_PANE' "$TMP/c147/jefe147.args" && grep -q 'jefe147' "$TMP/c147/jefe147.args" && ! grep -q -- "--strict-mcp-config" "$TMP/c147/jefe147.args"; check "claude recibe el MCP de galgo en línea y conserva sus MCP (sin --strict-mcp-config)" $?
grep -q "Coordinas" "$TMP/c147/jefe147.args"; check "claude recibe las instrucciones de su rol" $?
grep -q "mcp_servers.galgo.command=" "$TMP/c147/cx147.args" && grep -q "developer_instructions=" "$TMP/c147/cx147.args"; check "codex recibe overrides -c para el MCP y las instrucciones" $?
GALGO_PANE=jefe147 galgo delegate w147 crea x.txt --context "en la raíz" >/dev/null; check "el manager delega (aunque el worker aún no avisó)" $?
ok=0; for i in $(seq 1 60); do galgo inbox --pane jefe147 2>/dev/null | grep -q "hecho por w147" && { ok=1; break; }; sleep 0.25; done
[ $ok = 1 ]; check "el worker recibe la tarea, responde y el resultado vuelve al manager" $?
galgo tasks --pane jefe147 | grep -q "lista"; check "la tarea queda lista en el registro" $?
galgo read w147 | grep -q "tarea #"; check "el texto llegó al worker pegado en su terminal" $?
export PATH="$ANTES_PATH"; unset TMP_ARGS_DIR
echo "resultado: $pass ok, $fail con falla, $skipped omitidos"
[ "$fail" = "0" ]
