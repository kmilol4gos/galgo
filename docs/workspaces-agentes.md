# Superpoderes: workspaces con varias IAs trabajando juntas

Principio: el terminal normal es el default ([[principio-terminal-primero]]). Un *workspace* es una capa opcional que se activa cuando el usuario la pide.

## Modelo
- **Workspace** = carpeta/repo + grupo de paneles + (opcional) un equipo de agentes. Se crea con Cmd+Shift+N / paleta «Nuevo workspace…» (elige carpeta; recientes). Aparece como grupo en la barra superior (píldora con el nombre del proyecto y sus terminales dentro), persistente.
- **Agente** = panel con rol + proveedor/modelo + aislamiento. Proveedores: Claude, Codex/GPT, PI, opencode. Se añade con «+ Agente…» en el workspace: proveedor·modelo, rol (manager | worker | reviewer | libre), aislamiento (misma carpeta | worktree propio `galgo spawn --worktree`).
- **Equipo** = plantilla versionable `.galgo/team.toml` en el repo (con confianza al abrir, como cmux): lista de agentes (nombre, proveedor, modelo, rol, aislamiento, prompt inicial) y layout. Un comando/atajo «Levantar equipo» los crea en splits (manager grande a la izquierda, workers apilados a la derecha). También presets integrados (p. ej. «Claude manager + Codex worker + Claude reviewer»).

## Cómo colaboran (mucho ya existe en el daemon/MCP)
- El manager recibe avisos fiables (ack) cuando un worker termina o se bloquea: ya hecho.
- Herramientas MCP `galgo_spawn/send/read/...` para que el manager delegue: ya hecho. Worktree por worker y `galgo worktree diff|accept|discard`: ya hecho.
- Falta: **pizarra compartida** (`.galgo/board.md` + herramienta MCP `galgo_board`) para tareas y notas que todos leen/escriben; **bandeja «Te necesitan»** por workspace en la GUI (bloqueados y resultados sin leer, salto con Cmd+Shift+U, aprobar/denegar desde el panel del manager); **resumen de resultado** del worker adjunto en el mensaje al manager; **uso y coste agregados por workspace y proveedor**.

## Fases
1. Workspace nativo en la GUI: crear/listar/cerrar, píldora de grupo, persistir layout (también restaura splits y pestañas al reiniciar).
2. «+ Agente…» con proveedor·modelo·rol·aislamiento, y `team.toml` + «Levantar equipo» + presets.
3. Vista de equipo: layout automático, bandeja «Te necesitan», diff/accept de worktree desde la GUI.
4. Pizarra compartida y resumen de resultados (MCP + CLI).
5. Uso/coste por workspace.
