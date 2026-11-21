---
name: galgo
description: Coordina workers de galgo como manager, espera resultados sin sondeo y gestiona bloqueos y variables sin exponer secretos.
---

<!-- galgo-managed-skill: v1 -->
# Coordinar trabajo con galgo

Usa galgo cuando haya trabajo independiente que se pueda delegar a otro agente: delimita tarea, archivos y resultado esperado, evita asignar cambios simultáneos sobre los mismos archivos y conserva la integración y la revisión final en el manager. Para una tarea pequeña o estrictamente secuencial trabaja directamente. No crees workers por rutina.

## Crear y coordinar workers

Trabaja dentro de un panel galgo con `GALGO_PANE` definido. Para un worker Claude, por ejemplo:

```sh
galgo spawn worker-a --manager "$GALGO_PANE" -- claude
galgo send worker-a 'Investiga la tarea asignada e informa hallazgos y pruebas.'
```

El MCP, si está disponible, ofrece `galgo_spawn` con `name`, `cmd` (array de textos), `manager` y opcionalmente `cwd`; `galgo_send` recibe `pane`, `text` y opcionalmente `enter`. Identifica el panel manager mediante `GALGO_PANE`. `galgo_list` o `galgo ls` muestran estado; `galgo_read` con `pane` o `galgo read <panel>` permite inspeccionar una pantalla si hay un bloqueo. No pases secretos en comandos, mensajes ni resúmenes.

## Esperar y procesar resultados

Lanza **un solo wait activo por worker y turno**; no hagas bucles de sondeo con list/read ni dupliques esperas concurrentes. Con MCP usa `galgo_wait` con `target` y, si hace falta, `timeout_secs` (por defecto 600). Alternativamente ejecuta:

```sh
galgo --json wait worker-a --timeout 600
```

`galgo wait` usa `GALGO_PANE` como panel que espera; fuera de un panel debes proporcionar `--pane`. La espera termina con resultado, salida del worker o timeout. Un timeout no significa éxito: decide si seguir esperando o resolver el bloqueo; no vuelvas a spawnear el worker automáticamente. Si hay varios workers independientes, espera a cada uno una vez sin esperar repetidamente al mismo; aprovecha la ejecución paralela si el cliente la permite.

Deduplica las entregas por `msg_id` cuando el resultado de `wait` lo incluya. El `wait` de CLI y el servidor MCP confirman la entrega después de escribir correctamente su respuesta; si la respuesta no llega, puede reaparecer. Para recuperar pendientes usa `galgo_inbox` / `galgo inbox`; cada mensaje del inbox tiene `id`. Procesa cada id una sola vez y confirma con `galgo_ack` pasando `msg`, o con `galgo ack <id>`, después de procesarlo. No confundas la entrega con el éxito: revisa estado, resumen y cambios antes de integrar.

## Bloqueos, preguntas y secretos

Si un worker está `blocked`, revisa su mensaje pendiente o pantalla y determina qué necesita. Si requiere una decisión o permiso, pregunta al humano con el contexto mínimo y transmite la respuesta al worker mediante `galgo_send` / `galgo send`; no apruebes acciones sensibles por tu cuenta. Si falta una variable, el worker puede ejecutar:

```sh
galgo env request API_KEY --reason 'necesaria para la prueba' --dir .
```

El comando avisa al manager y espera a que exista la variable en el archivo `.env`; si vence el timeout, el worker puede repetir la solicitud. Pide al humano que, en su propio terminal, introduzca el valor sin eco mediante `galgo env set API_KEY --dir .`. Para otro archivo usa la misma opción `--file` en ambos comandos. `galgo env list --dir .` muestra nombres, no valores; verifica los avisos de `.gitignore`. No solicites ni leas el valor en el chat, logs o pantalla. Nunca lo pongas en argv, `galgo send`, resúmenes, comandos con valores literales, código ni commits; no lo muestres. No existe una herramienta MCP `galgo_env`: usa la CLI para estas operaciones.

## Cerrar el turno

Indica al worker que, cuando termine, reporte un resumen sin secretos con `galgo done --summary 'resultado y pruebas'`; usa `--failed` si no pudo completar la tarea. Desde MCP puede usar `galgo_done` con `summary` y opcionalmente `failed`. El panel actual debe tener `GALGO_PANE` o especificarse con `--pane` en la CLI. Si tú también reportas a un manager, aplica la misma regla. Revisa resultados y comunica claramente fallos y trabajo pendiente.
