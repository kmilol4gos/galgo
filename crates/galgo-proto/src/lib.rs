//! Protocolo entre el daemon (`galgod`) y sus clientes (CLI, MCP, GUI).
//!
//! Transporte: socket Unix, un JSON por línea. Cada solicitud recibe una respuesta.

pub mod client;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::PathBuf, str::FromStr};

/// Estado de un panel. Lo fijan hooks de agentes, la CLI o el propio daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    Failed,
    Exited,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::Blocked => "blocked",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Exited => "exited",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "idle" => Ok(Status::Idle),
            "working" | "busy" => Ok(Status::Working),
            "blocked" => Ok(Status::Blocked),
            "done" | "finished" => Ok(Status::Done),
            "failed" => Ok(Status::Failed),
            "exited" => Ok(Status::Exited),
            other => Err(format!(
                "estado desconocido: {other} (idle, working, blocked, done, failed, exited)"
            )),
        }
    }
}

/// Eventos que un agente informa a través de sus hooks. El daemon decide qué significan
/// según el panel (por ejemplo, terminar un turno en un worker equivale a reportar su resultado).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// El usuario envió un prompt: empieza un turno nuevo.
    PromptSubmitted,
    /// El agente empezó a trabajar (o volvió a hacerlo), por ejemplo tras una herramienta.
    Working,
    /// El agente espera al usuario sin haber terminado nada nuevo.
    Idle,
    /// El agente espera una decisión humana (permiso, pregunta).
    Blocked,
    /// El agente terminó su turno.
    TurnEnded,
    /// El turno terminó por un error (por ejemplo, de la API).
    Failed,
}

impl FromStr for HookEvent {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "prompt_submitted" => Ok(HookEvent::PromptSubmitted),
            "working" => Ok(HookEvent::Working),
            "idle" => Ok(HookEvent::Idle),
            "blocked" => Ok(HookEvent::Blocked),
            "turn_ended" => Ok(HookEvent::TurnEnded),
            "failed" => Ok(HookEvent::Failed),
            other => Err(format!("evento desconocido: {other}")),
        }
    }
}

/// Los paneles se referencian por id numérico o por nombre.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        name: String,
        cmd: Vec<String>,
        cwd: Option<String>,
        /// Panel que recibe los resultados de este panel.
        manager: Option<String>,
        /// Si es true, los mensajes pendientes se escriben en este panel cuando está `idle`.
        inject: bool,
        rows: u16,
        cols: u16,
    },
    List,
    Send {
        pane: String,
        text: String,
        enter: bool,
    },
    Read {
        pane: String,
    },
    SetStatus {
        pane: String,
        status: Status,
    },
    /// Un worker reporta su resultado; se entrega a su manager.
    Done {
        pane: String,
        summary: String,
        #[serde(default)]
        failed: bool,
    },
    /// Bloquea hasta que `target` reporte, termine o se agote el tiempo.
    Wait {
        waiter: String,
        target: String,
        timeout_secs: u64,
        /// El cliente confirma con `Ack` tras entregar el resultado; el daemon no confirma al escribir.
        #[serde(default)]
        client_ack: bool,
    },
    /// Evento de un hook de agente. `summary` es el último mensaje del agente o el motivo del bloqueo.
    Hook {
        pane: String,
        event: HookEvent,
        summary: Option<String>,
    },
    Inbox {
        pane: String,
    },
    Ack {
        pane: String,
        msg: u64,
    },
    Kill {
        pane: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub data: Value,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self { ok: true, error: None, data }
    }
    pub fn err(e: impl ToString) -> Self {
        Self { ok: false, error: Some(e.to_string()), data: Value::Null }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u32,
    pub name: String,
    pub status: Status,
    pub manager: Option<String>,
    pub cmd: Vec<String>,
    /// Mensajes entregados al panel que nadie ha confirmado.
    pub unacked: usize,
    pub oldest_unacked_secs: Option<u64>,
    /// true si hay mensajes sin confirmar más viejos que el umbral: el flujo quedó a medias.
    pub orphan: bool,
    pub waiting_on: Option<String>,
    pub waiting_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub from: u32,
    pub from_name: String,
    /// "done", "failed" o "exited".
    pub kind: String,
    pub summary: String,
    pub age_secs: u64,
    pub acked: bool,
}

/// Ruta del socket: `$GALGO_SOCKET` o `~/.galgo/galgo.sock`.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("GALGO_SOCKET") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".galgo").join("galgo.sock")
}
