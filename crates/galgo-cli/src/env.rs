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
        /// Permite escribir un valor vacío (por defecto se rechaza).
        #[arg(long)]
        allow_empty: bool,
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
        #[arg(long, default_value_t = 100)]
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
        EnvCmd::Set { key, dirs, file, allow_empty } => {
            check_key(key)?;
            let value = read_value(key)?;
            if value.is_empty() && !allow_empty {
                bail!("el valor está vacío; no se escribió nada (usa --allow-empty si es lo que quieres)");
            }
            if value.contains('\'') && value.contains(['$', '`', '\\']) {
                eprintln!("aviso: con una comilla simple el valor va entre comillas dobles; `source` interpretaría $, ` y \\");
            }
            for d in dirs {
                let path = target_w(d, file)?;
                locked(d, || {
                    let new = set_in(&read_or_empty_nofollow(&path)?, key, &value);
                    write_private(&path, &new)
                })?;
                let mut warn = String::new();
                if git_ignored(&path) == Some(false) {
                    warn.push_str("  ⚠ no está en .gitignore");
                }
                if std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
                    warn.push_str("  ⚠ permisos abiertos a otros usuarios (chmod 600)");
                }
                println!("{key} → {}{warn}", path.display());
            }
            Ok(())
        }
        EnvCmd::Unset { key, dirs, file } => {
            check_key(key)?;
            for d in dirs {
                let path = target_w(d, file)?;
                let changed = locked(d, || {
                    let text = read_or_empty_nofollow(&path)?;
                    let new = unset_in(&text, key);
                    if new == text {
                        return Ok(false);
                    }
                    write_private(&path, &new)?;
                    Ok(true)
                })?;
                if changed {
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
            let resume = || {
                let _ = request(&Request::Hook { pane: pane.clone(), event: HookEvent::Working, summary: None });
            };
            while Instant::now() < end {
                if has(&path) {
                    resume();
                    println!("{key} disponible en {}", path.display());
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            // El worker no debe quedar `blocked` para siempre. Repite el comando para seguir esperando
            // (el manager no recibe el mismo aviso dos veces).
            resume();
            bail!("tiempo agotado esperando {key}; vuelve a ejecutar el comando para seguir esperando")
        }
    }
}

fn target(dir: &Path, file: &str) -> Result<PathBuf> {
    target_for(dir, file, false)
}

/// Para escribir: rechaza enlaces simbólicos. Leer a través de un enlace no expone nada.
fn target_w(dir: &Path, file: &str) -> Result<PathBuf> {
    target_for(dir, file, true)
}

fn target_for(dir: &Path, file: &str, write: bool) -> Result<PathBuf> {
    if file.contains('/') || file == ".." || file.is_empty() {
        bail!("--file debe ser un nombre de archivo, sin rutas");
    }
    if !dir.is_dir() {
        bail!("{} no es un directorio", dir.display());
    }
    let path = dir.join(file);
    if write && std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("{} es un enlace simbólico; galgo no escribe secretos a través de enlaces", path.display());
    }
    Ok(path)
}

/// Serializa las ediciones del mismo directorio (varios agentes o varios `--dir`).
fn locked<T>(dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::os::fd::AsRawFd;
    let d = std::fs::File::open(dir).with_context(|| format!("no se pudo abrir {}", dir.display()))?;
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: flock sobre un descriptor válido que vive hasta el final de la función.
        if unsafe { libc::flock(d.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        if Instant::now() >= end {
            bail!("{} está bloqueado por otro proceso de galgo env desde hace 10 s", dir.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
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

/// Lectura para editar: no sigue enlaces (cierra la carrera entre la comprobación y la lectura).
fn read_or_empty_nofollow(path: &Path) -> Result<String> {
    let mut f = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e).with_context(|| format!("no se pudo leer {} (¿enlace simbólico?)", path.display())),
    };
    let mut s = String::new();
    f.read_to_string(&mut s).with_context(|| format!("no se pudo leer {}", path.display()))?;
    Ok(s)
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
    let real = path.to_path_buf();
    let mode = std::fs::metadata(&real).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o600);
    let mut tmp_name = real.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".galgo-tmp-{}", std::process::id()));
    let tmp = real.with_file_name(tmp_name);
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

static SAVED_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

extern "C" fn restore_and_exit(sig: libc::c_int) {
    if let Some(old) = SAVED_TERMIOS.get() {
        // SAFETY: tcsetattr y _exit son seguros en un manejador de señales.
        unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, old) };
    }
    unsafe { libc::_exit(128 + sig) };
}

