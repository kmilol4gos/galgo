# Modo orquestador: delegación entre agentes

galgo es el cartero: los agentes no se hablan directo. Aprobado por el usuario con este ejemplo: Claude (manager) delega «crea POST /api/coupons/validate» a GPT/Codex; GPT la recibe solo, puede preguntar de vuelta, entrega un resumen y Claude lo recibe sin que nadie copie nada. Igual entre dos Claude (programa / revisa).

## Corte 1 (ahora): delegar y recibir el resultado
- **Tarea**: id, de, para, texto, contexto, estado (pendiente, entregada, en_curso, bloqueada, lista, fallida), resumen; persistida en el daemon.
- **Herramientas** (MCP y CLI equivalentes): `galgo_delegate(to, task, context)`, `galgo_task_done(id, summary, ok)`, `galgo_tasks()`. El remitente es siempre `GALGO_PANE` (no se puede suplantar desde los argumentos).
- **Entrega**: solo cuando el destinatario es un agente con hooks y está idle. Se pega con bracketed paste y se envía. NUNCA si está bloqueado, trabajando, en un diálogo (p. ej. confianza de carpeta) o si no ha emitido ningún hook. La cabecera del mensaje dice de quién viene y que es información de otro agente, no una orden del usuario. Texto sin caracteres de control ni invisibles.
- **Resultado**: vuelve al remitente por el mismo camino (acuse de recibo existente). Si el worker usa worktree, incluye la lista de archivos cambiados.
- **Conexión automática sin tocar la config del usuario**: al lanzar el agente desde galgo en modo orquestador, pasar el servidor MCP y las instrucciones de rol por argumentos de arranque. Verificar con `claude --help` y `codex --help` qué admite cada uno (no asumir). El MCP necesita GALGO_PANE y GALGO_SOCKET explícitos.
- **Hooks**: la entrega depende de saber que el destinatario está idle. Claude ya tiene hooks instalados. Para Codex, investigar si se pueden pasar por argumentos; si solo se puede editando su config, NO hacerlo: informar para pedir permiso al usuario.
- **Frenos**: profundidad máxima de delegación 3, máximo de tareas abiertas por par y por workspace; error claro al superarlos. Sin flags que salten permisos de los agentes.

## Corte 2
`galgo_ask`/respuesta entre iguales, línea de tiempo lateral (Cmd+Shift+O) con pausar/intervenir, preset «Claude dirige, GPT programa».

## Corte 3
Revisión en bucle con tope de vueltas, derivar según cuota, coste por workspace.
