//! Integración con Claude Code: el hook que traduce sus eventos y el instalador de `settings.json`.
//!
//! Esquema de hooks verificado en https://code.claude.com/docs/en/hooks :
//! `{"hooks": {"<Evento>": [{"matcher"?: "...", "hooks": [{"type": "command", "command": "...", "timeout": N}]}]}}`
//! El comando recibe el JSON del evento por stdin (`hook_event_name`, `notification_type`,
//! `message`, `last_assistant_message`…). Exit 0 no tiene efecto sobre Claude.

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use galgo_proto::{socket_path, HookEvent, Request};
use serde_json::{json, Map, Value};
use std::{
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

/// Texto que identifica nuestras entradas en settings.json, para actualizarlas o quitarlas
/// sin tocar los hooks del usuario.
const MARKER: &str = " # galgo-hook";
const HOOK_TIMEOUT_SECS: u64 = 5;
const SUMMARY_MAX_CHARS: usize = 600;

/// (evento de Claude Code, matcher)
const INSTALLED: &[(&str, Option<&str>)] = &[
    ("UserPromptSubmit", None),
    ("PostToolUse", None),
    ("Notification", Some("permission_prompt")),
    ("Notification", Some("elicitation_dialog")),
    ("Stop", None),
    ("StopFailure", None),
];

#[derive(Clone, Copy, ValueEnum)]
pub enum Agent {
    Claude,
    Pi,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Scope {
    /// ~/.claude/settings.json (o $CLAUDE_CONFIG_DIR/settings.json)
    User,
    /// ./.claude/settings.json
    Project,
    /// ./.claude/settings.local.json
    Local,
}

#[derive(Subcommand)]
pub enum HooksCmd {
    /// Agrega los hooks de galgo a la configuración del agente. Muestra el plan y pide confirmación.
    Install {
        agent: Agent,
        #[arg(long, value_enum, default_value = "user")]
        scope: Scope,
        /// No pregunta antes de escribir.
        #[arg(long)]
        yes: bool,
        /// Solo muestra qué cambiaría.
        #[arg(long)]
        dry_run: bool,
    },
    /// Quita únicamente los hooks de galgo.
    Uninstall {
        agent: Agent,
        #[arg(long, value_enum, default_value = "user")]
        scope: Scope,
        #[arg(long)]
        yes: bool,
    },
    /// Indica si los hooks están instalados.
    Status {
        agent: Agent,
        #[arg(long, value_enum, default_value = "user")]
        scope: Scope,
    },
}

// ---------------------------------------------------------------------------------------------
// El hook en sí
// ---------------------------------------------------------------------------------------------

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SUMMARY_MAX_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(SUMMARY_MAX_CHARS).collect();
    format!("{cut}…")
}

/// Traduce el JSON que Claude Code manda por stdin a un evento de galgo.
pub fn map_event(p: &Value) -> Option<(HookEvent, Option<String>)> {
    let text = |k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(truncate)
    };
    match p.get("hook_event_name")?.as_str()? {
        "UserPromptSubmit" => Some((HookEvent::PromptSubmitted, None)),
        "PostToolUse" => Some((HookEvent::Working, None)),
        "Stop" => Some((HookEvent::TurnEnded, text("last_assistant_message"))),
        "StopFailure" => Some((
            HookEvent::Failed,
            text("last_assistant_message").or_else(|| text("error")).or_else(|| text("error_details")),
        )),
        "Notification" => match p.get("notification_type")?.as_str()? {
            "permission_prompt" | "elicitation_dialog" | "agent_needs_input" => {
                Some((HookEvent::Blocked, text("message")))
            }
            "idle_prompt" => Some((HookEvent::Idle, None)),
            _ => None,
        },
        _ => None,
    }
}

fn notify_daemon(pane: String, event: HookEvent, summary: Option<String>) -> Result<()> {
    // Si no hay daemon, no se arranca uno: un hook nunca debe hacer ruido ni demorar a Claude.
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut line = serde_json::to_string(&Request::Hook { pane, event, summary })?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(())
}

/// Punto de entrada de `galgo hook claude`. Siempre sale con 0 y no imprime nada por stdout:
/// en UserPromptSubmit, el stdout de un hook se agrega al contexto de Claude.
pub fn run_hook(agent: Agent) -> i32 {
    let Ok(pane) = std::env::var("GALGO_PANE") else { return 0 };
    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_string(&mut input);
    }
    let payload = serde_json::from_str::<Value>(&input).unwrap_or(Value::Null);
    let mapped = match agent {
        Agent::Claude => map_event(&payload),
        Agent::Pi => crate::pi::map_event(&payload),
    };
    let Some((event, summary)) = mapped else { return 0 };
    if let Err(e) = notify_daemon(pane, event, summary) {
        if std::env::var_os("GALGO_DEBUG").is_some() {
            eprintln!("galgo hook: {e:#}");
        }
    }
    0
}

