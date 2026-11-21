# galgo

**Terminal para trabajar con varios agentes de código. Rápida como un galgo, y no deja a ningún agente esperando.**

> *Galgo* es *greyhound* en español, el perro más rápido. Y en inglés se lee «gal-go»: *go*.

**Estado: alfa.** Probado en macOS. Linux todavía no se ha probado. No hay interfaz gráfica aún: hoy es un daemon, una CLI y un servidor MCP.

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

Requiere [Rust](https://rustup.rs).

```sh
git clone https://github.com/kmilol4gos/galgo
cd galgo
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

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

### Servidor MCP

`galgo-mcp` expone el daemon a cualquier cliente MCP por stdio: `galgo_list`, `galgo_spawn`, `galgo_send`, `galgo_read`, `galgo_wait`, `galgo_done`, `galgo_inbox` y `galgo_ack`.

Hay un skill listo para enseñar a un agente a usar galgo en [`skills/galgo`](skills/galgo/SKILL.md).

## Seguridad

- El socket queda con permisos `0600` en un directorio `0700`: quien lo alcance puede escribir en todos los paneles.
- `--inject` escribe texto en la terminal de otro panel. Úsalo con agentes, no con shells. Los caracteres de control se filtran.
- Los secretos no viajan por el protocolo. Ningún comando de galgo lee ni escribe archivos `.env`.
- Los hooks nunca imprimen por stdout ni fallan: salen siempre con código 0.

## Pruebas

```sh
cargo test
scripts/e2e.sh     # daemon real, PTYs reales, agentes simulados
```

## Hoja de ruta

- Interfaz gráfica nativa para macOS, cliente del daemon con varias ventanas. Plan en [`docs/gui-plan.md`](docs/gui-plan.md).
- Panel de variables de entorno: poner una variable en varios worktrees sin abrir un IDE, y que un agente la pida sin que el valor pase por su conversación.
- Estado de agentes por la secuencia OSC 7501 y hooks para más agentes además de Claude Code.
- Persistencia del inbox entre reinicios del daemon.
- Linux.

## Licencia

MIT o Apache-2.0, a tu elección. Ver [LICENSE-MIT](LICENSE-MIT) y [LICENSE-APACHE](LICENSE-APACHE).