/// Lee un valor sin eco en modo no canónico: no hay límite de línea (pegar un JWT largo funciona) y
/// Ctrl+C, Ctrl+D y Ctrl+Z cancelan al instante. Una señal externa también restaura el terminal.
fn read_hidden() -> Result<String> {
    // SAFETY: termios y read sobre el descriptor 0, que es un terminal; se restaura siempre.
    unsafe {
        let mut old: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut old) != 0 {
            bail!("no se pudo leer el terminal");
        }
        let _ = SAVED_TERMIOS.set(old);
        let mut raw = old;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let sigs = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGTSTP];
        let prev: Vec<libc::sighandler_t> = sigs.iter().map(|&s| libc::signal(s, handler)).collect();
        libc::tcsetattr(0, libc::TCSAFLUSH, &raw);
        let result = read_hidden_bytes();
        libc::tcsetattr(0, libc::TCSAFLUSH, &old);
        for (&s, &p) in sigs.iter().zip(&prev) {
            libc::signal(s, p);
        }
        result
    }
}

fn read_hidden_bytes() -> Result<String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let mut b = 0u8;
        // SAFETY: lee un byte del descriptor 0 a una variable local.
        let n = unsafe { libc::read(0, (&mut b as *mut u8).cast(), 1) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        match (n, b) {
            (1, b'\n' | b'\r') => break,
            (1, 0x7f | 0x08) => {
                while buf.pop().is_some_and(|c| (c & 0xC0) == 0x80) {}
            }
            (1, 0x03 | 0x04 | 0x1a) | (0, _) | (-1, _) => bail!("cancelado"),
            (1, c) if c < 0x20 => bail!("carácter de control no permitido en el valor"),
            (1, c) => buf.push(c),
            _ => bail!("cancelado"),
        }
    }
    String::from_utf8(buf).map_err(|_| anyhow::anyhow!("el valor no es UTF-8 válido"))
}

// ---- Edición de texto .env (pura, con pruebas) ----

struct Entry {
    /// Primera y última línea (incluida) que ocupa la definición; varias si el valor es multilínea.
    first: usize,
    last: usize,
    key: String,
    exported: bool,
    empty: bool,
}

/// (nombre, exportada?, resto tras `=`) si la línea empieza una definición.
fn parse_def(line: &str) -> Option<(&str, bool, &str)> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let (exported, t) = match t.strip_prefix("export") {
        Some(r) if r.starts_with([' ', '\t']) => (true, r.trim_start()),
        _ => (false, t),
    };
    let (k, v) = t.split_once('=')?;
    let k = k.trim_end();
    (!k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some((k, exported, v))
}

/// Posición del cierre de `quote` en `s` (ya sin la comilla de apertura), respetando `\` en comillas dobles.
fn close_quote(s: &str, quote: char) -> Option<usize> {
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
        } else if c == '\\' && quote == '"' {
            esc = true;
        } else if c == quote {
            return Some(i);
        }
    }
    None
}

fn scan(lines: &[&str]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i].trim_end_matches(['\n', '\r']);
        let raw = if i == 0 { raw.trim_start_matches('\u{feff}') } else { raw };
        let Some((key, exported, rest)) = parse_def(raw) else {
            i += 1;
            continue;
        };
        let rest = rest.trim_start();
        let first = i;
        let mut empty = false;
        if let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let inner = &rest[1..];
            match close_quote(inner, q) {
                Some(end) => empty = inner[..end].is_empty(),
                None => {
                    // Valor multilínea: se consume hasta la línea que cierra la comilla. Si nunca
                    // cierra es un error del archivo: se trata como una sola línea, sin tragarse el resto.
                    let mut j = i + 1;
                    while j < lines.len() && close_quote(lines[j], q).is_none() {
                        j += 1;
                    }
                    if j < lines.len() {
                        i = j;
                    }
                }
            }
        } else {
            let v = rest.split(" #").next().unwrap_or("").trim();
            empty = v.is_empty() || v.starts_with('#');
        }
        out.push(Entry { first, last: i, key: key.to_string(), exported, empty });
        i += 1;
    }
    out
}

/// (nombre, vacía?) de cada variable, en orden.
pub fn keys_in(text: &str) -> Vec<(String, bool)> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    scan(&lines).into_iter().map(|e| (e.key, e.empty)).collect()
}