// ---------------------------------------------------------------------------------------------
// settings.json
// ---------------------------------------------------------------------------------------------

fn settings_path(scope: Scope) -> Result<PathBuf> {
    Ok(match scope {
        Scope::User => {
            let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(d) => PathBuf::from(d),
                None => PathBuf::from(std::env::var_os("HOME").context("HOME no está definido")?)
                    .join(".claude"),
            };
            base.join("settings.json")
        }
        Scope::Project => PathBuf::from(".claude").join("settings.json"),
        Scope::Local => PathBuf::from(".claude").join("settings.local.json"),
    })
}

/// Comando que se escribe en settings.json. Ruta absoluta entre comillas simples; si falla,
/// `|| true` evita que Claude Code muestre un error por un hook que no es esencial.
pub fn hook_command(exe: &Path) -> String {
    let quoted = exe.display().to_string().replace('\'', r"'\''");
    format!("'{quoted}' hook claude 2>/dev/null || true{MARKER}")
}

fn is_ours(command: &str) -> bool {
    if command.ends_with(MARKER) { return true; }
    let Some(path) = command.strip_prefix('\'').and_then(|s| s.strip_suffix("' hook claude 2>/dev/null || true")) else { return false };
    let decoded = path.replace("'\\''", "'");
    path == decoded.replace('\'', "'\\''") && decoded.starts_with('/') && decoded.ends_with("/galgo")
}

fn entry_commands(entry: &Value) -> impl Iterator<Item = &str> {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
}

fn has_ours(entry: &Value) -> bool {
    entry_commands(entry).any(is_ours)
}

/// Actualiza el comando de nuestras entradas dentro de un grupo. Devuelve si cambió algo.
fn set_command(entry: &mut Value, command: &str) -> bool {
    let mut changed = false;
    if let Some(hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
        for h in hooks {
            let ours = h.get("command").and_then(Value::as_str).is_some_and(is_ours);
            if ours && h["command"].as_str() != Some(command) {
                h["command"] = json!(command);
                changed = true;
            }
        }
    }
    changed
}

/// Agrega o actualiza nuestros hooks sin tocar nada más. Idempotente.
pub fn merge(settings: &mut Value, command: &str) -> Result<Vec<String>> {
    let root = settings.as_object_mut().context("settings.json no es un objeto JSON")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("\"hooks\" en settings.json no es un objeto")?;
    let mut changes = Vec::new();
    for (event, matcher) in INSTALLED {
        let arr = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} no es una lista"))?;
        let label = match matcher {
            Some(m) => format!("{event} [{m}]"),
            None => (*event).to_string(),
        };
        let existing = arr
            .iter_mut()
            .find(|e| e.get("matcher").and_then(Value::as_str) == *matcher && has_ours(e));
        match existing {
            Some(entry) => {
                if set_command(entry, command) {
                    changes.push(format!("actualiza {label}"));
                } else {
                    changes.push(format!("sin cambios {label}"));
                }
            }
            None => {
                let mut e = Map::new();
                if let Some(m) = matcher {
                    e.insert("matcher".into(), json!(m));
                }
                e.insert(
                    "hooks".into(),
                    json!([{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS }]),
                );
                arr.push(Value::Object(e));
                changes.push(format!("agrega {label}"));
            }
        }
    }
    Ok(changes)
}

/// Quita solo nuestras entradas y limpia lo que quede vacío. Devuelve cuántas quitó.
pub fn remove(settings: &mut Value) -> usize {
    let mut removed = 0;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else { return 0 };
    hooks.retain(|_, value| {
        let Some(arr) = value.as_array_mut() else { return true };
        let mut changed = false;
        arr.retain_mut(|entry| {
            let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) else { return true };
            let before = inner.len();
            inner.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(is_ours));
            let n = before - inner.len();
            removed += n;
            changed |= n > 0;
            n == 0 || !inner.is_empty()
        });
        !changed || !arr.is_empty()
    });
    if removed > 0 && hooks.is_empty() {
        settings.as_object_mut().unwrap().remove("hooks");
    }
    removed
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(json!({})),
        Ok(s) => serde_json::from_str(&s).with_context(|| {
            format!("{} no es JSON válido; no toqué nada", path.display())
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("no pude leer {}", path.display())),
    }
}

pub(crate) fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("hace falta confirmar: ejecuta de nuevo con --yes");
    }
    print!("{question} [s/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "s" | "si" | "sí" | "y" | "yes"))
}

