//! Cliente del socket del daemon. Lo usan la CLI (`galgo`) y el servidor MCP (`galgo-mcp`).

use crate::{socket_path, Request, Response};
use anyhow::{bail, Context, Result};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::{OpenOptionsExt, PermissionsExt}, net::UnixStream, process::CommandExt},
    process::{Command, Stdio},
    time::Duration,
};

/// Conecta al daemon. Si no está corriendo, arranca `galgod` (hermano del ejecutable actual)
/// y espera a que acepte conexiones.
pub fn connect() -> Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let log_path = dir.join("galgod.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(&log_path)
        .with_context(|| format!("no pude abrir {}", log_path.display()))?;
    log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let galgod = std::env::current_exe()?.with_file_name("galgod");
    let mut cmd = Command::new(&galgod);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(log));
    // Sesión propia: un daemon arrancado desde un panel no debe morir con el `kill` de ese panel.
    // SAFETY: setsid es seguro entre fork y exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("no pude iniciar {}", galgod.display()))?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
        if child.try_wait()?.is_some() { break; }
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let tail = log.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    bail!("el daemon no respondió en {}; log {}:\n{}", path.display(), log_path.display(), tail)
}

/// Envía una solicitud y espera su respuesta (una línea JSON por sentido).
pub fn request(req: &Request) -> Result<Response> {
    let mut stream = connect()?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        bail!("el daemon cerró la conexión sin responder");
    }
    Ok(serde_json::from_str(&reply)?)
}