pub fn is_set(text: &str, key: &str) -> bool {
    keys_in(text).iter().any(|(k, empty)| k == key && !empty)
}

/// Comillas simples (literales para bash, dotenv y python-dotenv) salvo que el valor tenga `'`.
/// Entonces comillas dobles escapando solo `"` y `\`; un salto de línea va tal cual dentro de las
/// comillas (lo leen igual bash y los dotenv).
fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:@+,=%".contains(c));
    if plain {
        return value.to_string();
    }
    if !value.contains('\'') {
        return format!("'{value}'");
    }
    let mut s = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Reemplaza la variable en su sitio o la añade al final. Conserva el resto tal cual.
pub fn set_in(text: &str, key: &str, value: &str) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let entries: Vec<Entry> = scan(&lines).into_iter().filter(|e| e.key == key).collect();
    let new_line = format!("{key}={}", quote(value));
    let mut out = String::new();
    let mut i = 0;
    let mut done = false;
    while i < lines.len() {
        if let Some(e) = entries.iter().find(|e| e.first == i) {
            if !done {
                let prefix = if e.exported { "export " } else { "" };
                let had_eol = lines[e.last].ends_with('\n');
                out.push_str(&format!("{prefix}{new_line}{}", if had_eol { eol } else { "" }));
                done = true;
            }
            i = e.last + 1;
            continue;
        }
        out.push_str(lines[i]);
        i += 1;
    }
    if !done {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&new_line);
        out.push_str(eol);
    }
    out
}

pub fn unset_in(text: &str, key: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let entries: Vec<Entry> = scan(&lines).into_iter().filter(|e| e.key == key).collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(e) = entries.iter().find(|e| e.first == i) {
            i = e.last + 1;
            continue;
        }
        out.push_str(lines[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_replaces_and_keeps_the_rest() {
        let t = "# nota\nA=1\nexport B=2\n\nC=3";
        assert_eq!(set_in(t, "A", "uno"), "# nota\nA=uno\nexport B=2\n\nC=3");
        assert_eq!(set_in(t, "B", "x y"), "# nota\nA=1\nexport B='x y'\n\nC=3");
        assert_eq!(set_in(t, "D", "4"), "# nota\nA=1\nexport B=2\n\nC=3\nD=4\n");
        assert_eq!(set_in("", "K", "v"), "K=v\n");
    }

    #[test]
    fn duplicates_collapse_and_comments_are_ignored() {
        assert_eq!(set_in("A=1\n#A=9\nA=2\n", "A", "3"), "A=3\n#A=9\n");
        assert_eq!(keys_in("#A=1\nB=\nC=\"\"\nD=x\nE= # nada\n"), vec![("B".into(), true), ("C".into(), true), ("D".into(), false), ("E".into(), true)]);
    }

    #[test]
    fn quoting_is_literal_for_shells() {
        assert_eq!(quote("sk-abc.123"), "sk-abc.123");
        assert_eq!(quote("a b#c$d`e\\f\"g"), "'a b#c$d`e\\f\"g'");
        assert_eq!(quote("it's\"x\\y"), "\"it's\\\"x\\\\y\"");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn multiline_values_are_skipped_whole() {
        let t = "A=1\nPRIV=\"-----BEGIN\nKUpRKfFLfRYC9AIK=jbJT\n-----END\"\nB=2\n";
        assert_eq!(keys_in(t).iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["A", "PRIV", "B"]);
        assert_eq!(set_in(t, "PRIV", "x"), "A=1\nPRIV=x\nB=2\n");
        assert_eq!(unset_in(t, "PRIV"), "A=1\nB=2\n");
    }

    #[test]
    fn crlf_bom_and_export_lookalikes() {
        assert_eq!(set_in("A=1\r\n", "B", "2"), "A=1\r\nB=2\r\n");
        assert_eq!(set_in("A=1\r\nB=2\r\n", "A", "9"), "A=9\r\nB=2\r\n");
        assert_eq!(set_in("\u{feff}A=1\n", "A", "2"), "A=2\n");
        assert_eq!(set_in("exports=1\n", "exports", "x"), "exports=x\n");
    }

    #[test]
    fn unclosed_quote_does_not_swallow_the_file() {
        let t = "A=\"oops\nB=1\nC=2\n";
        assert_eq!(keys_in(t).iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["A", "B", "C"]);
        assert_eq!(set_in(t, "A", "x"), "A=x\nB=1\nC=2\n");
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
