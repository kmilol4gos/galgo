//! Statusline Claude: informe numérico best-effort y encadenamiento sin alterar stdout.
use anyhow::{bail, Context, Result};
use galgo_proto::{socket_path, LimitReport, Request};
use serde_json::Value;
use std::{io::{Read, Write}, os::unix::{fs::OpenOptionsExt, net::UnixStream}, path::{Path, PathBuf}, process::{Command, Stdio}, time::Duration};

const MARKER: &str = " # galgo-statusline";
fn sidecar(settings_path: &Path) -> Result<PathBuf> {
    let path = if settings_path.is_absolute() { settings_path.to_path_buf() } else { std::env::current_dir()?.join(settings_path) };
    Ok(path.with_extension("json.galgo-statusline"))
}
fn read_sidecar(path: &Path) -> Result<Option<Value>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_file() => bail!("el respaldo del statusline no es un archivo regular"),
        Ok(_) => Ok(Some(serde_json::from_slice(&std::fs::read(path)?).context("respaldo del statusline inválido")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn owned(v: &Value) -> bool { v.get("command").and_then(Value::as_str).is_some_and(|s| s.ends_with(MARKER)) }
/// Prepara cambios en memoria; Some contiene el respaldo que debe crearse antes de escribir settings.
pub(crate) fn prepare(settings: &mut Value, settings_path: &Path, exe: &Path) -> Result<Option<(PathBuf, Value)>> {
    let path = sidecar(settings_path)?;
    let had_previous = settings.get("statusLine").is_some();
    let previous = settings.get("statusLine").cloned().unwrap_or(Value::Null);
    if owned(&previous) {
        let saved = read_sidecar(&path)?.context("statusline de galgo sin respaldo; no se modificó")?;
        if saved.get("managed") != Some(&previous) || saved.get("previous").is_none() { bail!("statusline de galgo modificado; no se reemplazó"); }
        return Ok(None);
    }
    if read_sidecar(&path)?.is_some() { bail!("ya existe un respaldo de statusline; no se sobrescribió"); }
    if !previous.is_null() && (!previous.is_object() || previous.get("type").and_then(Value::as_str) != Some("command") || previous.get("command").and_then(Value::as_str).is_none()) {
        bail!("statusLine existente no es un comando compatible; no se modificó");
    }
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    let command = format!("{} statusline claude --backup {}{}", quote(exe), quote(&path), MARKER);
    let mut managed = if previous.is_null() { serde_json::json!({"type":"command"}) } else { previous.clone() };
    managed["command"] = command.into();
    settings.as_object_mut().context("settings no es un objeto")?.insert("statusLine".into(), managed.clone());
    Ok(Some((path, serde_json::json!({"previous":previous,"had_previous":had_previous,"managed":managed}))))
}
pub(crate) fn save_sidecar(path: &Path, value: &Value) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("ruta sin directorio")?)?;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    if let Err(e) = serde_json::to_writer(&mut f, value) { let _ = std::fs::remove_file(path); return Err(e.into()); }
    Ok(())
}
/// Solo restaura si el objeto completo es el instalado; conserva cambios posteriores del usuario.
pub(crate) fn restore(settings: &mut Value, settings_path: &Path) -> Result<Option<PathBuf>> {
    let path = sidecar(settings_path)?;
    let Some(saved) = read_sidecar(&path)? else { return Ok(None) };
    let current = settings.get("statusLine").cloned().unwrap_or(Value::Null);
    if saved.get("managed") != Some(&current) || !owned(&current) { return Ok(None); }
    let previous = saved.get("previous").context("respaldo de statusline incompleto")?;
    let root = settings.as_object_mut().context("settings no es un objeto")?;
    if !saved.get("had_previous").and_then(Value::as_bool).context("respaldo de statusline incompleto")? { root.remove("statusLine"); }
    else { root.insert("statusLine".into(), previous.clone()); }
    Ok(Some(path))
}

