//! Hooks de Codex: solo eventos verificados, configuración compartida conservada.
use crate::claude::{confirm, Scope};
use anyhow::{bail, Context, Result};
use galgo_proto::HookEvent;
use serde_json::{json, Value};
use std::{io::{Read, Write}, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}};

const EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop"];
const MARKER: &str = " # galgo-hook-codex";

pub fn map_event(payload: &Value) -> Option<(HookEvent, Option<String>)> {
    match payload.get("hook_event_name")?.as_str()? {
        "SessionStart" => Some((HookEvent::Idle, None)),
        "UserPromptSubmit" => Some((HookEvent::PromptSubmitted, None)),
        "Stop" => {
            let summary = payload.get("last_assistant_message").and_then(Value::as_str)
                .map(str::trim).filter(|s| !s.is_empty()).map(|s| {
                    if s.chars().count() > 600 { format!("{}…", s.chars().take(600).collect::<String>()) } else { s.to_owned() }
                });
            Some((HookEvent::TurnEnded, summary))
        }
        _ => None,
    }
}

fn path(scope: Scope) -> Result<PathBuf> {
    if !matches!(scope, Scope::User) { bail!("Codex solo admite --scope user"); }
    let base = match std::env::var_os("CODEX_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME no está definido")?).join(".codex"),
    };
    Ok(base.join("hooks.json"))
}

