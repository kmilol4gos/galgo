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

/// Qué hacer con el worktree de tarea de un panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeAction {
    Diff,
    Accept,
    Discard,
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
        /// Workspace del panel (p. ej. "inventia/cotizacion"). Si falta, el daemon lo deduce del repo git
        /// del directorio de trabajo ("<repo>/<rama>").
        #[serde(default)]
        workspace: Option<String>,
        /// Rama nueva para un worktree git propio del panel (`git worktree add -b <rama>`), creado desde el repo
        /// de `cwd`. El panel arranca dentro de ese worktree; el workspace sale del repo original.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
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
    /// Pantalla con estilos y cursor, para clientes gráficos.
    Snapshot {
        pane: String,
        /// Líneas hacia atrás en el historial (0 = pantalla en vivo).
        #[serde(default)]
        offset: u16,
        /// Token de la vista que el cliente ya tiene (el `gen` de un snapshot anterior, que codifica la
        /// generación y el `offset`). Si la vista no ha cambiado, el daemon responde
        /// `{unchanged: true, gen}`; con otro `offset` el token no coincide y responde el snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
    /// Cambia el tamaño del PTY y de la pantalla.
    Resize {
        pane: String,
        rows: u16,
        cols: u16,
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
    /// Acción sobre el worktree de tarea de un panel: ver cambios, fusionarlo en el repo principal o descartarlo.
    /// `Accept` y `Discard` exigen `confirm` (tocan el repo del usuario).
    Worktree {
        pane: String,
        action: WorktreeAction,
        #[serde(default)]
        confirm: bool,
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
    /// `source` dice de dónde leer el uso (agente, sesión, transcript); ausente en clientes anteriores.
    Hook {
        pane: String,
        event: HookEvent,
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<HookSource>,
    },
    /// Uso que el agente informa directamente (statusline de Claude, extensiones): contexto, coste y cuotas.
    UsageReport {
        pane: String,
        agent: String,
        model: Option<String>,
        context_tokens: Option<u64>,
        context_window: Option<u64>,
        cost_usd: Option<f64>,
        #[serde(default)]
        limits: Vec<LimitReport>,
    },
    /// Uso vivo agregado de los paneles y cuotas por agente. `group_by`: workspace, agent o model.
    Usage {
        #[serde(default)]
        group_by: Option<String>,
    },
    /// Título de la tarea de un panel (vacío lo borra). Solo en memoria del daemon.
    SetTitle {
        pane: String,
        title: String,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

/// Tramo de texto contiguo con el mismo estilo. Los valores por defecto no se escriben: un tramo
/// sin estilo es solo `{"t": "..."}`. Al leer, lo que falte vale su valor por defecto.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub t: String,
    #[serde(default, skip_serializing_if = "Color::is_default")]
    pub fg: Color,
    #[serde(default, skip_serializing_if = "Color::is_default")]
    pub bg: Color,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Color {
    pub fn is_default(&self) -> bool {
        matches!(self, Color::Default)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Token de la vista: `generación << 16 | offset`. La generación cambia con cada escritura del panel y
    /// con cada `Resize`; el cliente lo devuelve tal cual en `since`.
    #[serde(default)]
    pub gen: u64,
    pub rows: u16,
    pub cols: u16,
    /// (fila, columna) del cursor.
    pub cursor: (u16, u16),
    pub cursor_hidden: bool,
    pub lines: Vec<Vec<Run>>,
    /// Desplazamiento efectivo en el historial (puede ser menor que el pedido).
    #[serde(default)]
    pub scrollback: u16,
    /// Líneas de historial disponibles.
    #[serde(default)]
    pub scrollback_max: u16,
    /// La aplicación del panel activó bracketed paste (modo 2004): el pegado debe envolverse.
    #[serde(default)]
    pub bracketed_paste: bool,
    /// Por fila: `true` si la línea continúa en la siguiente (soft wrap). Vacío en respuestas antiguas.
    #[serde(default)]
    pub wrapped: Vec<bool>,
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

/// De dónde sacar el uso de un panel: agente, sesión y ruta del transcript (solo en memoria del daemon).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookSource {
    pub agent: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript: Option<String>,
}

/// Una cuota informada por el agente. `name` es `five_hour` o `seven_day`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitReport {
    pub name: String,
    pub used_pct: f32,
    pub resets_at: Option<u64>,
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
    /// Workspace del panel. Ausente en daemons anteriores.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Directorio de trabajo del panel. Ausente en daemons anteriores.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Uso vivo del panel (tokens, contexto, coste). Ausente si el panel no tiene fuente de uso.
    #[serde(default)]
    pub usage: Option<galgo_usage::PaneUsage>,
    /// Título de la tarea que el agente declara (`galgo title`). Ausente si nunca lo declaró.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Un tramo sin estilo es solo su texto; los valores por defecto no se escriben, pero se leen.
    #[test]
    fn tramo_compacto() {
        let plain = Run { t: "x".into(), fg: Color::Default, bg: Color::Default, bold: false, italic: false, underline: false, inverse: false };
        assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"t":"x"}"#);
        let styled = Run { fg: Color::Idx(1), bold: true, ..plain.clone() };
        assert_eq!(serde_json::to_string(&styled).unwrap(), r#"{"t":"x","fg":{"Idx":1},"bold":true}"#);
        let back: Run = serde_json::from_str(r#"{"t":"x"}"#).unwrap();
        assert_eq!(back, plain);
    }

    /// `since` solo viaja cuando el cliente lo pide; los clientes viejos no lo envían.
    #[test]
    fn since_solo_si_se_pide() {
        let sin = Request::Snapshot { pane: "p".into(), offset: 0, since: None };
        assert!(!serde_json::to_string(&sin).unwrap().contains("since"));
        let con = Request::Snapshot { pane: "p".into(), offset: 0, since: Some(7) };
        assert!(serde_json::to_string(&con).unwrap().contains(r#""since":7"#));
        let viejo: Request = serde_json::from_str(r#"{"op":"snapshot","pane":"p"}"#).unwrap();
        assert!(matches!(viejo, Request::Snapshot { since: None, offset: 0, .. }));
    }
}

/// Agrupa los paneles por workspace, en orden alfabético; los que no tienen workspace van al final.
/// Dentro de cada grupo, el orden de la lista se conserva.
pub fn group_by_workspace(panes: &[PaneInfo]) -> Vec<(Option<String>, Vec<&PaneInfo>)> {
    let mut groups: Vec<(Option<String>, Vec<&PaneInfo>)> = Vec::new();
    for p in panes {
        match groups.iter_mut().find(|(w, _)| *w == p.workspace) {
            Some((_, list)) => list.push(p),
            None => groups.push((p.workspace.clone(), vec![p])),
        }
    }
    groups.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    groups
}

#[cfg(test)]
mod tests_grupos {
    use super::*;

    fn pane(id: u32, name: &str, ws: Option<&str>) -> PaneInfo {
        PaneInfo {
            id,
            name: name.into(),
            status: Status::Idle,
            manager: None,
            cmd: vec![],
            unacked: 0,
            oldest_unacked_secs: None,
            orphan: false,
            waiting_on: None,
            waiting_secs: None,
            workspace: ws.map(String::from),
            cwd: None,
            usage: None,
            title: None,
        }
    }

    #[test]
    fn agrupa_por_workspace_en_orden_y_deja_los_sin_workspace_al_final() {
        let panes = vec![
            pane(1, "a", Some("inventia/cotizacion")),
            pane(2, "b", None),
            pane(3, "c", Some("galgo/main")),
            pane(4, "d", Some("inventia/cotizacion")),
        ];
        let groups = group_by_workspace(&panes);
        let names: Vec<Option<&str>> = groups.iter().map(|(w, _)| w.as_deref()).collect();
        assert_eq!(names, vec![Some("galgo/main"), Some("inventia/cotizacion"), None]);
        // Dentro de un grupo se conserva el orden de la lista.
        let cot: Vec<u32> = groups[1].1.iter().map(|p| p.id).collect();
        assert_eq!(cot, vec![1, 4]);
    }

    #[test]
    fn lista_vacia_no_tiene_grupos() {
        assert!(group_by_workspace(&[]).is_empty());
    }
}
