# Iteración de UX 1: terminal que den ganas de usar

El usuario autorizó iterar hasta que quede bien y competitiva. Dijo además: «elegir carpeta y todo eso no me gusta para nada, esto no es un IDE».
Hallazgos probando el binario instalado con un daemon aislado (capturas /tmp/galgo-ux/ux-1.png, ux-2.png, ux-3.png).

## P0
1. **Fuera los selectores de carpeta.** Sin diálogo de carpeta, sin «recientes», sin «Abrir carpeta», sin formulario «Nuevo panel». Un terminal o workspace nuevo HEREDA el cwd del terminal enfocado; el usuario navega con `cd`. Cmd+Shift+N agrupa desde el cwd actual sin preguntar nada (el nombre sale de la carpeta y se puede renombrar). «+ Agente…» usa ese cwd.
2. **cwd vivo.** Tras `cd ~/Development/nexo`, la píldora, la cabecera y `galgo ls` siguen diciendo `~`. Leer el cwd real del proceso en primer plano (macOS: proc_pidinfo) y OSC 7; que lo hereden Cmd+T / Cmd+D / «+».
3. **Una sola fila de pestañas.** Con 3 terminales aparecen dos filas (sesiones «Development» y «~» agrupadas por carpeta + pestañas), el texto «galgo» y DOS botones ⌘ (ux-3.png). Una fila plana de píldoras en la barra de título; solo se agrupa si el usuario creó un workspace.
4. **Dibujo de cajas y bloques procedural** (U+2500–U+259F): hoy las líneas quedan cortadas entre filas y celdas (ux-2.png). Se nota en Claude Code, vim, htop, lazygit.

## P1
5. Títulos OSC 0/2 ignorados; la píldora debe mostrar el título del programa, o proceso en primer plano + carpeta corta (`vim · nexo`, `claude · nexo`), no siempre `zsh`.
6. Tachado (SGR 9) no se pinta. Revisar también dim, blink, reverse, subrayado ondulado/color.
7. El entorno del daemon filtra el terminal que lo lanzó (TERM_PROGRAM=herdr, TERM_PROGRAM_VERSION, TERMINFO ajeno, CLAUDE_EFFORT, HERDR_*). Fijar TERM_PROGRAM=galgo y su versión; quitar variables de otros terminales y de sesiones de agentes.
8. Con un solo panel la cabecera repite la píldora y muestra el nombre interno («ux», «shell»): ocultarla (los controles de split van a la barra superior o aparecen al pasar el puntero); con splits, mostrar proceso + carpeta.
9. El icono de maximizar se ve como líneas apiladas (≣): usar uno claro.

## P2
10. Cmd+F en el scrollback; restaurar pestañas y splits; latencia de tecla medida en la GUI; scroll suave con inercia; indicador de scroll.
