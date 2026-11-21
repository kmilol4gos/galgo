---
name: galgo
description: Coordina agentes que corren en paneles de galgo. Úsalo cuando tengas que delegar trabajo a otro panel, esperar su resultado, reportar el tuyo a un manager o leer lo que hay en otro panel. Solo aplica si la variable GALGO_PANE está definida.
---

# galgo

Comprueba que estás dentro de galgo: `test -n "$GALGO_PANE"`. Si no, no uses estos comandos.

Cada panel tiene un id y un nombre. Tu panel es `$GALGO_PANE`. Un worker reporta a un manager.

## Si eres el manager

- Ver el estado de todos: `galgo ls`.
- Dar trabajo a un worker: `galgo send <worker> "<tarea completa y autocontenida>"`.
- **Esperar su resultado: `galgo wait <worker> --timeout 600`.** Vuelve apenas el worker termina, pide una decisión humana o muere. No leas su pantalla en bucle para saber si terminó.
- Si `galgo ls` marca `ENTREGA SIN CONFIRMAR`, tienes un resultado pendiente: `galgo inbox` y luego `galgo ack <id>`.
- Si hace falta ver qué hace un worker: `galgo read <worker>`.

Un `wait` que devuelve `necesita una decisión` significa que el worker espera a una persona. Avísale al usuario; no respondas por él.

## Si eres un worker

- Haz la tarea que te mandaron. Cuando termines, tu último mensaje debe ser un resumen corto de lo hecho y lo que quedó sin probar: es lo que recibe el manager.
- Si usas Claude Code con los hooks de galgo, el resumen se entrega solo al terminar el turno. Si no (por ejemplo PI u opencode), repórtalo tú:
  `galgo done --summary "qué hiciste, qué verificaste, qué falta"`
  y si fallaste: `galgo done --failed --summary "por qué"`.
- No reportes dos veces el mismo resultado.

## Reglas

- Las tareas que envías deben bastarse solas: el otro agente no ve tu conversación.
- No escribas en el panel de una persona (`galgo send`) sin que te lo hayan pedido.
- Los secretos (`.env`, claves) no se envían por `galgo send` ni se pegan en resúmenes.
