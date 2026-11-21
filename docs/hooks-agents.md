# Integración de hooks de PI, OpenCode y Codex

Investigación: 21-11-2026. Localmente se consultaron `--help`, versiones y archivos de configuración/extensiones sin modificar la configuración del usuario; también se probaron sesiones aisladas, incluido Codex con un servidor de modelo simulado. Versiones: **PI 1.5.2** (`@earendil-works/pi-coding-agent`), **OpenCode 1.18.33**, **codex-cli 0.167.0**. Las páginas oficiales pueden describir versiones posteriores; se distinguen de lo observado en la instalación. No se muestran valores privados de las configuraciones locales.

## Punto de partida y límites

`crates/galgo-proto/src/lib.rs` define `HookEvent`: `prompt_submitted`, `working`, `idle`, `blocked`, `turn_ended` y `failed`. El daemon los interpreta según el panel: `turn_ended` puede entregar un resultado al manager; no equivale simplemente a `idle`. Es importante no producir dos resultados por un `galgo done` explícito seguido de eventos de herramientas/finalización.

**Implementados: `galgo hook claude`, `galgo hook pi` y `galgo hook codex`.** El instalador de Claude modifica settings de Claude; PI instala una extensión separada y Codex fusiona entradas en su `hooks.json`. Los tres leen JSON por stdin, buscan `GALGO_PANE`, conectan al socket sin arrancar el daemon, recortan resúmenes, no imprimen stdout y retornan 0. `galgo hook opencode` sigue **sin implementar**.

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

**Comprobación local adicional (21-11-2026).** Con `opencode 1.18.33 run`, HOME y XDG temporales, un plugin V1 de prueba se cargó y recibió `GALGO_PANE` y `GALGO_SOCKET`. Para un modelo inexistente observamos `session.created` con `properties.sessionID` e `info` sin `parentID`, `chat.message` con `sessionID`, `session.status` `busy`, `session.error`, `session.status` `idle` y `session.idle`. El constructor del plugin se invocó dos veces, aunque en esta prueba cada evento observado se registró una vez. Esto no demuestra cómo vincular una sesión raíz con un panel cuando hay varias sesiones, servidores compartidos o `attach`, ni verifica permisos, sesiones hijas o un turno exitoso. En particular, tras el error hubo eventos idle: **idle no demuestra éxito**. No se han implementado adaptador ni instalador OpenCode hasta resolver la identidad y esos casos.

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

**Comprobación local adicional (21-11-2026).** Con `codex-cli 0.167.0 exec`, HOME y `CODEX_HOME` temporales, `hooks.json` aislado y `--dangerously-bypass-hook-trust` solo para la prueba, se capturaron por stdin `SessionStart` (`cwd`, `hook_event_name`, `model`, `permission_mode`, `session_id`, `source`, `transcript_path`) y `UserPromptSubmit` (los campos comunes, `prompt` y `turn_id`). Con un servidor Responses simulado en localhost se completó un turno y se capturó `Stop`, con los campos comunes más `turn_id`, `stop_hook_active=false` y `last_assistant_message` con el texto simulado. El comando de prueba devolvió `{}` y Codex finalizó correctamente. Esto verifica el formato para ese flujo, no la interacción con otros hooks que continúen el turno, aprobación, herramientas, subagentes ni cancelación. La opción que evita la revisión de confianza es peligrosa y no debe usarse automáticamente en la instalación.

