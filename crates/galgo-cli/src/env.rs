//! `galgo env`: poner variables en archivos `.env` sin abrir un IDE.
//!
//! Los valores no viajan por el daemon ni por argv: se leen del teclado (sin eco) o de stdin y se
//! escriben directo al archivo. Listar muestra nombres, nunca valores.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use galgo_proto::{client::request, HookEvent, Request};
use std::{
    io::{IsTerminal, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Subcommand)]
pub enum EnvCmd {
    /// Muestra los nombres de las variables (nunca los valores) y si el archivo está en .gitignore.
    List {
        #[arg(long = "dir", default_value = ".")]
        dirs: Vec<PathBuf>,
        #[arg(long, default_value = ".env")]
        file: String,
    },
    /// Pone una variable en uno o varios directorios. El valor se pide sin eco o llega por stdin.
    Set {
        key: String,
        #[arg(long = "dir", default_value = ".")]
        dirs: Vec<PathBuf>,
        #[arg(long, default_value = ".env")]
        file: String,
    },
    /// Quita una variable de uno o varios directorios.
    Unset {
        key: String,
        #[arg(long = "dir", default_value = ".")]
        dirs: Vec<PathBuf>,
        #[arg(long, default_value = ".env")]
        file: String,
    },
    /// Para agentes: pide una variable al humano sin que el valor pase por la conversación.
    /// Avisa al manager del panel y espera a que aparezca en el archivo.
    Request {
        key: String,
        #[arg(long, default_value = "")]
        reason: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value = ".env")]
        file: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[arg(long, env = "GALGO_PANE")]
        pane: String,
    },
}

pub fn run(cmd: &EnvCmd) -> Result<()> {
    match cmd {
        EnvCmd::List { dirs, file } => {
            for d in dirs {
                let path = target(d, file)?;
                let text = read_or_empty(&path)?;
                let keys = keys_in(&text);
                let ignored = if path.exists() { git_ignored(&path) } else { None };
                let note = match ignored {
                    Some(false) => "  ⚠ NO está en .gitignore",
                    _ => "",
                };
                println!("{}{note}", path.display());
                if keys.is_empty() {
                    println!("  (sin variables)");
                }
                for (k, empty) in keys {
                    println!("  {k}{}", if empty { "  (vacía)" } else { "" });
                }
            }
            Ok(())
        }
        EnvCmd::Set { key, dirs, file } => {
            check_key(key)?;
            let value = read_value(key)?;
            for d in dirs {
                let path = target(d, file)?;
                let new = set_in(&read_or_empty(&path)?, key, &value);
                write_private(&path, &new)?;
                let warn = if git_ignored(&path) == Some(false) { "  ⚠ no está en .gitignore" } else { "" };
                println!("{key} → {}{warn}", path.display());
            }
            Ok(())
        }
        EnvCmd::Unset { key, dirs, file } => {
            check_key(key)?;
            for d in dirs {
                let path = target(d, file)?;
                let text = read_or_empty(&path)?;
                let new = unset_in(&text, key);
                if new != text {
                    write_private(&path, &new)?;
                    println!("{key} quitada de {}", path.display());
                } else {
                    println!("{key} no estaba en {}", path.display());
                }
            }
            Ok(())
        }
        EnvCmd::Request { key, reason, dir, file, timeout, pane } => {
            check_key(key)?;
            let path = target(dir, file)?;
            let has = |p: &Path| read_or_empty(p).map(|t| is_set(&t, key)).unwrap_or(false);
            if has(&path) {
                println!("{key} ya está definida en {}", path.display());
                return Ok(());
            }
            let why = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
            let summary = format!(
                "necesita la variable {key}{why}. Ponla con: galgo env set {key} --dir {}",
                dir.canonicalize().unwrap_or_else(|_| dir.clone()).display()
            );
            let resp = request(&Request::Hook { pane: pane.clone(), event: HookEvent::Blocked, summary: Some(summary) })?;
            if !resp.ok {
                bail!("{}", resp.error.unwrap_or_else(|| "el daemon rechazó el aviso".into()));
            }
            let end = Instant::now() + Duration::from_secs(*timeout);
            while Instant::now() < end {
                if has(&path) {
                    let _ = request(&Request::Hook { pane: pane.clone(), event: HookEvent::Working, summary: None });
                    println!("{key} disponible en {}", path.display());
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            bail!("tiempo agotado esperando {key}")
        }
    }
}

fn target(dir: &Path, file: &str) -> Result<PathBuf> {
    if file.contains('/') || file == ".." || file.is_empty() {
        bail!("--file debe ser un nombre de archivo, sin rutas");
    }
    if !dir.is_dir() {
        bail!("{} no es un directorio", dir.display());
    }
    Ok(dir.join(file))
}

fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!("nombre de variable inválido: {key} (letras, dígitos y _; no empieza con dígito)");
    }
    Ok(())
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("no se pudo leer {}", path.display())),
    }
}

/// Escribe de forma atómica. Conserva los permisos del archivo; uno nuevo queda en 0600.
fn write_private(path: &Path, text: &str) -> Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&real).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    let tmp = real.with_extension(format!("galgo-tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("no se pudo crear {}", tmp.display()))?;
    let res = f
        .write_all(text.as_bytes())
        .and_then(|_| f.sync_all())
        .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)))
        .and_then(|_| std::fs::rename(&tmp, &real));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("no se pudo escribir {}", real.display()));
    }
    Ok(())
}

