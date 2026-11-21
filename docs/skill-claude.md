# Skill de galgo para Claude Code

La skill embebida en `crates/galgo-cli/src/skill-claude.md` orienta al manager para delegar workers, esperar sin sondear, procesar entregas, atender bloqueos y gestionar variables sin exponer valores. No instala ni configura el servidor MCP o los hooks: las herramientas MCP solo están disponibles si el servidor ya está configurado y el cliente las ofrece. Para confirmar los comandos disponibles, consultar `galgo --help`.

```sh
galgo skill install claude --yes
galgo skill status claude
galgo skill uninstall claude --yes
```

Sin `--yes`, instalar y desinstalar piden confirmación interactiva. La ruta es `$HOME/.claude/skills/galgo/SKILL.md`; no se modifican otras skills ni settings de Claude. Un archivo anterior se respalda en el mismo directorio como `SKILL.md.galgo-bak`, preservando sus bytes y permisos. Si ese respaldo ya existe, la instalación de un archivo ajeno falla sin sobrescribir ninguno. Instalar y desinstalar son idempotentes.

La copia instalada indica si creó un respaldo. La desinstalación compara **todo el contenido** con la versión embebida en el binario: solo si coincide restaura su propio respaldo o elimina la skill que instaló sin respaldo. Si el usuario la editó, no modifica ni el archivo ni el respaldo; el usuario debe decidir cómo recuperar o combinar los cambios. Si falta el respaldo que corresponde, falla sin borrar la skill. Un respaldo preexistente que no pertenece a esta instalación nunca se restaura. Se rechazan symlinks en el archivo de skill y en el respaldo. Si se actualiza el binario y cambia la skill embebida, una copia anterior ya no coincide: conservar y revisar manualmente los archivos antes de reinstalar o desinstalar.
