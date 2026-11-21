# galgo

**Terminal para trabajar con varios agentes de código. Rápida como un galgo, y no deja a ningún agente esperando.**

> *Galgo* es *greyhound* en español, el perro más rápido. Y en inglés se lee «gal-go»: *go*.

**Estado: alfa.** Probado en macOS; la interfaz gráfica es experimental. Linux todavía no se ha probado.

## El problema

Cuando trabajas con varios agentes (Claude Code, PI, opencode, Codex…), un manager reparte tareas y espera los resultados. Pero el manager no siempre se entera de que un worker terminó, o de que se quedó esperando un permiso. El flujo queda a medias y tú lo descubres horas después.

## Qué hace galgo

Un daemon (`galgod`) es dueño de los PTYs y de la pantalla de cada panel, y entrega los resultados **con acuse de recibo**:

- `galgo done` registra el resultado de un worker y lo deja en el inbox de su manager.
- `galgo wait` se despierta en cuanto llega. Si el worker ya había terminado, vuelve de inmediato.
- Un resultado que nadie confirma (`galgo ack`) aparece en `galgo ls` como `ENTREGA SIN CONFIRMAR`.
- Un worker que se bloquea esperando una decisión humana se lo avisa a su manager, en vez de dejarlo esperando para siempre.
- Si un worker muere sin reportar, el manager recibe un mensaje `exited` con el código de salida.
- Con `--inject`, el daemon escribe el resultado en el panel del manager cuando está libre.

## Instalación

