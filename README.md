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

El instalador compila en release `galgo`, `galgod`, `galgo-mcp` y `galgo-gui`; la GUI usa un `CARGO_TARGET_DIR` independiente. Instala los cuatro juntos en `~/.galgo/bin`, sin sudo, e imprime la línea para el PATH de bash/zsh sin modificar tu `.zshrc`. Si ya hay un daemon corriendo, sigue usando la versión y el entorno anteriores: guarda el trabajo de sus paneles y ejecuta `galgo stop` para que el siguiente cliente arranque el nuevo (esto detiene todos los paneles). Para elegir otro destino, `PREFIX` es **el directorio exacto de binarios**, no su padre; usa `$HOME` en lugar de una tilde literal entre comillas:

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

Puedes mover `galgo.app` a `~/Applications` o `/Applications` y abrirla desde Finder, Spotlight o el Dock. El bundle lleva sus propios binarios; conecta con el daemon existente o lo inicia si no corre. Al iniciarlo añade al PATH `~/.local/bin`, `~/.opencode/bin`, `~/.cargo/bin`, `~/.npm-global/bin`, `~/.bun/bin`, `/opt/homebrew/bin` y `/usr/local/bin`. **No ejecuta la shell de login ni carga variables de `.zshrc`**: configura aparte las variables de proveedor que necesiten los agentes. Si el daemon ya estaba activo, conserva su PATH y entorno anteriores; detenerlo con `galgo stop` cierra sus paneles. El lanzador muestra una alerta si no puede conectar o arrancar el daemon.

Por ahora la app usa el icono genérico de macOS y no está firmada ni notarizada; Gatekeeper puede bloquear la app descargada y requerir autorización. El script requiere Python 3 y no reemplaza una app existente, tampoco si aparece durante la creación. Para elegir rutas, consulta `scripts/make-app.sh --help`.

Los tags `v*` activan el workflow de release de macOS, pero solo se publican los que cumplen `vMAJOR.MINOR.PATCH` con un sufijo opcional `-PRERELEASE` de letras, números y puntos. El workflow publica un `.tar.gz` con `bin/` y `galgo.app`; los binarios son para la arquitectura del runner indicada en el nombre, no son universales. Se recomienda proteger los tags `v*` con reglas del repositorio.

## Uso

Para el flujo diario, la GUI y la transición desde herdr, consulta la [guía de uso](docs/guia-uso.md).

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

### Workspaces

Un workspace agrupa los paneles de un proyecto o de una feature: `inventia/cotizacion`, `galgo/main`. Es un nombre libre.

- **Al crear:** `galgo spawn w --workspace inventia/cotizacion -- claude`. Sin `--workspace`, el daemon lo deduce del repo git del directorio (`--cwd`, o el actual): `<repo>/<rama>`. Fuera de un repo no hay workspace.
- **Al listar:** `galgo ls` agrupa por workspace, con un resumen de atención por grupo (bloqueados, entregas pendientes). `galgo ls --workspace W` muestra solo ese grupo, también con `--json`. Las herramientas MCP `galgo_spawn` y `galgo_list` aceptan el mismo campo.
- **En la GUI:** la barra lateral agrupa los paneles. Cada grupo se pliega con su cabecera. El formulario de «Nuevo panel» hereda el workspace del panel seleccionado, y el título de la ventana cuenta los paneles que piden atención en todos los grupos.
- **Estado:** el workspace se guarda con el resto del estado (ver «Estado entre reinicios»). Un estado anterior sin ese campo se carga igual.

### Estado entre reinicios

El daemon guarda el inbox sin confirmar y la lista de paneles en un archivo junto al socket: `galgo.sock` → `galgo.state.json` (`~/.galgo/galgo.state.json` por defecto; `GALGO_STATE` lo cambia). Así dos daemons en el mismo directorio no comparten estado. La escritura es atómica, el archivo queda con permisos `0600`, y el directorio debe ser privado (no escribible por otros).

- **Cuándo se guarda.** Solo cuando cambia el inbox o la lista de paneles, agrupado: un guardado espera 200 ms y escribe una vez. Los `send` no escriben nada, y el apagado guarda lo pendiente antes de salir.
- **Mensajes sin confirmar** siguen en el inbox tras reiniciar: un resultado no se pierde.
- **Los paneles no sobreviven.** Cada panel que existía vuelve como panel sin proceso (`exited` en `galgo ls`), y su manager recibe el aviso «el daemon se reinició; el panel X ya no existe». Un panel sin proceso y sin mensajes pendientes no se guarda.
- **Reutilizar y quitar.** `galgo spawn` con el nombre de un panel sin proceso lo reemplaza y hereda su inbox. `galgo kill` quita un panel sin proceso y pasa sus mensajes pendientes a su manager. Si no tiene manager vivo y tiene mensajes pendientes, `kill` se rechaza: confírmalos con `galgo ack` antes.
- **Ids de mensaje con epoch.** Cada arranque tiene un epoch (los segundos de la hora de arranque) en la parte alta del id. Un `ack` de otra ejecución que ya no existe se rechaza sin confirmar nada. El epoch no depende del archivo: si el estado falta o está corrupto, no vuelve a 1.
- **Un archivo que no sirve se aparta, no se sobrescribe.** Si no es un archivo regular, es demasiado grande, está corrupto o es de otra versión, el daemon lo ignora con aviso en el log (`~/.galgo/galgod.log`) y lo deja como `<nombre>.corrupt-<segundos>` para revisarlo. Arranca sin estado.

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
