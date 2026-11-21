# Integración de hooks de PI, OpenCode y Codex

Investigación: 21-11-2026. Localmente se consultaron `--help`, versiones y archivos de configuración/extensiones, sin modificar la configuración del usuario ni ejecutar una sesión de modelo. Versiones: **PI 1.5.2** (`@earendil-works/pi-coding-agent`), **OpenCode 1.18.33**, **codex-cli 0.167.0**. Las páginas oficiales pueden describir versiones posteriores; se distinguen de lo observado en la instalación. No se muestran valores privados de las configuraciones locales.

## Punto de partida y límites

`crates/galgo-proto/src/lib.rs` define `HookEvent`: `prompt_submitted`, `working`, `idle`, `blocked`, `turn_ended` y `failed`. El daemon los interpreta según el panel: `turn_ended` puede entregar un resultado al manager; no equivale simplemente a `idle`. Es importante no producir dos resultados por un `galgo done` explícito seguido de eventos de herramientas/finalización.

**Implementados: `galgo hook claude` y `galgo hook pi`.** El instalador de Claude modifica settings de Claude; PI instala una extensión separada. Ambos leen JSON por stdin, buscan `GALGO_PANE`, conectan al socket sin arrancar el daemon, recortan resúmenes, no imprimen stdout y retornan 0. Los ejemplos `galgo hook opencode|codex` son **propuestas no implementadas**.