Requiere macOS y [Rust](https://rustup.rs) con Cargo. Desde el repositorio:

```sh
git clone https://github.com/kmilol4gos/galgo
cd galgo
scripts/install.sh
export PATH="$HOME/.galgo/bin:$PATH"
```

El instalador compila en release `galgo`, `galgod`, `galgo-mcp` y `galgo-gui`; la GUI usa un `CARGO_TARGET_DIR` independiente. Instala los cuatro juntos en `~/.galgo/bin`, sin sudo, e imprime la línea para el PATH sin modificar tu `.zshrc`. Para elegir otro destino, `PREFIX` es **el directorio exacto de binarios**, no su padre:

```sh
PREFIX="$HOME/bin" scripts/install.sh
export PATH="$HOME/bin:$PATH"
scripts/install.sh --help
```

Para crear una app autónoma de macOS a partir de esos binarios:

```sh
scripts/make-app.sh                       # crea ./galgo.app
# Si usaste PREFIX, pásalo también aquí:
# PREFIX="$HOME/bin" scripts/make-app.sh
open galgo.app
```

Puedes mover `galgo.app` a `~/Applications` o `/Applications` y abrirla desde Finder, Spotlight o el Dock. El bundle lleva sus propios binarios; conecta con el daemon existente o lo inicia si no corre. Por ahora usa el icono genérico de macOS y no está firmado ni notarizado; macOS puede exigir autorización para abrirlo. El script no reemplaza una app existente. Para elegir rutas, consulta `scripts/make-app.sh --help`.

Los tags `v*` activan el workflow de release de macOS, que publica un `.tar.gz` con `bin/` y `galgo.app`. Los binarios del archivo son para la arquitectura del runner indicada en el nombre; no son universales.

## Uso

```sh
galgo spawn manager -- claude
galgo spawn reviewer --manager manager -- claude --model opus
galgo ls
galgo send reviewer "revisa la rama"

# dentro del panel del manager (--pane sale de $GALGO_PANE):
galgo wait reviewer --timeout 600

# dentro del panel del worker:
galgo done --summary "aprobado con 2 comentarios"

galgo read reviewer      # lee la pantalla de un panel
galgo stop               # detiene el daemon
```

`galgo send` responde `ok` cuando el texto queda en la cola del panel, no cuando llega a su PTY. Si un panel no lee su entrada, la cola se llena (8 envíos) y los siguientes reciben un error claro en vez de congelar el daemon.

El daemon se inicia solo. Cada panel recibe `GALGO_PANE`, `GALGO_PANE_NAME` y `GALGO_SOCKET`. El socket es `~/.galgo/galgo.sock` (o `$GALGO_SOCKET`).

### Un equipo completo

```sh
PLANNER_MODEL=openai/<modelo> scripts/team.sh miproyecto /ruta/al/proyecto
```

Levanta un manager (Claude Code), un planner (PI), un programmer (Claude con Haiku) y un reviewer (Claude con Opus); los tres últimos reportan al manager. Ver el encabezado del script para todas las opciones.

### Hooks de Claude Code

Para que el estado (`working`, `blocked`, `done`) y los resultados se actualicen solos:

```sh
galgo hooks install claude        # muestra el plan y pide confirmación
galgo hooks status claude
galgo hooks uninstall claude      # quita solo lo de galgo
```

Hace una copia de respaldo de tu `settings.json`, conserva lo que ya tengas y no toca un archivo que no sea JSON válido. Los hooks solo actúan dentro de paneles de galgo.

### Variables de entorno sin abrir un IDE

```sh
galgo env set API_KEY --dir ~/proy/main --dir ~/proy/feature-x   # pide el valor sin eco
echo "$TOKEN" | galgo env set API_KEY --dir .                      # o por stdin
galgo env list --dir ~/proy/main     # nombres, nunca valores; avisa si el archivo no está en .gitignore
galgo env unset API_KEY --dir .
```

Un agente que necesita una variable la pide sin que el valor pase por su conversación:

```sh
galgo env request DB_URL --reason "migraciones"   # avisa al manager, queda blocked y sigue cuando aparece
```

El valor nunca va por argv ni por el daemon; se escribe de forma atómica, conserva lo que ya hay en el archivo y los archivos nuevos quedan en `0600`.

### Servidor MCP

`galgo-mcp` expone el daemon a cualquier cliente MCP por stdio: `galgo_list`, `galgo_spawn`, `galgo_send`, `galgo_read`, `galgo_wait`, `galgo_done`, `galgo_inbox` y `galgo_ack`.

Hay un skill listo para enseñar a un agente a usar galgo en [`skills/galgo`](skills/galgo/SKILL.md).

### Estado entre reinicios

El daemon guarda en `state.json`, junto al socket (`~/.galgo/state.json`; `GALGO_STATE` lo cambia), los mensajes del inbox que nadie ha confirmado y la lista de paneles. La escritura es atómica y el archivo queda con permisos `0600`. Se guarda al cambiar algo, no en cada lectura.

Al reiniciar:

- **Los mensajes sin confirmar siguen en el inbox.** Un resultado no se pierde por reiniciar el daemon.
- **Los paneles no sobreviven.** Su proceso muere con el daemon. Cada panel que existía vuelve como panel sin proceso: aparece en `galgo ls` con estado `exited`, y su manager recibe un aviso («el daemon se reinició; el panel X ya no existe»). Un `spawn` con el mismo nombre lo reemplaza y hereda su inbox.
- **Los ids de mensaje llevan un epoch** en la parte alta (`epoch << 32 | secuencia`). Un `ack` de un mensaje de la ejecución anterior confirma ese mensaje si sigue en el inbox. Un id de otra ejecución que ya no existe se rechaza con un aviso, sin confirmar nada.
- **Un archivo corrupto o de otra versión se ignora** con un aviso en el log del daemon (`~/.galgo/galgod.log`). El daemon arranca con un estado vacío y lo sobrescribe en el siguiente cambio.

## Seguridad

- El socket queda con permisos `0600` en un directorio `0700`: quien lo alcance puede escribir en todos los paneles.
- `--inject` escribe texto en la terminal de otro panel. Úsalo con agentes, no con shells. Los caracteres de control se filtran.
- Los secretos no viajan por el protocolo ni por argv. Solo `galgo env` toca archivos `.env`, y lo hace localmente.
- Los hooks nunca imprimen por stdout ni fallan: salen siempre con código 0.
- Los paneles no heredan los marcadores de la sesión que arrancó el daemon (`CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_MESSAGING_*`, …, y todo `HERDR_*`). La configuración del usuario sí pasa (proveedor, tokens, límites). `CLAUDE_EFFORT` pasa a propósito: es una preferencia, no una identidad. La lista está en `is_session_marker` (`crates/galgo-daemon/src/main.rs`).
- `galgo kill` mata la sesión del panel (incluidos los trabajos de su shell que ignoran SIGHUP). Un proceso que crea su propia sesión con `setsid()` escapa, y `galgo kill` no lo alcanza.

## Pruebas

```sh
cargo test
scripts/e2e.sh     # daemon real, PTYs reales, agentes simulados
```

## Hoja de ruta

- Completar la interfaz gráfica nativa para macOS, cliente del daemon con varias ventanas. Plan en [`docs/gui-plan.md`](docs/gui-plan.md).
- Panel gráfico de variables de entorno (la CLI `galgo env` ya existe).
- Estado de agentes por la secuencia OSC 7501 y hooks para más agentes además de Claude Code.
- Persistencia del inbox entre reinicios del daemon.
- Linux.

## Licencia

MIT o Apache-2.0, a tu elección. Ver [LICENSE-MIT](LICENSE-MIT) y [LICENSE-APACHE](LICENSE-APACHE).
