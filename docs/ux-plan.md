# Plan de UX de la GUI de galgo (v1)

Referencias estudiadas: Orca, cmux, herdr (TUI y app), Rex. Problema actual: la GUI es un spike con un formulario; crear workspaces/ventanas/paneles es torpe. Objetivo: que un programador lo use todo el día sin leer documentación.

## Modelo (un solo vocabulario)
Proyecto (repo) > Workspace (rama/worktree o tarea) > Pestañas > Paneles (splits).
Hoy: panes planos con etiqueta `workspace`. Se pasa a: sidebar jerárquico Proyecto > Workspace > agentes, y dentro de cada workspace pestañas con splits.

## Prioridades (en orden)
1. **Arranque sin fricción**: al abrir sin paneles, pantalla de bienvenida con 3 acciones grandes: "Abrir carpeta/proyecto…" (selector nativo), "Nuevo terminal aquí", "Reabrir lo último". Al abrir un proyecto se crea workspace + un shell, no un formulario.
2. **Cmd+N nuevo workspace** (selector de carpeta + lista de recientes con búsqueda), **Cmd+T nueva pestaña**, **Cmd+D split derecho**, **Cmd+Shift+D split abajo**, **Cmd+W cerrar con confirmación si hay agente trabajando**, **Cmd+1..9** workspace, **Cmd+[ / ]** pestaña, **Opt+Cmd+flechas** foco entre paneles, **Cmd+Shift+U** saltar al siguiente que necesita atención.
3. **Paleta de comandos Cmd+K / Cmd+Shift+P**: buscar workspace, pestaña, acción ("Nuevo agente Claude aquí", "Nuevo worktree…", "Instalar hooks"). Fuzzy. Es la forma más barata de cubrir todo.
4. **Lanzador de agentes**: botón/atajo que abre un menú corto: Claude / Codex / PI / opencode / Shell, en el workspace actual, con cwd heredado. Sin pedir nombre (autogenerado, editable después con doble clic).
5. **Worktree en un paso** (como Orca/herdr): clic derecho en un proyecto > "Nuevo worktree…" pide solo el nombre de la rama, crea `git worktree`, abre workspace y ofrece lanzar un agente.
6. **Sidebar**: filas con icono de estado (working/blocked/done/idle), nombre, rama, y contador de "necesitan atención" arriba (sección "Te necesitan"). Atención sube de pane a pestaña a workspace. Clic derecho: renombrar, cerrar, mover. Arrastrar para reordenar (v2).
7. **Splits y pestañas reales** con barra de pestañas por workspace, divisores arrastrables, zoom de panel (Cmd+Shift+Enter).
8. **Persistir layout**: al reabrir, restaurar workspaces/pestañas/splits (los agentes siguen vivos porque el daemon los posee).
9. **Notificación nativa de macOS** cuando un agente termina o se bloquea y la ventana no está enfocada; clic enfoca ese panel.
10. **Ajustes** (Cmd+,): fuente/tamaño, tema, atajos, hooks (instalar/estado), statusline.

## Criterios de aceptación
- Desde ventana vacía hasta "Claude trabajando en mi repo" en <= 3 interacciones (Cmd+N, elegir carpeta, elegir agente).
- Ninguna acción común requiere escribir un nombre o un cwd a mano.
- Todo lo del menú tiene atajo y aparece en la paleta.
- El workspace actual, rama y estado visibles sin abrir nada.

## Fuera de alcance por ahora
Navegador embebido, revisión de diffs, remoto/SSH, móvil, plugins.
