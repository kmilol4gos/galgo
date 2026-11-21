# Uso de los agentes en galgo: tokens, coste, contexto y cuotas

Investigación del 21-11-2026 en esta máquina. Fue solo lectura: no se editó código ni configuración del
usuario.

Versiones observadas:

| Agente | Versión |
| --- | --- |
| Claude Code | 2.1.329 (statusline) / 2.1.330 (transcript) |
| codex-cli | 0.169.1 (sesión activa) |
| PI | 1.5.2 |
| opencode | 1.18.33 |

Todo lo que dice **observado** se vio en archivos reales de esta máquina. Para no mostrar contenido
privado, solo se extrajeron claves, tipos y valores numéricos o enumerados: nunca texto de conversaciones,
rutas de sesión completas, cuentas ni correos. Los ejemplos de abajo son esqueletos con números reales y
las cadenas sustituidas.

## Resumen

| Agente | Tokens | Coste | % de contexto | Cuotas 5 h / semana | Mejor fuente | Cómo se asocia al panel |
| --- | --- | --- | --- | --- | --- | --- |
| Claude Code | sí, por mensaje (transcript) | estimado: statusline `cost.total_cost_usd`, o tabla de precios | sí: statusline `context_window.used_percentage`; del transcript solo los tokens, no el tamaño de ventana | **sí, solo en el statusline** (`rate_limits.five_hour`, `seven_day`) | statusline y transcript | hooks: `session_id` y `transcript_path` con `GALGO_PANE` |
| Codex | sí (`token_count`: por turno y acumulado) | no lo da: tabla de precios | sí: `model_context_window` + `last_token_usage` | **sí** (`rate_limits.primary` 300 min y `secondary` 10080 min, `plan_type`) | rollout `~/.codex/sessions/**.jsonl` | hooks: `session_id` y `transcript_path` (vistos en el binario) |
| PI | sí, por mensaje | **sí**, por mensaje (`usage.cost`, calculado por PI con su catálogo) | sí: `ctx.getContextUsage()` → `{tokens, contextWindow, percent}` | no se encontraron | extensión galgo, o el jsonl de la sesión | extensión: `ctx.sessionManager.getSessionFile()` |
| opencode | sí, por mensaje y agregado por sesión (SQLite) | sí, campo `cost` (0 en esta máquina) | tokens del último mensaje + `limit.context` de `models.json` | no se encontraron | `~/.local/share/opencode/opencode.db` | **pendiente**: no hay hook todavía; como apaño, el directorio y la hora |

---

## 1. Claude Code

### 1.1 Transcript: `~/.claude/projects/<cwd con / → ->/<session_id>.jsonl`

- **Subagentes.** Van en `…/<session_id>/subagents/agent-<id>.jsonl`; en esta máquina hay 45, y su consumo
  cuenta aparte.
- **Tamaño observado.** Hay 115 jsonl con 1,7 GB en total y 85 de ellos pasan de 1 MB. Los archivos de
  sesión de primer nivel pesan 23 MiB de media y el mayor llega a **586 MB**.
- **Formato.** Hay una línea JSON por evento. Las de `type: "assistant"` llevan `message.usage`. El
  esqueleto observado, con los números de este transcript:

```json
{"type":"assistant","sessionId":"<uuid>","requestId":"<str>","timestamp":"<iso>","cwd":"<str>",
 "isSidechain":false,"version":"2.1.330",
 "message":{"id":"<str>","model":"claude-opus-5-5","stop_reason":"tool_use",
  "usage":{"input_tokens":2,"output_tokens":179,
   "cache_read_input_tokens":28971,"cache_creation_input_tokens":31395,
   "cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":31395},
   "output_tokens_details":{"thinking_tokens":18},
   "server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},
   "service_tier":"standard","speed":"standard","inference_geo":"global","iterations":[…]}}}
```

- **Duplicados (observado).** Claude Code escribe una línea por bloque de contenido y **repite el mismo
  `usage`** en cada una. En este transcript hubo 448 líneas `assistant` para solo 228 valores distintos de
  `message.id`, y las 159 repetidas tenían un `usage` idéntico. **Hay que deduplicar por `message.id`**
  (o por `requestId`): si no, se cuenta dos o tres veces.