// La shell recibe únicamente la ruta del ejecutable, no texto del agente. El JSON
// de stdin llega directamente a galgo; Codex recibe siempre un objeto vacío.
fn command(exe: &Path) -> Result<String> {
    let exe = exe.to_str().context("ruta del ejecutable no es UTF-8")?.replace('\'', "'\\''");
    Ok(format!("'{exe}' hook codex 2>/dev/null; printf '{{}}'{MARKER}"))
}
fn is_ours(hook: &Value) -> bool {
    hook.get("command").and_then(Value::as_str).is_some_and(|s| s.ends_with(MARKER))
}
fn remove(root: &mut Value) -> usize {
    let mut count = 0;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else { return 0 };
    hooks.retain(|_, value| {
        let Some(groups) = value.as_array_mut() else { return true };
        let mut changed = false;
        groups.retain_mut(|group| {
            let Some(items) = group.get_mut("hooks").and_then(Value::as_array_mut) else { return true };
            let n = items.len();
            items.retain(|item| !is_ours(item));
            let removed = n - items.len();
            count += removed;
            changed |= removed > 0;
            removed == 0 || !items.is_empty()
        });
        !changed || !groups.is_empty()
    });
    if count > 0 && hooks.is_empty() { root.as_object_mut().unwrap().remove("hooks"); }
    count
}
fn merge(root: &mut Value, cmd: &str) -> Result<bool> {
    // Validar toda la estructura antes de modificarla.
    let object = root.as_object().context("hooks.json no es un objeto")?;
    if let Some(hooks) = object.get("hooks") {
        let hooks = hooks.as_object().context("hooks no es un objeto")?;
        for event in EVENTS {
            if let Some(value) = hooks.get(*event) {
                if !value.is_array() { bail!("hooks.{event} no es una lista"); }
            }
        }
    }
    let mut desired = root.clone();
    let mut found = 0;
    let mut matches = 0;
    if let Some(hooks) = root.get("hooks").and_then(Value::as_object) {
        for (event, groups) in hooks {
            if let Some(groups) = groups.as_array() {
                for group in groups {
                    if let Some(items) = group.get("hooks").and_then(Value::as_array) {
                        for item in items.iter().filter(|h| is_ours(h)) {
                            found += 1;
                            if EVENTS.contains(&event.as_str()) && item == &json!({"type":"command", "command":cmd, "timeout":3}) { matches += 1; }
                        }
                    }
                }
            }
        }
    }
    // No cambia la posición de los hooks ni el archivo si ya está instalado.
    if found == EVENTS.len() && matches == EVENTS.len() && EVENTS.iter().all(|event| {
        root["hooks"][*event].as_array().is_some_and(|groups| groups.iter().any(|group| group.get("hooks").and_then(Value::as_array).is_some_and(|items| items.iter().any(is_ours))))
    }) { return Ok(false); }
    remove(&mut desired);
    let hooks = desired.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({})).as_object_mut().unwrap();
    for event in EVENTS {
        hooks.entry(*event).or_insert_with(|| json!([])).as_array_mut().unwrap()
            .push(json!({"hooks":[{"type":"command", "command":cmd, "timeout":3}]}));
    }
    *root = desired;
    Ok(true)
}
fn read(path: &Path) -> Result<Value> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => bail!("{} no es un archivo regular", path.display()),
        Ok(_) => {
            let bytes = std::fs::read(path)?;
            serde_json::from_slice(&bytes).with_context(|| format!("{} no es JSON válido; no se modificó", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}
fn write(path: &Path, value: &Value, make_backup: bool) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("ruta sin directorio")?)?;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => Some(m),
        Ok(_) => bail!("{} no es un archivo regular", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mode = meta.as_ref().map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    if make_backup && meta.is_some() {
        let backup = path.with_file_name("hooks.json.galgo-bak");
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&backup) {
            Ok(mut file) => {
                let result = (|| -> Result<()> { file.set_permissions(std::fs::Permissions::from_mode(mode))?; std::io::copy(&mut std::fs::File::open(path)?, &mut file)?; Ok(()) })();
                if let Err(e) = result { let _ = std::fs::remove_file(&backup); return Err(e); }
                println!("respaldo: {}", backup.display());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !std::fs::symlink_metadata(&backup)?.file_type().is_file() { bail!("respaldo no es un archivo regular: {}", backup.display()); }
                println!("Se conserva el respaldo existente: {}", backup.display());
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let tmp = path.with_file_name(format!(".hooks.json.galgo-{}-{:016x}.tmp", std::process::id(), u64::from_ne_bytes(random)));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    let result = (|| -> Result<()> {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(tmp); }
    result
}
pub fn install_command(scope: Scope, yes: bool, dry_run: bool) -> Result<()> {
    let path = path(scope)?;
    let mut settings = read(&path)?;
    let cmd = command(&std::env::current_exe()?.canonicalize()?)?;
    if !merge(&mut settings, &cmd)? { println!("Codex · {}: ya está instalado.", path.display()); return Ok(()); }
    println!("Codex · {}: agregar SessionStart, UserPromptSubmit y Stop; se conservan los hooks existentes.", path.display());
    if dry_run { println!("(simulación: no se escribió nada)"); return Ok(()); }
    if !yes && !confirm("¿Instalar los hooks de galgo para Codex?")? { println!("Cancelado."); return Ok(()); }
    write(&path, &settings, true)?;
    println!("Instalado. Revisa y aprueba los hooks con /hooks en Codex.");
    Ok(())
}
pub fn uninstall_command(scope: Scope, yes: bool) -> Result<()> {
    let path = path(scope)?;
    let mut settings = read(&path)?;
    let count = remove(&mut settings);
    if count == 0 { println!("No hay hooks de galgo en {}", path.display()); return Ok(()); }
    if !yes && !confirm("¿Quitar los hooks de galgo para Codex?")? { println!("Cancelado."); return Ok(()); }
    write(&path, &settings, false)?;
    println!("Retirados {count} hook(s) de galgo; se conservan las demás entradas y el respaldo.");
    Ok(())
}
pub fn status_command(scope: Scope) -> Result<()> {
    let path = path(scope)?;
    let settings = read(&path)?;
    println!("Codex · {}", path.display());
    for event in EVENTS {
        let present = settings["hooks"][*event].as_array().is_some_and(|groups| groups.iter().any(|g| g.get("hooks").and_then(Value::as_array).is_some_and(|a| a.iter().any(is_ours))));
        println!("  {} {event}", if present { "✓" } else { "·" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_mapping() {
        assert_eq!(map_event(&json!({"hook_event_name":"SessionStart"})), Some((HookEvent::Idle, None)));
        assert_eq!(map_event(&json!({"hook_event_name":"UserPromptSubmit"})), Some((HookEvent::PromptSubmitted, None)));
        assert_eq!(map_event(&json!({"hook_event_name":"Stop","last_assistant_message":"  listo  "})), Some((HookEvent::TurnEnded, Some("listo".into()))));
        assert_eq!(map_event(&json!({"hook_event_name":"Stop","last_assistant_message":" ".to_owned()+&"á".repeat(601)})).unwrap().1.unwrap().chars().count(), 601);
        for name in ["PermissionRequest", "PostToolUse", "SubagentStop", "StopFailure"] { assert_eq!(map_event(&json!({"hook_event_name":name})), None); }
    }
    #[test]
    fn merge_and_remove_preserve_other_hooks() {
        let original = json!({"extra":1,"hooks":{"Stop":[{"matcher":"x","hooks":[{"type":"command","command":"user"}]}],"Other":[{"hooks":[{"command":"other"}]}]}});
        let mut value = original.clone();
        let cmd = command(Path::new("/tmp/a'b/galgo")).unwrap();
        assert!(cmd.contains("a'\\''b"));
        assert!(merge(&mut value, &cmd).unwrap());
        let first = value.clone();
        assert!(!merge(&mut value, &cmd).unwrap());
        assert_eq!(value, first);
        // Una entrada del usuario añadida después de instalar sobrevive al uninstall.
        value["hooks"]["Stop"].as_array_mut().unwrap().push(json!({"hooks":[{"command":"later"}]}));
        assert_eq!(remove(&mut value), 3);
        assert_eq!(value["extra"], original["extra"]);
        assert_eq!(value["hooks"]["Stop"][0], original["hooks"]["Stop"][0]);
        assert_eq!(value["hooks"]["Stop"][1]["hooks"][0]["command"], "later");
        assert_eq!(remove(&mut value), 0);
    }
    #[test]
    fn temp_home_backup_and_symlink() {
        let dir = std::env::temp_dir().join(format!("galgo-codex-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join(".codex/hooks.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let original = "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"command\":\"user\"}]}]}}";
        std::fs::write(&file, original).unwrap();
        let mut data = read(&file).unwrap();
        merge(&mut data, &command(Path::new("/tmp/galgo")).unwrap()).unwrap();
        write(&file, &data, true).unwrap();
        assert_eq!(std::fs::read_to_string(file.with_file_name("hooks.json.galgo-bak")).unwrap(), original);
        assert_eq!(remove(&mut data), 3);
        write(&file, &data, false).unwrap();
        assert_eq!(read(&file).unwrap(), serde_json::from_str::<Value>(original).unwrap());
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read(&link).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
