//! Cliente del socket del daemon. Lo usan la CLI (`galgo`) y el servidor MCP (`galgo-mcp`).

use crate::{socket_path, Request, Response};
use anyhow::{bail, Context, Result};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixStream, process::CommandExt},
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
    let galgod = std::env::current_exe()?.with_file_name("galgod");
    Command::new(&galgod)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("no pude iniciar {}", galgod.display()))?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
    }
    bail!("el daemon no respondió en {}", path.display())
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