Fuentes: [PI: extensiones](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md), también leída en la instalación 1.5.2; [OpenCode: plugins](https://opencode.ai/docs/plugins/), [construcción](https://opencode.ai/docs/build/plugins/) y [server](https://opencode.ai/docs/build/plugins/server/); [Codex: hooks](https://developers.openai.com/codex/hooks/) y [referencia de configuración](https://developers.openai.com/codex/config-reference/). La validez de cada integración en una sesión interactiva real queda **no verificada**.

## PI

**Mecanismo y ubicación.** Extensión TypeScript/JavaScript cargada en el proceso PI. Descubrimiento automático: `~/.pi/agent/extensions/galgo.ts` global o `.pi/extensions/galgo.ts` en el proyecto; esta última requiere que el proyecto sea confiable. Alternativamente, añadir a `~/.pi/agent/settings.json` (JSON, conservando el resto) un array `"extensions": ["/ruta/absoluta/galgo.ts"]`, o probar sin instalar con `pi -e /ruta/galgo.ts`. La instalación local tiene `~/.pi/agent/extensions/herdr-agent-state.ts`; es independiente y no debe modificarse. `pi --help` confirma `-e` y `--no-extensions`.

**Mapeo propuesto, API PI 1.5.2:**

| Evento PI | HookEvent galgo | Precaución |
|---|---|---|
| `before_agent_start`, con `event.messageType === "user"` | `prompt_submitted` | Excluir arranques `custom`; verificar mensajes encolados y extensiones. |
| `agent_start` | `working` | También ocurre en reintentos/continuaciones; no reiniciar el turno aquí. |
| `ui_prompt_start` / `ui_prompt_end` | `blocked` / `working` | Solo diálogos de extensiones de `ctx.ui`; no implica cobertura de toda espera humana. Son best-effort y no esperados antes de mostrar el diálogo. |
| `agent_settled` | `turn_ended` o `failed` | Es la frontera final; `agent_end` y `turn_end` pueden preceder reintentos, compactación o continuaciones. `agent_settled` expone `aborted`, pero clasificación exacta de fallo y extracción del último mensaje: **no verificadas**. No declarar éxito ante error/abort sin resolverlo. |
| estado inicial sin ejecución | `idle` | Obtener `ctx.isIdle()` en `session_start`; no convertir automáticamente cada `agent_end` en idle. |

**Implementación:** `galgo hooks install pi --yes` escribe `$HOME/.pi/agent/extensions/galgo.ts`, sin tocar settings ni extensiones ajenas; `--dry-run`, `status` y `uninstall` están disponibles. Un archivo ajeno existente se respalda como `galgo.ts.galgo-bak` y se restaura al desinstalar; un respaldo previo impide reemplazar un archivo ajeno. Se rechazan symlinks. Solo se admite `--scope user`. Para aplicar cambios en PI usar `/reload` o reiniciar.

PI 1.5.2 documenta `pi.exec` con timeout pero no una opción para enviar stdin; la extensión usa `node:child_process.spawn` directamente, sin shell, con stdin canalizado, stdout/stderr descartados, timeout de 1 s y envíos ordenados. La ruta del ejecutable se fija al instalar; reinstalar si cambia. Payload JSON de `galgo hook pi`:

| `type` | Campos | Mapeo |
|---|---|---|
| `session_start` | `idle`: boolean | `idle` o `working` |
| `before_agent_start` | `messageType`: `user` o `custom` | solo `user` → `prompt_submitted` |
| `agent_start` | — | `working` |
| `ui_prompt_start` | `title`: texto opcional | `blocked` |
| `ui_prompt_end` | — | `working`, solo si la extensión considera activo el agente |
| `agent_settled` | `outcome`: `completed`, `failed` o `unknown`; `summary` y `error`: textos opcionales | `turn_ended`, `failed` o `idle`, respectivamente |

La extensión observa `agent_end` para capturar el último mensaje assistant; solo emite resultado en `agent_settled`. `stopReason=stop` significa `completed`; `error`, `aborted` o `event.aborted` significan `failed`; otro caso es `unknown` y no entrega éxito. Esto es conservador: la semántica en retries, diálogos best-effort y sesiones reales sigue requiriendo validación. Ejemplo de payload directo (solo dentro de un panel):

```sh
printf '%s\\n' '{"type":"agent_start"}' | galgo hook pi
```

## OpenCode

**Mecanismo y ubicación.** Plugin de servidor JS/TS. En la instalación local existe `~/.config/opencode/plugins/herdr-agent-state.js`; no editarlo. El lugar propuesto es `~/.config/opencode/plugins/galgo.js` o `.opencode/plugins/galgo.js` del proyecto, descubiertos automáticamente. En la documentación oficial actual, los directorios de plugin pueden configurarse mediante la propiedad JSON/JSONC `"plugins"` de `~/.config/opencode/opencode.json(c)`, `./opencode.json(c)` o `./.opencode/opencode.json(c)`; las rutas locales configuradas son **directorios**, no archivos sueltos. La versión local incluye una API de plugin V1 y tipos instalados `@opencode-ai/plugin`; la documentación actual también describe una API nueva `@opencode/plugin`/`Plugin.define`. **Compatibilidad exacta entre la documentación actual y el binario 1.18.33: no verificada**; fijar versión y probar antes de instalar.

**Mapeo propuesto**, basado en el plugin local y tipos SDK locales `@opencode-ai/sdk/dist/v2/gen/types.gen.d.ts`:

| Evento/hook | HookEvent | Precaución |
|---|---|---|
| `chat.message` (`sessionID`) | `prompt_submitted` | Distinguir mensaje humano, sesión raíz y mensajes de otras sesiones; **semántica exacta no verificada**. |
| `session.status` con `properties.status.type` `busy` o `retry` | `working` | `retry` no es fallo final; filtrar por `properties.sessionID`. |
| `permission.asked`, `question.asked` | `blocked` | `permission.replied`, `question.replied` o `question.rejected` pueden reanudar `working`; manejar solicitudes simultáneas. |
| `session.idle` o `session.status` `idle` | candidato a `turn_ended` | Ambos pueden duplicar notificación; comprobar que terminó la tarea, no solo que quedó idle, y obtener respuesta final mediante API. Sin evidencia de término inequívoco, emitir solo `idle`. |
| `session.error` | candidato a `failed` | `properties.sessionID` y `error` son opcionales; distinguir error final, reintento y cancelación antes de reportar. |

El servidor puede servir múltiples sesiones/clientes: un plugin global no debe atribuir todos los eventos al mismo `GALGO_PANE`. El plugin local de Herdr distingue sesiones hijas y excluye ciertos modos `serve`, `web` y `attach`; esto es evidencia de una dificultad, no una garantía para galgo. Identificación fiable de la sesión raíz y propagación de `GALGO_PANE`/`GALGO_SOCKET` con TUI y servidor compartido: **no verificadas**.

Ejemplo mínimo **propuesto para el estilo V1 observado localmente**, `~/.config/opencode/plugins/galgo.js`. Requiere implementar `galgo hook opencode` con el JSON del evento; no usar aún en un servidor compartido:

```js
import { spawn } from "node:child_process";
export const GalgoPlugin = async () => ({
  event: async ({ event }) => {
    if (!process.env.GALGO_PANE || event.type !== "session.idle") return;
    const child = spawn("galgo", ["hook", "opencode"], { stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(event));
  },
});
```

El ejemplo no identifica la sesión ni limita la duración del hijo: es ilustrativo, **no apto para producción**. Confirmar si el cargador local acepta ese export y sustituir por la API oficial que corresponda a la versión elegida; agregar timeout y filtro de sesión.

## Codex

**Mecanismo y ubicación.** Hooks de comando, un objeto JSON por stdin. La documentación oficial describe `$CODEX_HOME/hooks.json` (por defecto `~/.codex/hooks.json`) y `.codex/hooks.json` de un proyecto confiable, o declaraciones `[hooks]` en el `config.toml` de la misma capa. También hay hooks de sistema y de plugins. En este Mac existe `~/.codex/hooks.json` con grupos `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `SubagentStart`, `SubagentStop` y `Stop`; `codex features list` muestra `hooks stable true`. Esto verifica presencia y configuración, no que se hayan disparado. Codex combina fuentes y exige revisar/confiar hooks no gestionados mediante `/hooks`; si se registran dos veces pueden ejecutarse dos veces. No sobrescribir el archivo ni las entradas existentes.

**Mapeo propuesto:** `UserPromptSubmit` → `prompt_submitted`; `PostToolUse` → `working`; `PermissionRequest` → candidato a `blocked` (se ejecuta cuando va a pedir aprobación; verificar si la política o el hook evita el diálogo); `Stop` → candidato a `turn_ended` con `last_assistant_message`. El payload común incluye `hook_event_name`, `session_id`, `cwd` y `transcript_path`; `Stop` añade `turn_id`, `stop_hook_active` y `last_assistant_message`. `Stop` puede continuar el turno por otro hook: confirmar la semántica antes de reportar resultado. `PreToolUse` no demuestra por sí solo que el agente esté esperando. `SessionStart` puede inicializar `idle`; `SessionEnd` no implica éxito. No se verificó un evento de fallo final equivalente a `failed`, ni un evento fiable de cierre de aprobación que garantice `working` en todos los casos. No confundir `SubagentStop` con el turno del panel padre.

Ejemplo de **fragmento** para fusionar en `$CODEX_HOME/hooks.json`, no reemplazar el archivo. `galgo hook codex` aún no existe. Codex documenta que `Stop` espera JSON en stdout: el wrapper devuelve `{}` y no inyecta salida al contexto, incluso si galgo falla. Verificar en la versión instalada que stdout vacío/`{}` y el timeout se comportan como se espera:

```json
{
  "hooks": {
    "Stop": [
      { "hooks": [
        { "type": "command", "command": "galgo hook codex >/dev/null 2>&1; printf '{}'", "timeout": 3 }
      ] }
    ]
  }
}
```

El JSON original llega a stdin de `galgo hook codex` mediante el wrapper de shell; no se interpola en el comando. Instalar sin alterar otras entradas y presentar la revisión de confianza a la persona usuaria. Evitar imprimir texto de prompts o secretos en logs.

## Orden y pruebas de aceptación propuestos

1. **Codex:** estructura local de hooks ya presente y payload de `Stop` documentado. Probar instalación/desinstalación idempotente y confianza, prompt → trabajo → permiso real → reanudación → Stop, resumen, `galgo done` + PostToolUse + Stop sin duplicación, fallo/cancelación y daemon ausente. Verificar Stop continuado por otro hook y subagentes; no anunciar `failed` hasta identificarlo.
2. **PI:** extensión aislada con `-e`, luego descubrimiento global/proyecto confiable; probar `agent_settled` con retries, compactación, abort y mensajes encolados; UI prompts anidados, resumen y errores, recarga y ausencia del daemon. Verificar que no se informe `turn_ended` prematuramente.
3. **OpenCode:** primero resolver identidad de sesión/servidor por panel y escoger API/versiones; probar TUI local, `run`, `attach` y servidor compartido, sesiones hijas, permisos/preguntas simultáneos, `session.error` con retry y duplicados `idle/status`; solo después habilitar resultados automáticos.

Para los tres: pruebas de mapeo con payloads capturados y anonimizados, sin stdout inesperado, timeout, resumen acotado, socket inexistente, permisos y preservación de configuración ajena. La instalación solo debe ser opt-in y explicar código ejecutable y riesgos. Las pruebas automatizadas PI cubren el adaptador y el instalador; los flujos reales de agente siguen sin verificarse.