/// `Some(true)` si git lo ignora, `Some(false)` si no, `None` si no es un repo o no hay git.
fn git_ignored(path: &Path) -> Option<bool> {
    let dir = path.parent()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q"])
        .arg(path.file_name()?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    match out.code() {
        Some(0) => Some(true),
        Some(1) => {
            let inside = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["rev-parse", "--is-inside-work-tree"])
                .stderr(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .status()
                .ok()?;
            inside.success().then_some(false)
        }
        _ => None,
    }
}

fn read_value(key: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("Valor de {key} (no se muestra): ");
        let v = read_hidden()?;
        eprintln!();
        if v.contains('\n') {
            bail!("el valor no puede tener saltos de línea desde el teclado");
        }
        return Ok(v);
    }
    let mut s = String::new();
    stdin.lock().read_to_string(&mut s)?;
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    Ok(s)
}

fn read_hidden() -> Result<String> {
    // SAFETY: termios sobre el descriptor 0, que es un terminal; se restaura siempre.
    unsafe {
        let mut old: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut old) != 0 {
            bail!("no se pudo leer el terminal");
        }
        let mut quiet = old;
        quiet.c_lflag &= !libc::ECHO;
        libc::tcsetattr(0, libc::TCSAFLUSH, &quiet);
        let mut line = String::new();
        let r = std::io::stdin().read_line(&mut line);
        libc::tcsetattr(0, libc::TCSAFLUSH, &old);
        r?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

// ---- Edición de texto .env (pura, con pruebas) ----

/// Nombre de la variable que define esta línea, si la define.
fn line_key(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let t = t.strip_prefix("export").filter(|r| r.starts_with([' ', '\t'])).map_or(t, str::trim_start);
    let (k, _) = t.split_once('=')?;
    let k = k.trim_end();
    (!k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(k)
}

fn line_value(line: &str) -> &str {
    line.split_once('=').map_or("", |(_, v)| v.trim())
}

/// (nombre, vacía?) de cada variable, en orden.
pub fn keys_in(text: &str) -> Vec<(String, bool)> {
    text.lines()
        .filter_map(|l| line_key(l).map(|k| (k.to_string(), line_value(l).trim_matches(['"', '\'']).is_empty())))
        .collect()
}

pub fn is_set(text: &str, key: &str) -> bool {
    keys_in(text).iter().any(|(k, empty)| k == key && !empty)
}

fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:@+,=%".contains(c));
    if plain {
        return value.to_string();
    }
    let mut s = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '$' => s.push_str("\\$"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Reemplaza la variable en su sitio o la añade al final. Conserva el resto tal cual.
pub fn set_in(text: &str, key: &str, value: &str) -> String {
    let new_line = format!("{key}={}", quote(value));
    let mut out = String::new();
    let mut done = false;
    for line in text.split_inclusive('\n') {
        if line_key(line.trim_end_matches(['\n', '\r'])) == Some(key) {
            if !done {
                let prefix = if line.trim_start().starts_with("export") { "export " } else { "" };
                let eol = if line.ends_with('\n') { "\n" } else { "" };
                out.push_str(&format!("{prefix}{new_line}{eol}"));
                done = true;
            }
            continue;
        }
        out.push_str(line);
    }
    if !done {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&new_line);
        out.push('\n');
    }
    out
}

pub fn unset_in(text: &str, key: &str) -> String {
    text.split_inclusive('\n')
        .filter(|l| line_key(l.trim_end_matches(['\n', '\r'])) != Some(key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_replaces_and_keeps_the_rest() {
        let t = "# nota\nA=1\nexport B=2\n\nC=3";
        assert_eq!(set_in(t, "A", "uno"), "# nota\nA=uno\nexport B=2\n\nC=3");
        assert_eq!(set_in(t, "B", "x y"), "# nota\nA=1\nexport B=\"x y\"\n\nC=3");
        assert_eq!(set_in(t, "D", "4"), "# nota\nA=1\nexport B=2\n\nC=3\nD=4\n");
        assert_eq!(set_in("", "K", "v"), "K=v\n");
    }

    #[test]
    fn duplicates_collapse_and_comments_are_ignored() {
        assert_eq!(set_in("A=1\n#A=9\nA=2\n", "A", "3"), "A=3\n#A=9\n");
        assert_eq!(keys_in("#A=1\nB=\nC=\"\"\nD=x\n"), vec![("B".into(), true), ("C".into(), true), ("D".into(), false)]);
    }

    #[test]
    fn quoting_is_safe() {
        assert_eq!(quote("sk-abc.123"), "sk-abc.123");
        assert_eq!(quote("a b#c"), "\"a b#c\"");
        assert_eq!(quote("p\"q\\r$s\nt"), "\"p\\\"q\\\\r\\$s\\nt\"");
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn unset_and_is_set() {
        assert_eq!(unset_in("A=1\nB=2\nA=3\n", "A"), "B=2\n");
        assert!(is_set("A=1\n", "A"));
        assert!(!is_set("A=\n", "A"));
        assert!(!is_set("AB=1\n", "A"));
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(check_key("OK_1").is_ok());
        assert!(check_key("1A").is_err());
        assert!(check_key("A B").is_err());
        assert!(check_key("A=B").is_err());
    }
}
