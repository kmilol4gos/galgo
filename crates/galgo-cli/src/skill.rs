//! Instalación local de la skill Claude, sin modificar otras skills.
use crate::claude::confirm;
use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use std::{io::{Read, Write}, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}};

const SKILL: &str = include_str!("skill-claude.md");
// Instantáneas exactas de las versiones publicadas. No modificar: agregar una copia nueva
// antes de cambiar el texto de la skill para reconocer instalaciones antiguas sin confiar
// solo en un marcador que también podría tener una edición del usuario.
const RELEASED_SKILLS: &[&str] = &[include_str!("skill-claude-v1.md")];
const BACKUP_FLAG: &str = "\n<!-- galgo-created-backup: 1 -->\n";
const MARKER: &str = "<!-- galgo-managed-skill: v1 -->";

#[derive(Clone, Copy, ValueEnum)]
pub enum Agent { Claude }
#[derive(Subcommand)]
pub enum SkillCmd {
    /// Instala la skill en $HOME/.claude/skills/galgo/SKILL.md.
    Install { agent: Agent, #[arg(long)] yes: bool },
    /// Retira la skill sin sobrescribir cambios posteriores del usuario.
    Uninstall { agent: Agent, #[arg(long)] yes: bool },
    /// Indica el estado de la skill.
    Status { agent: Agent },
}
fn skill_path(home: &Path) -> PathBuf { home.join(".claude/skills/galgo/SKILL.md") }
fn backup_path(path: &Path) -> PathBuf { path.with_file_name("SKILL.md.galgo-bak") }
fn home_path() -> Result<PathBuf> {
    Ok(skill_path(Path::new(&std::env::var_os("HOME").context("HOME no está definido")?)))
}
fn installed_from(skill: &str, with_backup: bool) -> Vec<u8> {
    let mut text = skill.as_bytes().to_vec();
    if with_backup { text.extend_from_slice(BACKUP_FLAG.as_bytes()); }
    text
}
fn installed(with_backup: bool) -> Vec<u8> { installed_from(SKILL, with_backup) }
// Solo reconocer contenidos completos conocidos protege las ediciones del usuario.
fn ownership_from(data: &[u8], current: &str) -> Option<bool> {
    std::iter::once(current).chain(RELEASED_SKILLS.iter().copied()).find_map(|skill| {
        if data == installed_from(skill, false) { Some(false) }
        else if data == installed_from(skill, true) { Some(true) }
        else { None }
    })
}
fn ownership(data: &[u8]) -> Option<bool> { ownership_from(data, SKILL) }
fn current(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => bail!("{} no es un archivo regular; no se modificó", path.display()),
        Ok(_) => Ok(Some(std::fs::read(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let tmp = path.with_file_name(format!(".SKILL.md.{}.{:016x}.tmp", std::process::id(), u64::from_ne_bytes(random)));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let result = (|| -> Result<()> { file.write_all(bytes)?; std::fs::rename(&tmp, path)?; Ok(()) })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result
}
fn install_with(path: &Path, skill: &str) -> Result<bool> {
    let existing = current(path)?;
    if let Some(has_backup) = existing.as_deref().and_then(|bytes| ownership_from(bytes, skill)) {
        let desired = installed_from(skill, has_backup);
        if existing.as_deref() == Some(desired.as_slice()) { return Ok(false); }
        // Una versión anterior gestionada se actualiza sin crear ni modificar su respaldo.
        write_atomic(path, &desired)?;
        return Ok(true);
    }
    std::fs::create_dir_all(path.parent().context("ruta sin directorio")?)?;
    let mut made_backup = false;
    if let Some(old) = &existing {
        let backup = backup_path(path);
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o7777;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&backup)
            .with_context(|| format!("no se pudo crear el respaldo {}; no se modificó la skill", backup.display()))?;
        let result = file.set_permissions(std::fs::Permissions::from_mode(mode)).and_then(|_| file.write_all(old));
        if let Err(e) = result { let _ = std::fs::remove_file(&backup); return Err(e.into()); }
        made_backup = true;
    }
    if let Err(e) = write_atomic(path, &installed_from(skill, made_backup)) {
        if made_backup { let _ = std::fs::remove_file(backup_path(path)); }
        return Err(e);
    }
    Ok(true)
}
fn install(path: &Path) -> Result<bool> { install_with(path, SKILL) }
fn uninstall_with(path: &Path, skill: &str) -> Result<bool> {
    let Some(bytes) = current(path)? else { return Ok(false) };
    match ownership_from(&bytes, skill) {
        Some(false) => std::fs::remove_file(path)?,
        Some(true) => {
            let backup = backup_path(path);
            match std::fs::symlink_metadata(&backup) {
                Ok(m) if m.file_type().is_file() => std::fs::rename(&backup, path)?,
                Ok(_) => bail!("el respaldo {} no es un archivo regular", backup.display()),
                Err(e) => return Err(e).with_context(|| format!("no se pudo restaurar {}; la skill no se modificó", backup.display())),
            }
        }
        None if bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()) => {
            bail!("{} fue modificada; se conservan la skill y cualquier respaldo", path.display());
        }
        None => return Ok(false),
    }
    Ok(true)
}
fn uninstall(path: &Path) -> Result<bool> { uninstall_with(path, SKILL) }
pub fn run(cmd: &SkillCmd) -> Result<()> {
    let path = home_path()?;
    match cmd {
        SkillCmd::Install { agent: Agent::Claude, yes } => {
            let existing = current(&path)?;
            let owned = existing.as_deref().and_then(ownership);
            if owned.is_some_and(|backup| existing.as_deref() == Some(installed(backup).as_slice())) {
                println!("Ya está instalada: {}", path.display()); return Ok(());
            }
            let note = if owned.is_some() { " (actualización de versión gestionada)" }
                else if existing.is_some() { " (se respaldará el archivo existente)" } else { "" };
            println!("Instalar skill Claude en {}{}", path.display(), note);
            if !yes && !confirm("¿Instalar la skill de galgo?")? { println!("Cancelado."); return Ok(()); }
            install(&path)?;
            println!("Skill instalada.");
        }
        SkillCmd::Uninstall { agent: Agent::Claude, yes } => {
            let existing = current(&path)?;
            if existing.is_none() { println!("No hay skill de galgo en {}", path.display()); return Ok(()); }
            if existing.as_deref().and_then(ownership).is_none() {
                if existing.as_deref().is_some_and(|s| s.windows(MARKER.len()).any(|w| w == MARKER.as_bytes())) {
                    bail!("{} fue modificada; se conservan la skill y cualquier respaldo", path.display());
                }
                println!("No hay skill gestionada de galgo en {}", path.display());
                return Ok(());
            }
            if !yes && !confirm("¿Desinstalar la skill de galgo?")? { println!("Cancelado."); return Ok(()); }
            uninstall(&path)?;
            println!("Skill retirada; se restauró el respaldo si correspondía.");
        }
        SkillCmd::Status { agent: Agent::Claude } => {
            let existing = current(&path)?;
            let state = match existing.as_deref() {
                Some(s) if ownership(s).is_some_and(|backup| s == installed(backup)) => "instalada",
                Some(s) if ownership(s).is_some() => "instalada; versión anterior",
                Some(s) if s.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()) => "modificada; no se desinstalará automáticamente",
                Some(_) => "archivo ajeno",
                None => "no instalada",
            };
            println!("Claude · {}: {state}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("galgo-skill-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }
    #[test]
    fn current_skill_has_a_frozen_release_snapshot() {
        // Obliga a archivar cada versión, antes de que una futura la sustituya.
        assert!(RELEASED_SKILLS.contains(&SKILL), "agrega una instantánea inmutable de esta versión a RELEASED_SKILLS");
    }
    #[test]
    fn backup_idempotence_restore_and_neighbors() {
        let home = home("backup"); let file = skill_path(&home);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"anterior\xff").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let other = home.join(".claude/skills/other/SKILL.md");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap(); std::fs::write(&other, "otra").unwrap();
        assert!(install(&file).unwrap()); assert!(!install(&file).unwrap());
        assert_eq!(std::fs::read(backup_path(&file)).unwrap(), b"anterior\xff");
        assert!(uninstall(&file).unwrap()); assert!(!uninstall(&file).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"anterior\xff");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_to_string(other).unwrap(), "otra");
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn edited_skill_and_stale_backup_are_not_overwritten() {
        let home = home("edited"); let file = skill_path(&home);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "original").unwrap(); install(&file).unwrap();
        let mut edited = std::fs::read(&file).unwrap(); edited.extend_from_slice(b"edit"); std::fs::write(&file, &edited).unwrap();
        assert!(uninstall(&file).is_err()); assert!(install(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), edited); assert_eq!(std::fs::read(backup_path(&file)).unwrap(), b"original");
        std::fs::remove_file(&file).unwrap(); install(&file).unwrap();
        assert!(uninstall(&file).unwrap()); assert_eq!(std::fs::read(backup_path(&file)).unwrap(), b"original");
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn published_version_upgrades_and_uninstalls_without_restoring_old_skill() {
        let home = home("upgrade");
        let file = skill_path(&home);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let old = RELEASED_SKILLS[0];
        let new = format!("{old}\nNueva versión de la skill.\n");
        // Una instalación anterior sin respaldo debe poder actualizarse sin crearlo.
        std::fs::write(&file, installed_from(old, false)).unwrap();
        assert_eq!(ownership_from(&std::fs::read(&file).unwrap(), &new), Some(false));
        assert!(install_with(&file, &new).unwrap());
        assert!(!install_with(&file, &new).unwrap());
        assert!(!backup_path(&file).exists());
        assert!(uninstall_with(&file, &new).unwrap());
        assert!(!file.exists());
        // Una instalación anterior con respaldo conserva y restaura el archivo original.
        std::fs::write(&file, installed_from(old, true)).unwrap();
        std::fs::write(backup_path(&file), "archivo ajeno").unwrap();
        assert_eq!(ownership_from(&std::fs::read(&file).unwrap(), &new), Some(true));
        assert!(install_with(&file, &new).unwrap());
        assert_eq!(std::fs::read(backup_path(&file)).unwrap(), b"archivo ajeno");
        assert!(uninstall_with(&file, &new).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"archivo ajeno");
        // También se puede desinstalar directamente una versión vieja desde un binario nuevo.
        std::fs::write(&file, installed_from(old, false)).unwrap();
        assert!(uninstall_with(&file, &new).unwrap());
        // Una edición, incluso un solo salto de línea, nunca se reconoce como versión publicada.
        let mut modified = installed_from(old, true);
        modified.push(b'\n');
        std::fs::write(&file, &modified).unwrap();
        std::fs::write(backup_path(&file), "intacto").unwrap();
        assert_eq!(ownership_from(&modified, &new), None);
        assert!(uninstall_with(&file, &new).is_err());
        assert!(install_with(&file, &new).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), modified);
        assert_eq!(std::fs::read(backup_path(&file)).unwrap(), b"intacto");
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn missing_backup_and_symlinks_are_safe() {
        let home = home("symlink"); let file = skill_path(&home);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "original").unwrap(); install(&file).unwrap();
        std::fs::remove_file(backup_path(&file)).unwrap();
        assert!(uninstall(&file).is_err()); assert_eq!(std::fs::read(&file).unwrap(), installed(true));
        std::fs::remove_file(&file).unwrap(); let other = home.join("other"); std::fs::write(&other, "safe").unwrap(); symlink(&other, &file).unwrap();
        assert!(install(&file).is_err()); assert!(uninstall(&file).is_err());
        assert_eq!(std::fs::read(other).unwrap(), b"safe");
        std::fs::remove_dir_all(home).unwrap();
    }
}
