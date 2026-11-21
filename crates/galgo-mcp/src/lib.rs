//! Servidor MCP por stdio para galgo: JSON-RPC 2.0, un mensaje JSON por línea, sin SDK externo.
//!
//! Cada herramienta delega en el daemon (`galgo_proto::client`). `handle_line` recibe el daemon
//! como función, así que las pruebas no necesitan socket.

use anyhow::Result;
use galgo_proto::{Request, Response};
use serde_json::{json, Value};
use std::fmt;

/// Versión del protocolo si el cliente no envía `protocolVersion`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Tiempo por defecto de `galgo_wait`, igual que `galgo wait`.
const DEFAULT_WAIT_SECS: u64 = 600;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Lo que el servidor toma de su proceso: el panel propio (`GALGO_PANE`) y el directorio de trabajo.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub pane: Option<String>,
    pub cwd: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            pane: std::env::var("GALGO_PANE").ok().filter(|s| !s.is_empty()),
            cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        }
    }
}

/// Procesa una línea de entrada con el entorno del proceso.
/// Devuelve la línea de respuesta, o `None` si no hay que responder (notificaciones, líneas vacías).
pub fn handle_line(line: &str, daemon: &dyn Fn(Request) -> Result<Response>) -> Option<String> {
    handle_line_in(line, daemon, &Env::from_process())
}

/// Igual que `handle_line`, pero con el entorno explícito. Es la parte determinista.
pub fn handle_line_in(
    line: &str,
    daemon: &dyn Fn(Request) -> Result<Response>,
    env: &Env,
) -> Option<String> {
    let text = line.trim();
    if text.is_empty() {
        return None;
    }
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Some(rpc_error(Value::Null, PARSE_ERROR, &format!("JSON inválido: {e}"))),
    };
    let Some(obj) = msg.as_object() else {
        return Some(rpc_error(Value::Null, INVALID_REQUEST, "el mensaje debe ser un objeto JSON"));
    };
    // Sin `id` es una notificación: nunca se responde, sea cual sea el método.
    let id = obj.get("id")?.clone();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(id, INVALID_REQUEST, "falta 'method'"));
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    let outcome = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool_rpc(&params, daemon, env),
        other => Err(RpcError::new(METHOD_NOT_FOUND, format!("método desconocido: {other}"))),
    };
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        Err(e) => rpc_error(id, e.code, &e.message),
    })
}

/// Error JSON-RPC a nivel de protocolo (no de herramienta).
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "galgo", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// `tools/call`. Herramienta desconocida o falta de `name`: error JSON-RPC.
/// Cualquier fallo al ejecutarla (uso, daemon, transporte): resultado con `isError: true`.
fn call_tool_rpc(params: &Value, daemon: &dyn Fn(Request) -> Result<Response>, env: &Env) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "tools/call requiere 'name'"))?;
    if !tool_definitions().iter().any(|t| t["name"] == name) {
        return Err(RpcError::new(INVALID_PARAMS, format!("herramienta desconocida: {name}")));
    }
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok(match run_tool(name, &args, daemon, env) {
        Ok(text) => tool_result(&text, false),
        Err(message) => tool_result(&message, true),
    })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Ejecuta una herramienta. `Ok` es el JSON de la respuesta del daemon; `Err`, el mensaje de error.
fn run_tool(name: &str, args: &Value, daemon: &dyn Fn(Request) -> Result<Response>, env: &Env) -> Result<String, String> {
    let req = build_request(name, args, env)?;
    let resp = daemon(req).map_err(|e| format!("{e:#}"))?;
    if !resp.ok {
        return Err(resp.error.unwrap_or_else(|| "error desconocido del daemon".into()));
    }
    Ok(resp.data.to_string())
}

