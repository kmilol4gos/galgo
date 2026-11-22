# Galgo: guía de uso diario

Galgo es un terminal experimental para coordinar agentes. El daemon `galgod` mantiene los paneles y su pantalla; la GUI y la CLI se conectan a él. Un **manager** puede delegar en **workers** y recibir sus resultados sin consultar continuamente sus pantallas. Esta guía describe la versión actual en macOS; Linux aún no está probado.

## De herdr a galgo

| Lo que buscabas hacer con herdr | En galgo hoy |
|---|---|
| Abrir y controlar un panel con un agente | GUI «+ Nuevo panel» o `galgo spawn`; selecciona el panel en la barra lateral. |
| Ver su pantalla o enviarle instrucciones | Selecciónalo en la GUI; desde CLI, `galgo read` y `galgo send`. |
| Delegar trabajo y recibir el resultado | `galgo spawn --manager`, `galgo wait` o MCP `galgo_wait`, y `galgo done`. |
| Detectar una decisión pendiente | Insignia de atención en la GUI, `galgo ls` e inbox del manager. La cobertura depende de los hooks del agente. |
| Organizar por workspaces, tabs o splits | Los workspaces agrupan paneles en el árbol de desarrollo actual; se pueden filtrar por CLI y plegar en la GUI. **Tabs y splits aún no están implementados.** |
| Ver estado automático de OpenCode | Puedes iniciar OpenCode como proceso, pero su integración de hooks para galgo **no está implementada**. |

No asumas que la configuración o las variables de herdr se migran automáticamente. Los paneles de galgo usan su propia identidad y socket.

## 1. Instalar y abrir la GUI