/// Escribe de forma atómica y deja una copia de la configuración original la primera vez.
fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let resolved = match std::fs::symlink_metadata(path) {
        Ok(_) => path.canonicalize()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e.into()),
    };
    let path = resolved.as_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    if path.exists() {
        let backup = path.with_extension("json.galgo-bak");
        if !backup.exists() {
            std::fs::copy(path, &backup)
                .with_context(|| format!("no pude respaldar en {}", backup.display()))?;
            println!("respaldo: {}", backup.display());
        }
    }
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let tmp = path.with_extension(format!("json.galgo-tmp-{}-{:016x}", std::process::id(), u64::from_ne_bytes(random)));
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    let result = (|| -> Result<()> {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(text.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result?;
    Ok(())
}

pub fn hooks_command(cmd: &HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::Install { agent: Agent::Pi, scope, yes, dry_run } => crate::pi::install_command(*scope, *yes, *dry_run)?,
        HooksCmd::Uninstall { agent: Agent::Pi, scope, yes } => crate::pi::uninstall_command(*scope, *yes)?,
        HooksCmd::Status { agent: Agent::Pi, scope } => crate::pi::status_command(*scope)?,
        HooksCmd::Install { agent: Agent::Claude, scope, yes, dry_run } => {
            let path = settings_path(*scope)?;
            let mut settings = read_settings(&path)?;
            let exe = std::env::current_exe()?.canonicalize()?;
            let changes = merge(&mut settings, &hook_command(&exe))?;
            println!("Claude Code · {}", path.display());
            for c in &changes {
                println!("  {c}");
            }
            if changes.iter().all(|c| c.starts_with("sin cambios")) {
                println!("Ya está instalado.");
                return Ok(());
            }
            println!("Los hooks solo actúan dentro de paneles de galgo (variable GALGO_PANE).");
            if *dry_run {
                println!("(simulación: no se escribió nada)");
                return Ok(());
            }
            if !yes && !confirm("¿Aplicar estos cambios?")? {
                println!("Cancelado.");
                return Ok(());
            }
            write_settings(&path, &settings)?;
            println!("Listo. Si Claude Code pide confiar en la carpeta, acéptalo para que los hooks corran.");
        }
        HooksCmd::Uninstall { agent: Agent::Claude, scope, yes } => {
            let path = settings_path(*scope)?;
            let mut settings = read_settings(&path)?;
            let n = remove(&mut settings);
            if n == 0 {
                println!("No hay hooks de galgo en {}", path.display());
                return Ok(());
            }
            println!("Se quitarán {n} hook(s) de galgo de {}", path.display());
            if !yes && !confirm("¿Continuar?")? {
                println!("Cancelado.");
                return Ok(());
            }
            write_settings(&path, &settings)?;
            println!("Listo.");
        }
        HooksCmd::Status { agent: Agent::Claude, scope } => {
            let path = settings_path(*scope)?;
            let settings = read_settings(&path)?;
            println!("Claude Code · {}", path.display());
            for (event, matcher) in INSTALLED {
                let on = settings["hooks"][*event].as_array().is_some_and(|a| {
                    a.iter().any(|e| e.get("matcher").and_then(Value::as_str) == *matcher && has_ours(e))
                });
                let label = match matcher {
                    Some(m) => format!("{event} [{m}]"),
                    None => (*event).to_string(),
                };
                println!("  {} {label}", if on { "✓" } else { "·" });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "'/x/galgo' hook claude 2>/dev/null || true";

    #[test]
    fn settings_symlink_and_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("galgo-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real.json");
        let link = dir.join("settings.json");
        std::fs::write(&target, "{}").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &link).unwrap();
        write_settings(&link, &json!({"secret": 1})).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_settings(&target).unwrap()["secret"], 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn maps_claude_events() {
        let stop = json!({"hook_event_name": "Stop", "last_assistant_message": "  listo  "});
        assert_eq!(map_event(&stop), Some((HookEvent::TurnEnded, Some("listo".into()))));
        let perm = json!({"hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "Claude needs permission"});
        assert_eq!(map_event(&perm), Some((HookEvent::Blocked, Some("Claude needs permission".into()))));
        let idle = json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"});
        assert_eq!(map_event(&idle), Some((HookEvent::Idle, None)));
        let other = json!({"hook_event_name": "Notification", "notification_type": "auth_success"});
        assert_eq!(map_event(&other), None);
        assert_eq!(map_event(&json!({"hook_event_name": "PostToolUse"})), Some((HookEvent::Working, None)));
        assert_eq!(
            map_event(&json!({"hook_event_name": "UserPromptSubmit", "prompt": "x"})),
            Some((HookEvent::PromptSubmitted, None))
        );
        assert_eq!(map_event(&json!({"hook_event_name": "SessionStart"})), None);
        assert_eq!(map_event(&Value::Null), None);
    }

    #[test]
    fn failure_falls_back_to_error_text() {
        let p = json!({"hook_event_name": "StopFailure", "error": "rate_limit"});
        assert_eq!(map_event(&p), Some((HookEvent::Failed, Some("rate_limit".into()))));
    }

    #[test]
    fn long_summaries_are_cut() {
        let p = json!({"hook_event_name": "Stop", "last_assistant_message": "a".repeat(2000)});
        let (_, s) = map_event(&p).unwrap();
        assert_eq!(s.unwrap().chars().count(), SUMMARY_MAX_CHARS + 1);
    }

    #[test]
    fn marker_identifies_only_our_hooks() {
        assert!(hook_command(Path::new("/x/galgo")).ends_with(" # galgo-hook"));
        assert!(is_ours(CMD));
        assert!(!is_ours("galgo-wrapper hook claude"));
        assert!(!is_ours("echo galgo hook claude"));
        assert!(!is_ours("'/x/not-galgo' hook claude 2>/dev/null || true"));
    }

    #[test]
    fn uninstall_preserves_existing_empty_values() {
        let original = json!({"hooks": {"Other": [], "Custom": [{"hooks": []}]}});
        let mut value = original.clone();
        merge(&mut value, CMD).unwrap();
        remove(&mut value);
        assert_eq!(value, original);
        let mut empty = json!({"hooks": {}});
        assert_eq!(remove(&mut empty), 0);
        assert_eq!(empty, json!({"hooks": {}}));
        let mut formerly_empty = json!({"hooks": {"Stop": []}});
        merge(&mut formerly_empty, CMD).unwrap();
        remove(&mut formerly_empty);
        assert_eq!(formerly_empty, json!({}));
    }

    #[test]
    fn install_uninstall_never_write_unknown_top_level_keys() {
        let dir = std::env::temp_dir().join(format!("galgo-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = json!({"model": "opus", "hooks": {"Other": []}});
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        let mut value = read_settings(&path).unwrap();
        merge(&mut value, CMD).unwrap();
        write_settings(&path, &value).unwrap();
        let mut installed = read_settings(&path).unwrap();
        assert_eq!(keys(&installed), keys(&original));
        remove(&mut installed);
        write_settings(&path, &installed).unwrap();
        let uninstalled = read_settings(&path).unwrap();
        assert_eq!(keys(&uninstalled), keys(&original));
        assert_eq!(uninstalled, original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn install_into_empty_is_idempotent() {
        let mut s = json!({});
        let first = merge(&mut s, CMD).unwrap();
        assert_eq!(first.len(), INSTALLED.len());
        assert!(first.iter().all(|c| c.starts_with("agrega")));
        let snapshot = s.clone();
        let second = merge(&mut s, CMD).unwrap();
        assert!(second.iter().all(|c| c.starts_with("sin cambios")));
        assert_eq!(s, snapshot);
    }

    #[test]
    fn install_keeps_user_settings_and_hooks() {
        let mut s = json!({
            "model": "opus",
            "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "say listo" } ] } ] }
        });
        merge(&mut s, CMD).unwrap();
        assert_eq!(s["model"], "opus");
        let stop = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say listo");
        assert_eq!(s["hooks"]["Notification"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn reinstall_updates_a_moved_binary() {
        let mut s = json!({});
        merge(&mut s, "'/old/galgo' hook claude 2>/dev/null || true").unwrap();
        let changes = merge(&mut s, CMD).unwrap();
        assert!(changes.iter().all(|c| c.starts_with("actualiza")));
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], CMD);
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let original = json!({
            "model": "opus",
            "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "say listo" } ] } ] }
        });
        let mut s = original.clone();
        merge(&mut s, CMD).unwrap();
        assert_eq!(remove(&mut s), INSTALLED.len());
        assert_eq!(s, original);

        let mut only_ours = json!({});
        merge(&mut only_ours, CMD).unwrap();
        remove(&mut only_ours);
        assert_eq!(only_ours, json!({}));
    }

    #[test]
    fn rejects_unexpected_shapes() {
        assert!(merge(&mut json!([]), CMD).is_err());
        assert!(merge(&mut json!({"hooks": []}), CMD).is_err());
        assert!(merge(&mut json!({"hooks": {"Stop": {}}}), CMD).is_err());
    }

    #[test]
    fn command_quotes_paths_with_spaces_and_quotes() {
        assert_eq!(
            hook_command(Path::new("/Users/a b/it's/galgo")),
            r"'/Users/a b/it'\''s/galgo' hook claude 2>/dev/null || true # galgo-hook"
        );
    }
}
