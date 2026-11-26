# Referencia de diseño: Rex (capturas del usuario) → galgo v2

Capturas: /tmp/galgo-ux/rex-main.png (ventana principal) y rex-settings.png (ajustes). El usuario dice: "el diseño es perfecto, la idea es algo así: un terminal que además se integre muy bien con trabajo de múltiples IAs (distintos proveedores, o el mismo)".

## Lo que hace Rex (observado)
1. **Barra superior estilo pestañas de macOS** con una pestaña por *sesión* (icono + título de la sesión, p.ej. "Limpieza de containers Docker", "galgo · 1"); botón de sesiones a la izquierda, a la derecha: vista, paleta de atajos (⌘) y "+". Es una sola ventana con barra de título integrada.
2. **Sidebar izquierdo en dos bloques con títulos en minúsculas**:
   - `machines`: árbol "Local" (y hosts remotos) > proyectos, cada uno con punto de estado y su rama (main, feat/variant…). Pie: `new · Local   menu`.
   - `agents  priority`: lista ordenada por prioridad de TODOS los agentes de todos los proyectos. Cada fila: punto de color (rojo bloqueado/error, azul trabajando, amarillo, verde/hueco idle), nombre del agente (claude, sii-manager…), debajo el **título de la tarea** que el agente mismo pone ("bug de ventas", "error margen"), y a la derecha un contador o el proyecto. Sin recuadros ni botones: texto plano, tipografía monoespaciada.
3. **Pestañas internas del proyecto** como rectángulos planos: "1" (activa, relleno de acento), "nexo-planner", "nexo-programmer", "nexo-reviewer", "+"; reloj a la derecha (17:11). Iconos de split vertical/horizontal/zoom/cerrar arriba a la derecha, tenues.
4. **El terminal ocupa todo** el resto; sin cabeceras extra ni botones grandes.
5. **Ajustes** (⌘,) en ventana aparte, estilo macOS: buscador, y secciones Server (running), General, Remote Hosts, Interface, Themes, Terminal, Shortcuts, Integrations, Notifications, Developer. General: canal de actualización, etiqueta del cliente, control remoto, icono.
6. **Tema cálido**: fondo marrón muy oscuro (no negro azulado), texto crema, acentos ámbar/azul. Esquinas redondeadas grandes en la ventana, fuente monoespaciada en TODA la UI.

## Qué cambiar en galgo (prioridad)
1. **Sidebar = `machines` + `agents`** como Rex: el árbol Local > proyecto(rama) y la lista de agentes por prioridad (bloqueado > trabajando > done sin leer > idle) con **título de la tarea** (leer del título OSC / hook / primer prompt) y proveedor/modelo en gris (claude·opus, codex·gpt, pi·gpt…). Reemplaza el sidebar actual jerárquico de paneles.
2. **Barra superior de sesiones** (una por proyecto/espacio de trabajo) integrada en la barra de título (titlebar transparente) y pestañas internas planas como las de Rex (numeradas o con el nombre del agente), reloj a la derecha.
3. **Fuente monoespaciada en toda la UI**, tema cálido (módulo theme.rs): fondo ~#1b1713, superficie ~#241f19, texto crema, acento azul para activo, ámbar para atención, rojo para bloqueado/error.
4. **Quitar** los botones "+ Nuevo workspace/+ Nuevo panel/Cerrar panel" grandes; esas acciones van a "+" de la barra, al menú `new · Local` y a la paleta.
5. **Ventana de ajustes** (Cmd+,) con las secciones: General, Interface, Themes, Terminal, Shortcuts, Integrations (hooks Claude/Codex/PI/opencode, statusline: instalar/estado), Notifications, Usage (cuotas por proveedor), Developer (socket, versión del daemon).
6. **Multi-IA como ciudadano de primera**: al lanzar un agente elegir proveedor + modelo; el sidebar agrupa o filtra por proveedor; vista de uso/cuota por proveedor y por agente (ya existe galgo-usage); poder lanzar N agentes del mismo proveedor en el mismo proyecto con nombres automáticos (claude, claude-2…).
