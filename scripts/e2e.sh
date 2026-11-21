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

check() { # descripción, código de salida
  if [ "$2" = "0" ]; then echo "  ok    $1"; pass=$((pass + 1)); else echo "  FALLA $1"; fail=$((fail + 1)); fi
}
cleanup() { galgo stop >/dev/null 2>&1; rm -rf "$TMP"; }
trap cleanup EXIT

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
sleep 0.8
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

echo "resultado: $pass ok, $fail con falla"
[ "$fail" = "0" ]