- **Sin coste ni ventana.** El transcript no trae ningún campo de coste ni de tamaño de ventana. El nombre
  del modelo (`claude-opus-5-5`) no dice si la ventana es de 200k o de 1M.
- **Contexto usado.** Se calcula como `input_tokens + cache_read_input_tokens +
  cache_creation_input_tokens` del **último** mensaje del hilo principal (`isSidechain: false`). La doc
  del statusline usa exactamente esa definición y excluye la salida. En este transcript, el último valor es
  244.729 tokens.
- **Coste de leer.** Con un filtro previo por bytes (`"usage"` y `"assistant"` en la línea) y JSON solo de
  las líneas que pasan, recorrer entero el archivo de 586 MB tomó **0,53 s** (9.118 mensajes únicos). Con
  lectura incremental desde el último offset, el coste es casi cero.

### 1.2 Statusline: lo único que trae cuotas del plan

Hay una salida real en esta máquina, porque el statusline configurado por el usuario (de otra herramienta,
harp) vuelca su stdin en `~/.harp/claude-statusline.json`. El esqueleto observado, con las cadenas
sustituidas:

```json
{"session_id":"<uuid>","transcript_path":"<str>","cwd":"<str>","model":{"id":"claude-haiku-5-5","display_name":"Haiku 5.5"},
 "version":"2.1.329",
 "cost":{"total_cost_usd":7.07,"total_duration_ms":89224991,"total_api_duration_ms":1431236,
         "total_lines_added":281,"total_lines_removed":1},
 "context_window":{"total_input_tokens":598493,"total_output_tokens":7,"context_window_size":1000000,
   "current_usage":{"input_tokens":2,"output_tokens":7,"cache_creation_input_tokens":1662,"cache_read_input_tokens":596829},
   "used_percentage":60,"remaining_percentage":40},
 "exceeds_200k_tokens":true,
 "prompt_cache":{"hit_ratio":0.986,"requests":301,"misses":2,…},
 "rate_limits":{"five_hour":{"used_percentage":15,"resets_at":1795296000},
                "seven_day":{"used_percentage":14,"resets_at":1795798800}}}
```