fn build_request(name: &str, args: &Value, env: &Env) -> Result<Request, String> {
    match name {
        "galgo_list" => Ok(Request::List),
        "galgo_spawn" => Ok(Request::Spawn {
            name: str_arg(args, "name")?,
            cmd: cmd_arg(args)?,
            cwd: opt_str(args, "cwd").or_else(|| env.cwd.clone()),
            manager: opt_str(args, "manager"),
            inject: false,
            rows: 40,
            cols: 140,
        }),
        "galgo_send" => Ok(Request::Send {
            pane: str_arg(args, "pane")?,
            text: str_arg(args, "text")?,
            enter: opt_bool(args, "enter").unwrap_or(true),
        }),
        "galgo_read" => Ok(Request::Read { pane: str_arg(args, "pane")? }),
        "galgo_wait" => Ok(Request::Wait {
            waiter: own_pane(env)?,
            target: str_arg(args, "target")?,
            timeout_secs: opt_u64(args, "timeout_secs").unwrap_or(DEFAULT_WAIT_SECS),
            client_ack: true,
        }),
        "galgo_done" => Ok(Request::Done {
            pane: own_pane(env)?,
            summary: str_arg(args, "summary")?,
            failed: opt_bool(args, "failed").unwrap_or(false),
        }),
        "galgo_inbox" => Ok(Request::Inbox { pane: own_pane(env)? }),
        "galgo_ack" => Ok(Request::Ack { pane: own_pane(env)?, msg: u64_arg(args, "msg")? }),
        other => Err(format!("herramienta desconocida: {other}")),
    }
}

fn own_pane(env: &Env) -> Result<String, String> {
    env.pane
        .clone()
        .ok_or_else(|| "GALGO_PANE no está definido: esta herramienta solo funciona dentro de un panel de galgo".to_string())
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    opt_str(args, key).ok_or_else(|| format!("falta el argumento '{key}' (texto)"))
}

