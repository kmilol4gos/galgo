//! Servidor MCP por stdio; las solicitudes se procesan en orden salvo waits cancelables.
use galgo_mcp::handle_line;
use galgo_proto::{client, Request, Response};
use serde_json::{json, Value};
use std::{collections::HashMap, io::{self, BufRead, BufReader, Write}, net::Shutdown, os::unix::net::UnixStream, sync::{Arc, Mutex}, thread};

#[derive(Default)]
struct Call { cancelled: bool, stream: Option<UnixStream> }
type Calls = Arc<Mutex<HashMap<String, Arc<Mutex<Call>>>>>;

fn daemon_request(req: Request, call: &Arc<Mutex<Call>>) -> anyhow::Result<Response> {
    let mut stream = client::connect()?;
    {
        let mut state = call.lock().unwrap();
        if state.cancelled { anyhow::bail!("solicitud cancelada"); }
        state.stream = Some(stream.try_clone()?);
    }
    writeln!(stream, "{}", serde_json::to_string(&req)?)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() { anyhow::bail!("el daemon cerró la conexión sin responder"); }
    Ok(serde_json::from_str(&reply)?)
}

fn output(text: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn main() -> io::Result<()> {
    let calls: Calls = Arc::default();
    let mut threads = Vec::new();
    let mut input = io::stdin().lock();
    loop {
        let mut bytes = Vec::new();
        if input.read_until(b'\n', &mut bytes)? == 0 { break; }
        let line = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => { output(&json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"UTF-8 inválido"}}).to_string()); continue; }
        };
        let msg: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if msg.get("method").and_then(Value::as_str) == Some("notifications/cancelled") && msg.get("id").is_none() {
            if let Some(id) = msg.pointer("/params/requestId") {
                if let Some(call) = calls.lock().unwrap().get(&id.to_string()) {
                    let mut state = call.lock().unwrap();
                    state.cancelled = true;
                    if let Some(stream) = state.stream.take() { let _ = stream.shutdown(Shutdown::Both); }
                }
            }
            continue;
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
                let state = call.lock().unwrap();
                if !state.cancelled { if let Some(reply) = reply { output(&reply); } }
                all.remove(&id);
            }));
        } else if let Some(reply) = handle_line(&line, &|req| client::request(&req)) { output(&reply); }
    }
    for t in threads { let _ = t.join(); }
    Ok(())
}