**Documentación oficial** (https://code.claude.com/docs/en/statusline):
- `rate_limits.five_hour` y `seven_day` llevan `used_percentage` (0-100) y `resets_at` en segundos Unix.
  Solo aparecen para suscriptores Pro, Max o Ultra (o detrás de un gateway de Claude apps), y solo después
  de la primera respuesta. También hay `spend_limit.*`.
- `context_window.used_percentage` no incluye los tokens de salida. Puede ser `null` al principio de la
  sesión.
- `cost.total_cost_usd` es «Estimated session cost in USD, computed client-side at list price … May differ
  from your actual bill». Vuelve a 0 con `/clear`.
- El statusline se invoca al arrancar la sesión y luego en cada mensaje del asistente, `/compact`, cambio
  de modo, `refreshInterval` (opcional, mínimo 1 s) o vencimiento de `resets_at`. Tiene un debounce de
  300 ms y no consume tokens.

**Hooks** (https://code.claude.com/docs/en/hooks): todos reciben `session_id`, `transcript_path`, `cwd`,
`permission_mode`, `effort` y `hook_event_name`. `Stop` añade `last_assistant_message`,
`stop_hook_active`, `background_tasks` y `session_crons`. **Ningún hook trae tokens, coste ni
contexto**: sirven para *encontrar* el transcript, no para medir.

**Límites del plan fuera del statusline.** `/usage` los muestra en pantalla, y
https://code.claude.com/docs/en/costs no documenta ninguna otra vía para un script. Conclusión: **las
cuotas de Claude solo se obtienen con un statusline**.

**Conflicto.** Solo existe **un** `statusLine` en `settings.json`, y el usuario ya tiene uno (el de harp).
galgo no puede instalar el suyo sin encadenarlo con el existente (ver la propuesta, §6.3).

### 1.3 Precios de Claude, por si no hay statusline

Fuente: https://platform.claude.com/docs/en/about-claude/pricing (leída hoy). En USD por millón de tokens
(MTok):

| Modelo | Entrada | Caché 5 min (escritura) | Caché 1 h (escritura) | Lectura de caché | Salida |
| --- | --- | --- | --- | --- | --- |
| Opus 5.5 | 4 | 5 | 8 | 0,20 | 20 |
| Opus 5 | 5 | 6,25 | 10 | 0,50 | 25 |
| Sonnet 5.5 | 2 | 2,50 | 4 | 0,10 | 10 |
| Sonnet 5 | 2 | 2,50 | 4 | 0,20 | 10 |
| Haiku 5.5 (≤100k) | 0,10 | 0,125 | 0,20 | 0,01 | 0,50 |
| Haiku 5.5 (>100k) | 0,50 | 0,625 | 1 | 0,05 | 2,50 |
| Haiku 4.5 | 1 | 1,25 | 2 | 0,10 | 5 |
| Fable 5.2 | 8 | 10 | 16 | 0,20 | 40 |

El transcript separa `ephemeral_5m` y `ephemeral_1h`, así que se puede aplicar la tarifa correcta.

**No verificado:**
- el recargo de `speed: fast` y el de `inference_geo` distinto de `global`;
- cómo se cobran `server_tool_use` (búsquedas web);
- si «Haiku 5.5 >100k» se aplica por petición o por contexto.

Con suscripción, este coste es un **equivalente de lista**, no la factura.

---

## 2. Codex

### 2.1 Rollout: `~/.codex/sessions/AAAA/MM/DD/rollout-<fecha>-<thread_id>.jsonl`

- **Archivos cerrados.** Se comprimen a `.jsonl.zst`. En esta máquina hay 148 `.zst` y un solo `.jsonl`
  activo, 367 MB en total; el `.zst` mayor pesa 59 MB. También hay `archived_sessions/` (2 archivos).
- **`zstd`.** Está en `/opt/homebrew/bin/zstd`; descomprimir y filtrar el mayor tomó 1,3 s.
- **Eventos útiles (observado en la sesión activa de 2 MB).** Hubo 60 eventos `event_msg/token_count` y
  60 `token_usage_record`. El esqueleto, con números reales:

```json
{"type":"event_msg","timestamp":"<iso>","payload":{"type":"token_count",
 "info":{"total_token_usage":{"input_tokens":26395,"cached_input_tokens":7936,"cache_write_input_tokens":0,
                              "output_tokens":345,"reasoning_output_tokens":87,"total_tokens":26740},
         "last_token_usage":{…mismos campos…},"model_context_window":258400,"last_usage_includes_reasoning":true},
 "rate_limits":{"limit_id":"codex","plan_type":"plus",
   "primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1794969283},
   "secondary":{"used_percent":2.0,"window_minutes":10080,"resets_at":1795556083},
   "credits":{"has_credits":false,"unlimited":false,"balance":"<str>"},
   "rate_limit_reached_type":null,"spend_control_reached":null}}}
```

- **Otras líneas con usage.** `token_usage_record` trae `usage`, `turn_token_usage` y
  `thread_token_usage` con `thread_id`, `turn_id` y `session_id`.
- **Modelo.** Viene en `turn_context.payload.model` (observado `gpt-5.6-terra`).
- **Cabecera.** `session_meta` trae `id`, `session_id`, `cwd`, `cli_version`, `originator` y `source`.
  También `creator_account_id` y `creator_user_id`, que **no deben leerse**.
- **Índice.** `~/.codex/state_5.sqlite`, tabla `threads`, tiene `rollout_path`, `cwd`, `model`,
  `tokens_used` y `updated_at`. Sirve para localizar rollouts sin recorrer los directorios (el esquema se
  leyó en modo solo lectura).
- **Contexto usado.** Se calcula como `last_token_usage.input_tokens` (que en OpenAI *incluye* los tokens
  en caché) más `output_tokens`, dividido por `model_context_window`. En el último evento eran
  199.282 / 258.400, un 77 %.
  - **No verificado:** que la TUI de Codex use exactamente esta fórmula; se cree que resta una base fija.
    Hay que presentarlo como aproximado.
- **Cuotas.** `primary` es la ventana de 5 h (300 min) y `secondary` la de 7 días (10080 min). Son **de la
  cuenta, no del panel**: hay que mostrar el último valor observado con su hora, porque envejece.
- **Coste.** Codex no lo da, y con plan ChatGPT (`plan_type: plus`) no se factura por token. El
  equivalente de lista sale de https://developers.openai.com/api/docs/pricing (leída hoy), tabla
  «Standard» hasta 272K, en USD por MTok:

  | Modelo | Entrada | Entrada en caché | Salida |
  | --- | --- | --- | --- |
  | gpt-5.6-sol | 4 | 0,40 | 20 |
  | gpt-5.6-terra | 2 | 0,20 | 12 |
  | gpt-5.6-luna | 0,20 | 0,02 | 1,20 |
  | gpt-5.5 | 5 | 0,50 | 30 |
  | gpt-5.4 | 2,50 | 0,25 | 15 |
  | gpt-6.2-sol | 2 | 0,10 | 10 |
  | gpt-6.2-astra | 8 | 0,40 | 40 |

  - El coste es `(input − cached)·entrada + cached·caché + output·salida`. Los tokens de razonamiento ya
    están dentro de `output_tokens`; según `last_usage_includes_reasoning: true` no se suman aparte.
  - **No verificado:** los modelos ausentes de la tabla, por ejemplo `gpt-6.1-terra` y `gpt-6-luna`, que
    aparecen en PI y opencode, y los precios por encima de 272K. Si no hay tarifa, mostrar «sin tarifa»,
    nunca inventarla.

**Asociar el panel.** Los hooks de Codex reciben `session_id` y `transcript_path`: aparecen en las cadenas
del binario 0.167 junto a `hook_event_name`, `cwd` y `model`. `galgo hook codex` ya recibe ese JSON con
`GALGO_PANE`.
- **No verificado** en una sesión real: que `transcript_path` apunte al rollout `.jsonl` vivo.

---

## 3. PI

### 3.1 Sesión: `~/.pi/agent/sessions/--<cwd con / → ->--/<fecha ISO>_<id>.jsonl`

- **Tamaño observado.** 36 archivos, 104 MB, 2,9 MiB de media.
- **Cabecera.** `{"type":"session","version":3,"id":"<str>","timestamp":"<iso>","cwd":"<str>"}`.
- **Mensajes del asistente.** `message.role: "assistant"`, con `provider`, `model`, `api`, `stopReason`,
  `durationMs` y:

```json
"usage":{"input":3467,"output":188,"reasoning":16,"cacheRead":0,"cacheWrite":0,"totalTokens":3655,
         "cost":{"input":0.003467,"output":0.00094,"cacheRead":0,"cacheWrite":0,"total":0.004407,
                 "totalFromProvider":false}}
```

- **Origen del coste.** `totalFromProvider: false` significa que el coste lo **calcula PI** con su
  catálogo (`~/.pi/agent/models-store.json`, que por modelo trae `cost:{input,output,cacheRead,cacheWrite}`
  por MTok, `contextWindow` y `maxTokens`). Con suscripción vuelve a ser un equivalente de lista.
- **Contexto.** El mejor dato es `ctx.getContextUsage()` dentro de la extensión, que devuelve
  `{tokens: number|null, contextWindow, percent: number|null}`. Según
  `docs/extensions.md`, usa el último `usage` del asistente y estima los mensajes posteriores. Es `null`
  justo después de compactar. Desde el archivo, la alternativa es `input + cacheRead + cacheWrite` del
  último mensaje del asistente frente al `contextWindow` del catálogo.
- **Cuotas.** No se encontró ningún campo de límites en los jsonl ni en el catálogo.
- **Asociar el panel.** La extensión galgo ya corre con `GALGO_PANE`. En `agent_settled` puede enviar
  `ctx.sessionManager.getSessionFile()` (documentado en `extensions.md`) y los números de
  `getContextUsage()`, sin leer ningún archivo.

---

## 4. opencode

### 4.1 Base de datos: `~/.local/share/opencode/opencode.db` (SQLite en modo WAL)

- **Tamaño.** **6,1 GB** en esta máquina, más un WAL de 3 MB.
- **Tabla `session`** (y `session_v2`). Trae agregados por sesión: `cost`, `tokens_input`,
  `tokens_output`, `tokens_reasoning`, `tokens_cache_read`, `tokens_cache_write`, `model` (JSON
  `{"id","providerID"}`), `directory`, `time_created` y `time_updated` (ms). Hay índices por
  `project_id`, `parent_id` y `workspace_id`.
  - **Observado:** 1.170 sesiones con un coste total de 10,47 USD. Las recientes tienen `cost` 0 con
    `gpt-6-luna` (proveedor openai), con tokens de entrada que pasan del millón y lecturas de caché de
    cientos de millones por sesión, porque son agregados de muchas llamadas.
- **Tabla `message`.** Tiene `data` en JSON; hay 43.224 mensajes del asistente. Las claves de un mensaje
  del asistente son `parentID`, `role`, `mode`, `agent`, `path`, `cost`, `tokens`, `modelID`, `providerID`
  y `time`. Un ejemplo de `tokens`:
  `{"total":268758,"input":1497,"output":110,"reasoning":15,"cache":{"read":267136,"write":0}}`.
  Hay un índice `message_session_time_created_id_idx` por `(session_id, time_created, id)`.
- **Contexto usado.** Es `input + cache.read + cache.write` del último mensaje del asistente de la sesión
  raíz (268.633 en el ejemplo). La ventana sale de `~/.cache/opencode/models.json`, en
  `<proveedor>.models.<id>.limit.context` (observado `limit:{context:400000,input:272000,output:128000}`).
- **Coste.** El campo `cost` viene de opencode, que toma precios de models.dev (`models.json` trae
  `cost:{input,output,cache_read,cache_write}`). En esta máquina es 0, probablemente por la suscripción de
  OpenAI; **la causa no está verificada**.
- **Cuotas.** No se encontraron.
- **Coste de leer.** Hay que ir **siempre por `session_id`** usando el índice, y extraer solo
  `json_extract(data,'$.tokens')`, `$.cost`, `$.modelID` y `$.time`, nunca `data` completo. Con el WAL
  vivo, abrir en `mode=ro` normal: `immutable=1` (el que usé aquí, para no tocar nada) puede leer datos
  viejos o inconsistentes mientras opencode escribe.
- **Asociar el panel.** Todavía no hay `galgo hook opencode` (ver `docs/hooks-agents.md`). Con un plugin,
  el `sessionID` de `session.created` y `session.status` daría el vínculo. Mientras tanto, el apaño es
  `session.directory == cwd del panel` con `time_created ≥ spawn del panel`, ignorando las sesiones con
  `parent_id`. **Es ambiguo** si hay dos paneles en el mismo directorio: en ese caso, no mostrar nada.
- **No ejecutado:** `opencode stats`. Podría migrar o escribir la base de datos, y la investigación era de
  solo lectura.

---

## 5. Definiciones comunes

- **Tokens.** Se normalizan a `input` (sin caché), `cache_read`, `cache_write`, `output` y `reasoning`
  (informativo). Claude y PI ya separan la caché. En Codex, `input` *incluye* `cached`, así que se guarda
  `input − cached`. En opencode, `input` ya excluye `cache.read`, como en el ejemplo: 1.497 + 267.136.
- **Contexto usado.** Son los tokens de entrada (con caché) de la **última** respuesta del hilo principal,
  sin salida, divididos por la ventana del modelo. Si la ventana no se conoce con certeza (Claude sin
  statusline, modelos sin catálogo), se muestran los tokens y el porcentaje queda vacío. Nunca se supone
  200k.
- **Coste.** Siempre con su origen (`cost_source`):
  - `agent`: PI u opencode lo dan, calculado por el propio agente;
  - `statusline`: el estimado de Claude Code;
  - `list_price`: calculado por galgo con una tabla de precios versionada en el repo, con fecha y URL de
    origen;
  - `unknown`.

  Con suscripción se etiqueta como «equivalente de lista».
- **Cuotas.** Son de **cuenta y agente**, no de panel. Se guardan el último valor visto y su hora
  (`observed_at`), y se marcan como «viejo» si `resets_at` ya pasó o si tienen más de 15 minutos.
- **Workspace.** galgo no tiene ese concepto. Propongo dos agrupaciones: **árbol** (un panel sin manager
  y todos sus descendientes) y **directorio** (la raíz git del cwd del panel).

---

## 6. Propuesta

### 6.1 Dónde se calcula

**Crate nuevo `galgo-usage`**, sin red, con:
- un parser por agente, que solo deserializa structs con los campos numéricos (serde descarta el resto) y
  aplica antes el filtro por bytes;
- la lectura incremental por archivo: `(dev, inode, size, offset)`, solo líneas completas, y volver a
  empezar si el tamaño baja;
- la tabla `prices.toml` versionada, con `source_url` y `checked` por fila;
- la agregación.

**El daemon** guarda en memoria el **uso vivo por panel**:
- Los hooks y la extensión le pasan la *fuente* (agente, `session_id`, ruta) y los números ya calculados
  cuando el agente los da (statusline, `getContextUsage()`).
- Un hilo propio, fuera de tokio, relee incrementalmente la fuente de cada panel con cambios cada 2 s, o
  justo después de un hook.
- No guarda nada nuevo en `state.json`: el uso se reconstruye releyendo los archivos.

**La CLI** hace los informes **históricos** (por día o por agente) leyendo los archivos directamente, sin
daemon, con una caché `~/.galgo/usage-cache.json` (0600) que solo guarda offsets y totales por
archivo y día, sin texto.

### 6.2 Protocolo

- **`Request::Hook`.** Gana un campo opcional `source:
  {agent, session_id?, transcript?}`. `galgo hook claude` y `galgo hook codex` lo rellenan con
  `session_id` y `transcript_path` de su stdin. La extensión de PI añade `sessionFile` y
  `context:{tokens, window, percent}` a sus avisos.
- **`Request::UsageReport { pane, agent, model?, context_tokens?, context_window?, cost_usd?, limits? }`.**
  Lo usa el statusline de Claude (§6.3) y lo puede usar PI.
- **`Request::Usage { group_by, since? }`.** El daemon responde con el uso vivo agregado.

**`PaneInfo` gana `usage: Option<PaneUsage>`:**

```rust
pub struct PaneUsage {
    pub agent: String,                 // "claude" | "codex" | "pi" | "opencode"
    pub model: Option<String>,
    pub input: u64, pub cache_read: u64, pub cache_write: u64, pub output: u64, pub reasoning: u64,
    pub cost_usd: Option<f64>,
    pub cost_source: CostSource,       // Agent | Statusline | ListPrice | Unknown
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub context_pct: Option<f32>,
    pub updated_secs: u64,             // edad del dato
}
```

**Por agente** (no por panel): `Limits { agent, plan?, windows: [{minutes, used_pct, resets_at}],
observed_at }`, que se obtiene con `Request::Usage`.

### 6.3 Statusline de Claude, opcional y encadenado

`galgo hooks install claude --statusline`:
1. guarda el `statusLine.command` existente en una entrada propia de galgo;
2. pone `galgo statusline claude`.

Ese comando:
1. lee stdin;
2. si hay `GALGO_PANE`, envía **solo números** (`cost.total_cost_usd`, `context_window.*`,
   `rate_limits.*`, `model.id`) con un tiempo límite de 200 ms, sin autoarrancar el daemon;
3. ejecuta el comando anterior con el mismo stdin y devuelve su stdout tal cual. Si no había comando
   anterior, imprime una línea corta propia.

`uninstall` restaura el comando anterior. Sin esta opción, Claude no tiene cuotas en galgo; los tokens y
el contexto en tokens siguen saliendo del transcript.

### 6.4 `galgo usage`

```
galgo usage                       # paneles vivos: agente, modelo, ctx %, tokens, coste, edad
galgo usage --by tree|dir|agent|model
galgo usage --pane worker-a
galgo usage --days 7 --by day     # histórico desde los archivos (CLI, sin daemon)
galgo usage --limits              # cuotas por agente con hora de observación y reinicio
galgo usage --json
```

Ejemplo de salida:

```
PANEL      AGENTE  MODELO          CTX        TOKENS (in/cache/out)   COSTE
worker-a   claude  opus-5.5        60% 598k   0.6M/1.4M/12k           7.07 $ (estimado)
worker-b   codex   gpt-5.6-terra   77% 199k   7.6M total              ~3.10 $ (lista)
---
LÍMITES  claude 5h 15% (reinicia 17:00) · 7d 14%    codex 5h 12% · 7d 2% (plus, hace 3 min)
```

### 6.5 GUI

- **Barra lateral.** En cada panel, una línea con «ctx 60 % · 7,07 $», en amarillo desde el 80 % de
  contexto y en rojo desde el 95 %. Sin dato, no se muestra nada (nunca 0 %).
- **Cabecera del panel seleccionado.** Modelo, contexto en tokens y ventana, y tokens y coste de la
  sesión con su origen («estimado» o «lista»).
- **Pie de la barra lateral.** Las cuotas por agente, con barras de 5 h y 7 días, la hora de reinicio y la
  edad del dato.
- **Grupos.** Total de tokens y coste por árbol o directorio en el encabezado de cada grupo.
- **Alertas.** Una notificación al cruzar el 90 % de una cuota o el 90 % de contexto, una sola vez por
  ventana, con el mecanismo de atención que ya existe.

### 6.6 Privacidad y seguridad

- **Nunca leer ni guardar contenido.**
  - Los parsers deserializan structs con solo los campos de uso, y se descartan las líneas que no pasan el
    filtro por bytes.
  - No se registran líneas ni errores con fragmentos: un error de parseo solo dice «línea N inválida».
  - En opencode se usa `json_extract` de campos numéricos, nunca `data`.
- **Ignorar identificadores de cuenta:** `accountUuid` (Claude), `creator_account_id` y
  `creator_user_id` (Codex), `credits.balance` (Codex), correos. Del statusline, solo los números citados.
- **Rutas.** La ruta del transcript solo vive en memoria del daemon. Por el socket (0600) no viaja más que
  el nombre del agente y los contadores. `galgo usage --json` no imprime rutas salvo con `--debug`.
- **Solo lectura de los archivos de los agentes.** Sin crear locks, sin abrir para escritura y sin abrir
  SQLite con permisos de escritura. Nunca se ejecuta `opencode stats` ni nada que pueda migrar estado.
- **Modificar la configuración del usuario** (el statusline) es opcional, encadenado y reversible, con
  respaldo, igual que los instaladores actuales.
- **Coste de recursos.**
  - Solo lectura incremental, nunca por pulsación de tecla, y en un hilo aparte.
  - Si un archivo sin offset previo pasa de 100 MB, en el arranque solo se lee la cola para el contexto
    actual, y el histórico se deja a `galgo usage --days`.

### 6.7 Orden sugerido

1. `galgo-usage` con los parsers de Claude, Codex y PI, `prices.toml` y pruebas con fixtures sintéticos
   (no copiar transcripts reales).
2. Campo `source` en los hooks de Claude y Codex y en la extensión de PI; `PaneUsage` en `PaneInfo`;
   `galgo usage` en vivo.
3. Statusline de Claude encadenado (cuotas de Claude). Las cuotas de Codex salen del rollout.
4. GUI.
5. opencode, cuando exista su hook, para no depender del apaño por directorio.
6. Histórico `--days`.

## 7. No verificado

- Que `transcript_path` del hook de Codex apunte al rollout vivo, y la fórmula exacta de «context left» de
  la TUI de Codex.
- La ventana real de Claude cuando no hay statusline: 200k o 1M según el modelo y la cuenta.
- Los recargos de Claude por `speed: fast`, `inference_geo` y herramientas de servidor. Los precios de
  OpenAI para modelos fuera de la tabla citada (`gpt-6.1-terra`, `gpt-6-luna`) y por encima de 272K.
- Por qué el `cost` de opencode es 0 en esta máquina, y qué hace `opencode stats` (no se ejecutó).
- Que PI y opencode no expongan cuotas por otra vía (por ejemplo, cabeceras HTTP del proveedor).
- El comportamiento de un statusline encadenado con el de harp en una sesión real.
- Linux: las rutas XDG de opencode y PI pueden cambiar.