fn u64_arg(args: &Value, key: &str) -> Result<u64, String> {
    opt_u64(args, key).ok_or_else(|| format!("falta el argumento '{key}' (entero)"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn cmd_arg(args: &Value) -> Result<Vec<String>, String> {
    let items = args
        .get("cmd")
        .and_then(Value::as_array)
        .ok_or_else(|| "falta el argumento 'cmd' (lista de textos)".to_string())?;
    let cmd: Vec<String> = items
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or_else(|| "'cmd' debe ser una lista de textos".to_string())?;
    if cmd.is_empty() {
        return Err("'cmd' no puede estar vacío".into());
    }
    Ok(cmd)
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

/// Definiciones que devuelve `tools/list`.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "galgo_list",
            "Lista los paneles con su estado, manager y entregas pendientes.",
            json!({}),
            &[],
        ),
        tool(
            "galgo_spawn",
            "Crea un panel que ejecuta un comando, p. ej. [\"claude\"]. Devuelve el panel creado.",
            json!({
                "name": { "type": "string", "description": "Nombre único del panel." },
                "cmd": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Comando y argumentos." },
                "manager": { "type": "string", "description": "Panel que recibe el resultado de este." },
                "cwd": { "type": "string", "description": "Directorio de trabajo. Por defecto, el del servidor MCP." },
            }),
            &["name", "cmd"],
        ),
        tool(
            "galgo_send",
            "Escribe texto en un panel. Pulsa Enter al final salvo que enter sea false.",
            json!({
                "pane": { "type": "string", "description": "Panel destino: id o nombre." },
                "text": { "type": "string" },
                "enter": { "type": "boolean", "default": true },
            }),
            &["pane", "text"],
        ),
        tool(
            "galgo_read",
            "Lee la pantalla de un panel.",
            json!({ "pane": { "type": "string", "description": "Panel: id o nombre." } }),
            &["pane"],
        ),
        tool(
            "galgo_wait",
            "Espera a que otro panel reporte, termine o se agote el tiempo. Devuelve su resultado. Espera en GALGO_PANE.",
            json!({
                "target": { "type": "string", "description": "Panel por el que esperar: id o nombre." },
                "timeout_secs": { "type": "integer", "default": DEFAULT_WAIT_SECS },
            }),
            &["target"],
        ),
        tool(
            "galgo_done",
            "Reporta el resultado de este panel (GALGO_PANE) y lo entrega a su manager.",
            json!({
                "summary": { "type": "string", "description": "Resultado en pocas palabras." },
                "failed": { "type": "boolean", "default": false },
            }),
            &["summary"],
        ),
        tool(
            "galgo_inbox",
            "Lista los mensajes pendientes de confirmar en este panel (GALGO_PANE).",
            json!({}),
            &[],
        ),
        tool(
            "galgo_ack",
            "Confirma un mensaje del inbox de este panel (GALGO_PANE).",
            json!({ "msg": { "type": "integer", "description": "Id del mensaje, como lo muestra galgo_inbox." } }),
            &["msg"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Daemon que no debe usarse: las pruebas que lo pasan no tocan el socket.
    fn no_daemon(_: Request) -> Result<Response> {
        panic!("no debería llamarse al daemon");
    }

    fn env(pane: Option<&str>) -> Env {
        Env { pane: pane.map(String::from), cwd: Some("/proyecto".into()) }
    }

    /// Llama a `handle_line_in` y parsea la respuesta, que debe existir.
    fn call(line: &str, daemon: &dyn Fn(Request) -> Result<Response>, pane: Option<&str>) -> Value {
        let out = handle_line_in(line, daemon, &env(pane)).expect("debería responder");
        serde_json::from_str(&out).expect("la respuesta es JSON")
    }

    fn call_tool_line(name: &str, args: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": { "name": name, "arguments": args } })
        .to_string()
    }

    #[test]
    fn initialize_devuelve_la_version_pedida() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
        let r = call(line, &no_daemon, None);
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(r["result"]["serverInfo"]["name"], "galgo");
        assert_eq!(r["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn initialize_negocia_versiones_soportadas() {
        for version in SUPPORTED_VERSIONS {
            assert_eq!(initialize_result(&json!({"protocolVersion": version}))["protocolVersion"], *version);
        }
        assert_eq!(initialize_result(&json!({"protocolVersion": "2099-01-01"}))["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_sin_version_usa_la_por_defecto() {
        let r = call(r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#, &no_daemon, None);
        assert_eq!(r["id"], "a");
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_expone_las_ocho_herramientas() {
        let r = call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &no_daemon, None);
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["galgo_list", "galgo_spawn", "galgo_send", "galgo_read", "galgo_wait", "galgo_done", "galgo_inbox", "galgo_ack"]
        );
        for t in tools {
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn ping_responde_objeto_vacio() {
        let r = call(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#, &no_daemon, None);
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn tools_call_ok_reenvia_al_daemon_y_devuelve_json_como_texto() {
        let seen = RefCell::new(None);
        let daemon = |req: Request| {
            *seen.borrow_mut() = Some(req);
            Ok(Response::ok(json!({ "text": "hola" })))
        };
        let r = call(&call_tool_line("galgo_read", json!({ "pane": "w1" })), &daemon, None);
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["content"][0]["type"], "text");
        let text: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, json!({ "text": "hola" }));
        assert!(matches!(seen.borrow().as_ref(), Some(Request::Read { pane }) if pane == "w1"));
    }

    #[test]
    fn tools_call_con_error_del_daemon_es_is_error() {
        let daemon = |_: Request| Ok(Response::err("tiempo agotado esperando al panel"));
        let r = call(&call_tool_line("galgo_wait", json!({ "target": "w3" })), &daemon, Some("mgr"));
        assert!(r.get("error").is_none(), "no debe ser error JSON-RPC");
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "tiempo agotado esperando al panel");
    }

    #[test]
    fn tools_call_con_fallo_de_transporte_es_is_error() {
        let daemon = |_: Request| -> Result<Response> { Err(anyhow::anyhow!("no pude conectar")) };
        let r = call(&call_tool_line("galgo_list", json!({})), &daemon, None);
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "no pude conectar");
    }

    #[test]
    fn herramienta_desconocida_es_error_jsonrpc_32602() {
        let r = call(&call_tool_line("galgo_nada", json!({})), &no_daemon, None);
        assert_eq!(r["error"]["code"], -32602);
        assert!(r.get("result").is_none());
    }

    #[test]
    fn metodo_desconocido_es_error_jsonrpc_32601() {
        let r = call(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#, &no_daemon, None);
        assert_eq!(r["id"], 4);
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn json_invalido_es_error_jsonrpc_32700_con_id_nulo() {
        let r = call("{no es json", &no_daemon, None);
        assert_eq!(r["error"]["code"], -32700);
        assert_eq!(r["id"], Value::Null);
    }

    #[test]
    fn notificaciones_no_reciben_respuesta() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            r#"{"jsonrpc":"2.0","method":"metodo/inventado"}"#,
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"galgo_nada"}}"#,
            "   ",
            "",
        ] {
            assert_eq!(handle_line_in(line, &no_daemon, &env(None)), None, "línea: {line}");
        }
    }

    #[test]
    fn la_respuesta_es_una_sola_linea() {
        let daemon = |_: Request| Ok(Response::ok(json!({ "text": "uno\ndos" })));
        let out = handle_line_in(&call_tool_line("galgo_read", json!({ "pane": "w1" })), &daemon, &env(None)).unwrap();
        assert!(!out.contains('\n'));
    }

    #[test]
    fn spawn_usa_el_cwd_del_servidor_si_no_se_indica() {
        let seen = RefCell::new(None);
        let daemon = |req: Request| {
            *seen.borrow_mut() = Some(req);
            Ok(Response::ok(json!({})))
        };
        let args = json!({ "name": "w1", "cmd": ["bash", "--norc"], "manager": "mgr" });
        let r = call(&call_tool_line("galgo_spawn", args), &daemon, None);
        assert_eq!(r["result"]["isError"], false);
        let req = seen.borrow().clone();
        match req.as_ref() {
            Some(Request::Spawn { name, cmd, cwd, manager, .. }) => {
                assert_eq!(name, "w1");
                assert_eq!(cmd, &["bash", "--norc"]);
                assert_eq!(cwd.as_deref(), Some("/proyecto"));
                assert_eq!(manager.as_deref(), Some("mgr"));
            }
            other => panic!("petición inesperada: {other:?}"),
        }
    }

    #[test]
    fn send_pulsa_enter_por_defecto() {
        let seen = RefCell::new(None);
        let daemon = |req: Request| {
            *seen.borrow_mut() = Some(req);
            Ok(Response::ok(json!({})))
        };
        call(&call_tool_line("galgo_send", json!({ "pane": "w1", "text": "ls" })), &daemon, None);
        assert!(matches!(seen.borrow().as_ref(), Some(Request::Send { enter: true, .. })));
    }

    #[test]
    fn wait_y_done_usan_el_panel_propio_y_los_valores_por_defecto() {
        let seen = RefCell::new(Vec::new());
        let daemon = |req: Request| {
            seen.borrow_mut().push(req);
            Ok(Response::ok(json!({})))
        };
        call(&call_tool_line("galgo_wait", json!({ "target": "w1" })), &daemon, Some("mgr"));
        call(&call_tool_line("galgo_done", json!({ "summary": "listo" })), &daemon, Some("w1"));
        let seen = seen.borrow();
        assert!(matches!(&seen[0], Request::Wait { waiter, target, timeout_secs: 600, client_ack: true } if waiter == "mgr" && target == "w1"));
        assert!(matches!(&seen[1], Request::Done { pane, failed: false, .. } if pane == "w1"));
    }

    #[test]
    fn herramientas_de_panel_sin_galgo_pane_son_is_error() {
        for (name, args) in [
            ("galgo_wait", json!({ "target": "w1" })),
            ("galgo_done", json!({ "summary": "x" })),
            ("galgo_inbox", json!({})),
            ("galgo_ack", json!({ "msg": 1 })),
        ] {
            let r = call(&call_tool_line(name, args), &no_daemon, None);
            assert_eq!(r["result"]["isError"], true, "{name}");
            assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("GALGO_PANE"), "{name}");
        }
    }

    #[test]
    fn argumentos_que_faltan_son_is_error_no_error_jsonrpc() {
        let r = call(&call_tool_line("galgo_send", json!({ "pane": "w1" })), &no_daemon, None);
        assert!(r.get("error").is_none());
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("text"));

        let r = call(&call_tool_line("galgo_spawn", json!({ "name": "w", "cmd": [] })), &no_daemon, None);
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn tools_call_sin_name_es_error_jsonrpc() {
        let r = call(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}"#, &no_daemon, None);
        assert_eq!(r["error"]["code"], -32602);
    }
}