fn positive_float(v: Option<&Value>) -> Option<f64> { v?.as_f64().filter(|n| n.is_finite() && *n >= 0.0) }
fn model(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    (s.len() <= 128 && !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))).then(|| s.to_owned())
}
pub(crate) fn claude_report(pane: String, value: &Value) -> Request {
    let current = &value["context_window"]["current_usage"];
    let tokens = current.get("input_tokens").and_then(Value::as_u64).and_then(|i| {
        i.checked_add(current.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0))?
            .checked_add(current.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0))
    });
    let limits = ["five_hour", "seven_day"].into_iter().filter_map(|name| {
        let value = &value["rate_limits"][name];
        let used = positive_float(value.get("used_percentage"))?;
        if used > 100.0 { return None; }
        Some(LimitReport { name: name.to_string(), used_pct: used as f32, resets_at: value.get("resets_at").and_then(Value::as_u64) })
    }).collect();
    Request::UsageReport { pane, agent: "claude".into(), model: model(value.get("model").and_then(|v| v.get("id"))), context_tokens: tokens,
        context_window: value["context_window"].get("context_window_size").and_then(Value::as_u64).filter(|v| *v > 0),
        cost_usd: positive_float(value.get("cost").and_then(|v| v.get("total_cost_usd"))), limits }
}
pub(crate) fn pi_report(pane: String, value: &Value) -> Option<Request> {
    let context = value.get("context")?;
    let context_tokens = context.get("tokens").and_then(Value::as_u64);
    let context_window = context.get("contextWindow").and_then(Value::as_u64).filter(|v| *v > 0);
    if context_tokens.is_none() && context_window.is_none() { return None; }
    Some(Request::UsageReport { pane, agent: "pi".into(), model: model(value.get("model")), context_tokens, context_window, cost_usd: None, limits: vec![] })
}
/// La espera total del hilo llamante es 200 ms incluso si connect() queda bloqueado.
pub(crate) fn send_best_effort(req: Request) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = (|| -> std::io::Result<()> {
            let mut stream = UnixStream::connect(socket_path())?;
            stream.set_write_timeout(Some(Duration::from_millis(200)))?;
            let mut bytes = serde_json::to_vec(&req)?; bytes.push(b'\n');
            stream.write_all(&bytes)?;
            Ok(())
        })();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(Duration::from_millis(200));
}
fn previous_command(path: &Path) -> Option<String> {
    let file = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&file).ok()?;
    value.get("previous")?.get("command")?.as_str().map(str::to_owned)
}
pub fn run(backup: Option<&Path>) -> i32 {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() { return 0; }
    if let Ok(pane) = std::env::var("GALGO_PANE") {
        if let Ok(value) = serde_json::from_slice::<Value>(&input) { send_best_effort(claude_report(pane, &value)); }
    }
    if let Some(command) = backup.and_then(previous_command) {
        let mut child = match Command::new("sh").args(["-c", &command]).stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn() {
            Ok(child) => child, Err(_) => return 0,
        };
        if let Some(mut stdin) = child.stdin.take() { let _ = stdin.write_all(&input); }
        let _ = child.wait();
    } else {
        println!("galgo");
    }
    0
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn reports_only_numeric_fields_and_valid_limits() {
        let p = json!({"cwd":"SECRET", "model":{"id":"claude-opus-5-5"}, "cost":{"total_cost_usd":1.5},"context_window":{"context_window_size":1000,"current_usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3,"output_tokens":999}},"rate_limits":{"five_hour":{"used_percentage":10.5,"resets_at":123},"seven_day":{"used_percentage":200}}});
        let req = claude_report("pane".into(), &p);
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["context_tokens"],33); assert_eq!(json["context_window"],1000); assert_eq!(json["cost_usd"],1.5);
        assert_eq!(json["limits"].as_array().unwrap().len(),1); assert_eq!(json["limits"][0]["name"],"five_hour"); assert!(!json.to_string().contains("SECRET"));
    }
    #[test]
    fn statusline_install_reinstall_restore_and_user_edits() {
        let home = std::env::temp_dir().join(format!("galgo-statusline-test-{}",std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let path = home.join(".claude/settings.json");
        let original = json!({"statusLine":{"type":"command","command":"printf previo","padding":7},"other":true});
        let mut settings = original.clone();
        let (backup, value) = prepare(&mut settings, &path, Path::new("/tmp/galgo")).unwrap().unwrap();
        save_sidecar(&backup,&value).unwrap();
        assert_eq!(previous_command(&backup).as_deref(), Some("printf previo"));
        assert!(prepare(&mut settings,&path,Path::new("/tmp/galgo")).unwrap().is_none());
        let installed = settings.clone();
        settings["statusLine"]["padding"] = json!(9);
        assert!(restore(&mut settings,&path).unwrap().is_none());
        assert!(backup.exists());
        assert!(prepare(&mut settings,&path,Path::new("/tmp/galgo")).is_err());
        settings = installed;
        assert_eq!(restore(&mut settings,&path).unwrap(), Some(backup.clone()));
        assert_eq!(settings,original);
        std::fs::remove_file(&backup).unwrap();
        let mut empty = json!({});
        let (backup,value) = prepare(&mut empty,&path,Path::new("/tmp/galgo")).unwrap().unwrap();
        save_sidecar(&backup,&value).unwrap();
        assert_eq!(previous_command(&backup),None);
        assert!(restore(&mut empty,&path).unwrap().is_some());
        assert_eq!(empty,json!({}));
        std::fs::remove_file(&backup).unwrap();
        let mut null_setting = json!({"statusLine":null});
        let (backup,value) = prepare(&mut null_setting,&path,Path::new("/tmp/galgo")).unwrap().unwrap();
        save_sidecar(&backup,&value).unwrap();
        assert!(restore(&mut null_setting,&path).unwrap().is_some());
        assert_eq!(null_setting,json!({"statusLine":null}));
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn pi_context_is_numeric_and_optional() {
        let req=pi_report("p".into(), &json!({"context":{"tokens":100,"contextWindow":1000,"percent":10.0},"model":"test-model","message":"SECRET"})).unwrap();
        let value=serde_json::to_value(req).unwrap(); assert_eq!(value["context_tokens"],100); assert_eq!(value["context_window"],1000); assert!(!value.to_string().contains("SECRET"));
        assert!(pi_report("p".into(), &json!({"context":{"tokens":null,"contextWindow":null}})).is_none());
    }
}
