//! Servidor MCP por stdio; las solicitudes se procesan en orden salvo waits cancelables.
use galgo_mcp::handle_line;
use galgo_proto::{client, Request, Response};
use serde_json::{json, Value};
use std::{collections::HashMap, io::{self, BufRead, Read, Write}, net::Shutdown, os::unix::net::UnixStream, sync::{Arc, Mutex}, thread, time::Duration};

#[derive(Default)]
struct Call { cancelled: bool, response_received: bool, stream: Option<UnixStream>, pending_ack: Option<(String, u64)> }
type Calls = Arc<Mutex<HashMap<String, Arc<Mutex<Call>>>>>;

fn daemon_request(req: Request, call: &Arc<Mutex<Call>>) -> anyhow::Result<Response> {
    let mut stream = client::connect()?;
    {
        let mut state = call.lock().unwrap();
        if state.cancelled { anyhow::bail!("solicitud cancelada"); }
        state.stream = Some(stream.try_clone()?);
    }
    writeln!(stream, "{}", serde_json::to_string(&req)?)?;
    stream.set_nonblocking(true)?;
    let mut reply = Vec::new();
    loop {
        let mut state = call.lock().unwrap();
        if state.cancelled { anyhow::bail!("solicitud cancelada"); }
        let mut buf = [0u8; 8192];
        match stream.read(&mut buf) {
            Ok(0) => anyhow::bail!("el daemon cerró la conexión sin responder"),
            Ok(n) => {
                reply.extend_from_slice(&buf[..n]);
                if let Some(end) = reply.iter().position(|b| *b == b'\n') {
                    let response: Response = serde_json::from_slice(&reply[..end])?;
                    // Tras leer una respuesta completa, siempre la imprimimos: el daemon
                    // puede haberla confirmado. Esto no elimina la carrera anterior a la lectura.
                    state.response_received = true;
                    if let (Some(msg), Some(pane)) = (response.data["msg_id"].as_u64(), response.data["pane_id"].as_u64()) {
                        state.pending_ack = Some((pane.to_string(), msg));
                    }
                    return Ok(response);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                drop(state);
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {},
            Err(e) => return Err(e.into()),
        }
    }
}

fn output(text: &str) -> bool {
    let mut out = io::stdout().lock();
    writeln!(out, "{text}").and_then(|_| out.flush()).is_ok()
}

fn cancel(call: &Arc<Mutex<Call>>) {
    let mut state = call.lock().unwrap();
    if !state.response_received {
        state.cancelled = true;
        if let Some(stream) = state.stream.take() { let _ = stream.shutdown(Shutdown::Both); }
    }
}

fn reap(threads: &mut Vec<thread::JoinHandle<()>>) {
    let mut i = 0;
    while i < threads.len() {
        if threads[i].is_finished() { let _ = threads.swap_remove(i).join(); } else { i += 1; }
    }
}

fn main() -> io::Result<()> {
    let calls: Calls = Arc::default();
    let mut threads = Vec::new();
    let mut input = io::stdin().lock();
    loop {
        let mut bytes = Vec::new();
        if input.read_until(b'\n', &mut bytes)? == 0 { break; }
        reap(&mut threads);
        let line = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => { output(&json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"UTF-8 inválido"}}).to_string()); continue; }
        };
        let msg: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if msg.get("method").and_then(Value::as_str) == Some("notifications/cancelled") && msg.get("id").is_none() {
            if let Some(id) = msg.pointer("/params/requestId") {
                if let Some(call) = calls.lock().unwrap().get(&id.to_string()) {
                    cancel(call);
                }
            }
            continue;
        }
        if let Some(id) = msg.get("id") {
            if calls.lock().unwrap().contains_key(&id.to_string()) {
                output(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"id ya en curso"}}).to_string());
                continue;
            }
        }
        if msg.get("method").and_then(Value::as_str) == Some("tools/call")
            && msg.pointer("/params/name").and_then(Value::as_str) == Some("galgo_wait")
            && msg.get("id").is_some() {
            let id = msg["id"].to_string();
            let call = Arc::new(Mutex::new(Call::default()));
            calls.lock().unwrap().insert(id.clone(), call.clone());
            let calls = calls.clone();
            threads.push(thread::spawn(move || {
                let reply = handle_line(&line, &|req| daemon_request(req, &call));
                let mut all = calls.lock().unwrap();
                let mut state = call.lock().unwrap();
                let printed = !state.cancelled && reply.as_deref().is_some_and(output);
                // Se confirma solo tras imprimir; una espera cancelada deja el mensaje pendiente.
                let ack = state.pending_ack.take().filter(|_| printed);
                all.remove(&id);
                drop(state);
                drop(all);
                if let Some((pane, msg)) = ack { let _ = client::request(&Request::Ack { pane, msg }); }
            }));
        } else if let Some(reply) = handle_line(&line, &|req| client::request(&req)) { output(&reply); }
    }
    for call in calls.lock().unwrap().values() { cancel(call); }
    for t in threads { let _ = t.join(); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_wait_threads_are_reaped() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut threads = vec![thread::spawn(move || { tx.send(()).unwrap(); })];
        rx.recv().unwrap();
        for _ in 0..100 {
            reap(&mut threads);
            if threads.is_empty() { break; }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(threads.is_empty());
    }

    #[test]
    fn completed_response_is_not_cancelled() {
        let call = Arc::new(Mutex::new(Call { response_received: true, ..Call::default() }));
        cancel(&call);
        assert!(!call.lock().unwrap().cancelled);
    }
}