Necesitas macOS, Xcode Command Line Tools y [Rust/Cargo](https://rustup.rs). Desde la raíz del repositorio:

```sh
scripts/install.sh
export PATH="$HOME/.galgo/bin:$PATH"
scripts/make-app.sh
open galgo.app
```

`install.sh` compila `galgo`, `galgod`, `galgo-mcp` y `galgo-gui` en release. El directorio predeterminado de los cuatro binarios es `~/.galgo/bin`; si defines `PREFIX`, es **el directorio exacto de binarios**, no su padre. El script imprime cómo añadirlo al PATH y no cambia `.zshrc`. Para un destino distinto usa el mismo `PREFIX` al crear la app. `make-app.sh` crea `./galgo.app`, con sus propios binarios, y no reemplaza una app existente; consulta `scripts/make-app.sh --help` para elegir otra salida.

También puedes extraer un tarball de release para la arquitectura indicada y abrir el `galgo.app` incluido. Mueve la app a `~/Applications` si quieres encontrarla desde Finder, Spotlight o el Dock. La app no está firmada ni notarizada y macOS puede pedir autorización. Al abrirla, se conecta al daemon existente o lo arranca. Cerrar la ventana no es lo mismo que ejecutar `galgo stop`.

## 2. Crear y usar paneles

En la GUI pulsa **+ Nuevo panel**. Elige un nombre no numérico, un workspace si corresponde, un comando predefinido (`claude`, `pi`, `opencode`, `shell`) o `libre`, un directorio absoluto o que empiece por `~/`, y, para un worker, el **Manager (recibe el resultado)**. Si dejas el directorio vacío se usa HOME. Pulsa **Crear** y selecciona el panel en la barra lateral. Los agentes que elijas deben estar instalados y disponibles en el PATH del daemon; el comando libre admite comillas, pero no ejecuta automáticamente una shell ni expande variables.

También puedes hacerlo por CLI. Este ejemplo usa shells para que funcione sin credenciales de un proveedor; sustituye el comando del panel por tu agente instalado para trabajar con él:

```sh
galgo spawn manager -- bash --norc
galgo spawn worker --manager manager -- bash --norc
galgo ls
galgo send worker 'printf "hola desde el worker\n"'
galgo read worker
```

`send` confirma que aceptó el texto en la cola, no que el proceso ya lo haya ejecutado. El nombre debe ser único. En la GUI, «Cerrar panel» pide confirmación y termina el proceso del panel; también puedes usar `galgo kill` (ver abajo).

**Workspaces en desarrollo:** el árbol de trabajo actual permite `--workspace` en `spawn` y `ls`, y la GUI agrupa paneles en secciones plegables. Si se omite, el daemon intenta deducir el grupo del repositorio y la rama; fuera de un repositorio puede quedar sin workspace. El formulario hereda el workspace del panel seleccionado. Este soporte todavía no está en el commit base `bc1258e`; comprueba `galgo spawn --help` en tu binario antes de usarlo, ya que una release anterior puede no tenerlo. En el árbol de desarrollo se probó:

```sh
galgo spawn demo --workspace galgo/main -- bash --norc
galgo ls --workspace galgo/main
galgo kill demo
```

## 3. Manager y workers: esperar sin sondear

En el panel `worker`, cuando termine su tarea, el agente o la persona ejecuta:

```sh
galgo done --summary 'tarea lista'
```

En el panel `manager`, espera una vez por ese worker y turno:

```sh
galgo wait worker --timeout 600
```

Dentro de un panel, `GALGO_PANE` identifica automáticamente quién reporta o espera. Si ejecutas esos comandos desde una terminal **fuera** de galgo, indica el panel con `--pane`; por ejemplo, `--pane worker` para `done` y `--pane manager` para `wait`. Una tarea fallida puede reportarse con `galgo done --failed` y un resumen. No incluyas secretos en el resumen.

`wait` se despierta al recibir la entrega o cuando termina el worker; también puede agotar el tiempo. Un timeout no es éxito: revisa el estado y decide si continuar la espera o resolver un bloqueo. No lances varios waits simultáneos para el mismo worker ni hagas un bucle de `ls`/`read` para esperar. Una espera por cada worker independiente puede correr en paralelo si tu cliente lo permite. `galgo wait` confirma la entrega después de imprimirla correctamente; con `galgo --json wait` puedes usar `msg_id` para deduplicar.

### Si el agente dispone de MCP

Configura `galgo-mcp` como servidor **stdio** en tu cliente MCP; la manera de configurarlo depende del cliente. Debe ejecutarse en el entorno del panel manager para recibir `GALGO_PANE` y `GALGO_SOCKET`. Una llamada a la herramienta real `galgo_wait` usa estos argumentos:

```json
{"target":"worker","timeout_secs":600}
```

La herramienta espera al panel actual; el servidor confirma la entrega después de escribir la respuesta. También existen `galgo_list`, `galgo_spawn`, `galgo_send`, `galgo_read`, `galgo_done`, `galgo_inbox` y `galgo_ack`. No hay herramienta MCP `galgo_env`: para variables usa la CLI.

## 4. Hooks y skill

Instala solo las integraciones que vayas a usar. Los comandos siguientes aceptan la instalación sin preguntar; omite `--yes` si prefieres confirmar de forma interactiva:

```sh
galgo hooks install claude --yes
galgo hooks install pi --yes
galgo hooks install codex --yes
galgo hooks status claude
galgo hooks status pi
galgo hooks status codex
galgo skill install claude --yes
galgo skill status claude
```

Claude modifica sus settings conservando las demás entradas; PI instala una extensión separada; Codex añade entradas a `hooks.json`. Revisa los cambios y respaldos; PI requiere `/reload` o reinicio, y Codex puede pedir confiar los hooks mediante `/hooks`. Los hooks solo reportan dentro de paneles con `GALGO_PANE`. **Codex no informa automáticamente `blocked` ni `failed`**; en PI la cobertura de diálogos es limitada. OpenCode no tiene hook de galgo por ahora. Detalles y riesgos: [hooks-agents.md](hooks-agents.md).

La skill de Claude enseña al manager a coordinar workers; no instala ni configura MCP. Para quitar lo instalado, usa `galgo hooks uninstall` con el agente correspondiente o `galgo skill uninstall claude`. Consulta [skill-claude.md](skill-claude.md) antes de reemplazar una skill editada.

### Uso, contexto y cuotas

Los hooks de Claude y Codex informan al daemon de la sesión y la ruta local del transcript; la extensión PI informa su archivo de sesión y el contexto disponible. El daemon lee esas fuentes localmente. Para ver métricas disponibles:

```sh
galgo usage
galgo usage --pane worker
galgo usage --limits
```

Opcionalmente, `galgo hooks install claude --statusline` encadena el statusline previo de Claude para informar su coste estimado, contexto y cuotas de 5 h/7 días. Guarda el objeto previo en un archivo auxiliar junto a los settings y lo ejecuta con el mismo stdin, conservando su stdout. `galgo hooks uninstall claude` lo restaura si el statusline instalado no fue modificado; si cambió, conserva el respaldo y no pisa los cambios. Con `--scope project` o `local` se usan los settings y respaldo de ese ámbito. Sin statusline, Claude no informa cuotas a galgo.

Los importes de lista y los del agente son estimaciones, no necesariamente tu factura; puede no haber datos o tarifa conocida. Las cuotas pertenecen a la cuenta del agente, no a cada panel. Las rutas de transcript viven en memoria del daemon; los informes no incluyen texto de conversación.

## 5. Variables y bloqueos

Si un worker aparece **blocked** o «necesita una decisión», revisa el inbox del manager y, si hace falta, la pantalla del worker. Para una pregunta o permiso, consulta al humano y transmite solo la decisión; no aceptes acciones sensibles por defecto. Para una variable faltante, desde el mismo directorio del proyecto el worker puede pedirla así:

```sh
galgo env request API_KEY --reason 'pruebas' --timeout 100
```

El manager recibe un aviso y el comando espera a que la variable esté definida en `.env`. Desde una terminal humana, en ese mismo directorio:

```sh
galgo env set API_KEY
galgo env list
```

`set` pide el valor sin eco en un terminal; `list` muestra nombres, no valores, y advierte si el archivo existente no está ignorado por Git. Usa `--dir` y, si corresponde, `--file` en ambos comandos para apuntar al mismo lugar. Si `request` agota el tiempo, el worker puede volver a pedirla. Comprueba los avisos de permisos y `.gitignore`.

**Nunca pongas el valor de un secreto en argv, mensajes al agente, resúmenes, logs ni commits.** El humano debe introducirlo directamente en `galgo env set`, no enviarlo al chat. El valor se escribe localmente al archivo, no pasa por el daemon.

## 6. Atajos y límites de la GUI

La ventana tiene la barra lateral (proyectos, workspaces y paneles, con «Te necesitan» arriba y las cuotas abajo), las pestañas del workspace arriba y una barra de estado abajo (panel, carpeta, tamaño, errores, uso y la confirmación al cerrar). Paleta, lanzador, nuevo workspace y formulario flotan sobre el terminal; `Esc` o un clic fuera los cierra.

| Acción | Cómo |
|---|---|
| Paleta de comandos | `Cmd+K` o `Cmd+Shift+P`; busca workspaces, paneles y acciones. |
| Nuevo workspace / abrir carpeta | `Cmd+N` (recientes con búsqueda) / `Cmd+O` (selector de carpeta). |
| Nueva pestaña · cerrar panel | `Cmd+T` · `Cmd+W` (pide confirmación en la barra de estado si el proceso sigue vivo). |
| Lanzar agente | `Cmd+Shift+L`, luego `1`-`5` (Claude, Codex, PI, opencode, shell). |
| Workspace n · pestaña anterior/siguiente | `Cmd+1`…`Cmd+9` · `Cmd+[` / `Cmd+]`. |
| Dividir · mover el foco | `Cmd+D` (derecha), `Cmd+Shift+D` (abajo) · `Cmd+Opt+flechas`. |
| Redimensionar un split | Arrastra el divisor. |
| Maximizar el panel de un split | `Cmd+Shift+Enter` (otra vez, o enfocar otro panel, lo devuelve). |
| Siguiente que necesita atención | `Cmd+Shift+U`. |
| Tamaño de letra | `Cmd+=` / `Cmd+-` / `Cmd+0`. |
| Editar la línea | `Opt+←/→` salta palabras, `Opt+Borrar` borra una palabra, `Cmd+←/→` va al inicio/fin, `Cmd+Borrar` borra hasta el inicio, `Shift+Enter` es un salto de línea (Claude Code). |
| Seleccionar / copiar | Arrastra dentro de la cuadrícula; `Cmd+C` copia la selección. |
| Pegar | `Cmd+V`; se respeta bracketed paste cuando el programa lo activa. |
| Consultar historial | Rueda o trackpad sobre la cuadrícula; no sobre la barra lateral. |
| Salir de una selección congelada | `Esc`. También vuelve al escribir o interactuar con la pantalla. |
| Interrumpir el programa del panel | `Ctrl+C` se envía al panel; es distinto de copiar con `Cmd+C`. |

La selección congela la imagen visible mientras está activa; el proceso puede seguir trabajando. Con caracteres anchos o combinados, la selección, el cursor y las columnas pueden no coincidir. En aplicaciones de pantalla alterna como `vim`, `less` o `htop`, el ratón y la rueda no se transmiten como en un emulador completo. La GUI sondea snapshots en lugar de suscribirse a eventos. Los paneles visibles de la pestaña se redimensionan a su parte del split; los paneles de otras pestañas conservan su tamaño hasta que se muestran. Más contexto y límites históricos en [gui-plan.md](gui-plan.md).

## 7. Cuando algo se cuelga

1. Comprueba los paneles y mensajes:

   ```sh
   galgo ls
   galgo inbox --pane manager
   galgo read worker
   ```

   `blocked` puede requerir una pregunta o un permiso; `exited` significa que el proceso terminó. «ENTREGA SIN CONFIRMAR» indica un mensaje pendiente, no necesariamente un proceso colgado. El inbox muestra cada mensaje con su ID; tras procesarlo, usa `galgo ack --pane manager` seguido de ese ID. Deduplica por ID si ves una entrega de nuevo.
2. Inspecciona `~/.galgo/galgod.log` si el socket es el predeterminado. Con `GALGO_SOCKET` personalizado, el log `galgod.log` está junto a ese socket. No compartas el log sin revisarlo por posibles datos sensibles.
3. Para terminar un panel concreto:

   ```sh
   galgo kill worker
   ```

   Esto termina la sesión del panel; un proceso que haya creado otra sesión puede escapar. Si necesitas detener todo el daemon, `galgo stop` afecta a **todos** sus paneles, no solo al seleccionado.
4. Si reinicias el daemon, los mensajes sin confirmar persisten, pero los procesos de los paneles no se reanudan: los paneles reaparecen como `exited`. Crea de nuevo el panel que necesites. El estado está en `~/.galgo/state.json` junto al socket por defecto; `GALGO_STATE` puede cambiar la ruta. Consulta la sección «Estado entre reinicios» del [README](../README.md).

Para verificar cualquier opción de tu versión, usa `galgo --help` y la ayuda del subcomando correspondiente.