**Mecanismo y ubicación.** Hooks de comando, un objeto JSON por stdin. La documentación oficial describe `$CODEX_HOME/hooks.json` (por defecto `~/.codex/hooks.json`) y `.codex/hooks.json` de un proyecto confiable, o declaraciones `[hooks]` en el `config.toml` de la misma capa. También hay hooks de sistema y de plugins. En este Mac existe `~/.codex/hooks.json` con grupos `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `SubagentStart`, `SubagentStop` y `Stop`; `codex features list` muestra `hooks stable true`. Esto verifica presencia y configuración, no que se hayan disparado. Codex combina fuentes y exige revisar/confiar hooks no gestionados mediante `/hooks`; si se registran dos veces pueden ejecutarse dos veces. No sobrescribir el archivo ni las entradas existentes.

**Implementación y límites:** `galgo hook codex` mapea `SessionStart` → `idle`, `UserPromptSubmit` → `prompt_submitted` y `Stop` → `turn_ended` con `last_assistant_message` recortado a 600 caracteres. El payload común incluye `hook_event_name`, `session_id`, `cwd` y `transcript_path`; `Stop` añade `turn_id`, `stop_hook_active` y `last_assistant_message`. No se emiten `blocked` ni `failed`: la semántica de aprobación, errores y reintentos no se ha verificado. Tampoco se traducen `PostToolUse` ni `SubagentStop`. La entrega en `Stop` se probó con un servidor simulado; **otro hook podría continuar el turno después de `Stop`**, caso aún no verificado y que podría ocasionar una entrega prematura.

`galgo hooks install codex --yes` agrega tres grupos identificados en `$CODEX_HOME/hooks.json`, o `$HOME/.codex/hooks.json` si `CODEX_HOME` no está definido. Solo se admite `--scope user`; también existen `--dry-run`, `status` y `uninstall`. Conserva las entradas ajenas y crea `hooks.json.galgo-bak` si el archivo ya existía, sin sobrescribir un respaldo anterior. La desinstalación elimina solo los comandos marcados de galgo, conserva cambios posteriores del usuario y deja el respaldo intacto; no restaura una copia potencialmente obsoleta. Se rechazan symlinks y JSON inválido. Los cambios de formato al serializar JSON no alteran los valores ajenos. La ruta absoluta de galgo se fija al instalar; reinstalar si cambia. Revisar y confiar los hooks con `/hooks` si Codex lo solicita; el instalador nunca activa `--dangerously-bypass-hook-trust`.

Cada comando usa shell solo para una ruta de ejecutable citada y literales fijos; el JSON pasa por stdin directamente a `galgo hook codex`, sin interpolación. El wrapper devuelve `{}` a Codex incluso si galgo falla. **Riesgo de doble entrega con `galgo done`:** el daemon usa el flag `reported` para deduplicar un `Stop` posterior al `done` del mismo turno; un nuevo `UserPromptSubmit` reinicia ese flag. No garantiza deduplicación para casos no verificados como continuaciones de otro hook, por lo que conviene probar el flujo concreto antes de depender de ello.

## Orden y pruebas de aceptación propuestos

1. **Codex:** el e2e cubre instalación/desinstalación idempotente, preservación de hooks ajenos, `galgo done` seguido de `Stop` y un `exec` real contra un servidor simulado. Faltan confianza interactiva, permiso real, reanudación, fallo/cancelación, `Stop` continuado por otro hook y subagentes; no anunciar `blocked` ni `failed` hasta identificarlos.
2. **PI:** extensión aislada con `-e`, luego descubrimiento global/proyecto confiable; probar `agent_settled` con retries, compactación, abort y mensajes encolados; UI prompts anidados, resumen y errores, recarga y ausencia del daemon. Verificar que no se informe `turn_ended` prematuramente.
3. **OpenCode:** primero resolver identidad de sesión/servidor por panel y escoger API/versiones; probar TUI local, `run`, `attach` y servidor compartido, sesiones hijas, permisos/preguntas simultáneos, `session.error` con retry y duplicados `idle/status`; solo después habilitar resultados automáticos.

Para los tres: pruebas de mapeo con payloads capturados y anonimizados, sin stdout inesperado, timeout, resumen acotado, socket inexistente, permisos y preservación de configuración ajena. La instalación solo debe ser opt-in y explicar código ejecutable y riesgos. Las pruebas automatizadas PI cubren el adaptador y el instalador; los flujos reales de agente siguen sin verificarse.
