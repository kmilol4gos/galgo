# Skill de galgo para Claude Code

La skill embebida en `crates/galgo-cli/src/skill-claude.md` orienta al manager para delegar workers, esperar sin sondear, procesar entregas, atender bloqueos, gestionar variables sin exponer valores y consultar uso y cuotas sin confundir estimaciones con facturas. No instala ni configura el servidor MCP o los hooks: las herramientas MCP solo están disponibles si el servidor ya está configurado y el cliente las ofrece. Para confirmar los comandos disponibles, consultar `galgo --help`.

```sh
galgo skill install claude --yes
galgo skill status claude
galgo skill uninstall claude --yes
```

Sin `--yes`, instalar y desinstalar piden confirmación interactiva. La ruta es `$HOME/.claude/skills/galgo/SKILL.md`; no se modifican otras skills ni settings de Claude. Un archivo anterior se respalda en el mismo directorio como `SKILL.md.galgo-bak`, preservando sus bytes y permisos. Si ese respaldo ya existe, la instalación de un archivo ajeno falla sin sobrescribir ninguno. Instalar y desinstalar son idempotentes.

La copia instalada indica si creó un respaldo. Se compara **todo el contenido** con la versión actual y con instantáneas inmutables de versiones publicadas. Una versión reconocida anterior se actualiza sin crear otro respaldo, y se puede desinstalar directamente; si tenía un respaldo, se conserva y restaura el archivo original. Si el usuario la editó, no se desinstala automáticamente; instalar puede respaldar esa edición si no existe ya un respaldo. Si falta el respaldo que corresponde, falla sin borrar la skill. Un respaldo preexistente que no pertenece a esta instalación nunca se restaura. Se rechazan symlinks en el archivo de skill y en el respaldo.

Para mantener actualizaciones seguras entre releases, cada cambio de `skill-claude.md` debe incluir una copia exacta de la nueva versión como instantánea inmutable agregada a `RELEASED_SKILLS` en `skill.rs`; conservar también las instantáneas anteriores. Una prueba exige que la versión actual esté registrada. Las versiones que no estén en esa lista no se consideran propias solo por tener el marcador.
