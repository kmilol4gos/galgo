//! galgo: CLI para controlar paneles y agentes a través del daemon.

mod claude;
mod codex;
mod env;
mod pi;
mod skill;
mod statusline;
mod usage_cmd;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use galgo_proto::{client::request, socket_path, Message, PaneInfo, Request, Status};
use serde_json::Value;
use std::os::unix::net::UnixStream;

#[derive(Parser)]
#[command(name = "galgo", version, about = "Controla paneles y agentes")]
struct Cli {
    /// Imprime la respuesta en JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Crea un panel que ejecuta un comando: galgo spawn nombre -- claude
    Spawn {
        name: String,
        /// Panel que recibe el resultado de este.
        #[arg(long)]
        manager: Option<String>,
        /// Escribe los mensajes pendientes en este panel cuando esté idle.
        #[arg(long)]
        inject: bool,
        #[arg(long)]
        cwd: Option<String>,
        /// Workspace del panel, p. ej. "inventia/cotizacion". Si falta, se deduce del repo git de --cwd.
        #[arg(long)]
        workspace: Option<String>,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Lista paneles. Marca los que tienen entregas sin confirmar.
    /// Lista paneles agrupados por workspace. Con --workspace, solo los de ese workspace.
    Ls {
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Escribe texto en un panel.
    Send {
        pane: String,
        text: String,
        #[arg(long)]
        no_enter: bool,
    },
    /// Lee la pantalla de un panel.
    Read { pane: String },
    /// Fija el estado del panel (lo usan los hooks de los agentes).
    Status {
        status: Status,
        #[arg(long, env = "GALGO_PANE")]
        pane: String,
    },
    /// Reporta el resultado y lo entrega al manager.
    Done {
        #[arg(long)]
        summary: String,
        #[arg(long)]
        failed: bool,
        #[arg(long, env = "GALGO_PANE")]
        pane: String,
    },
    /// Espera a que otro panel termine. Devuelve su resultado.
    Wait {
        target: String,
        #[arg(long, env = "GALGO_PANE")]
        pane: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Mensajes pendientes de confirmar.
    Inbox {
        #[arg(long, env = "GALGO_PANE")]
        pane: String,
    },
    /// Confirma un mensaje.
    Ack {
        msg: u64,
        #[arg(long, env = "GALGO_PANE")]
        pane: String,
    },
    /// Termina el proceso de un panel.
    Kill { pane: String },
    /// Detiene el daemon.
    Stop,
    /// Lo invoca el agente en cada evento (lee el JSON del evento por stdin). No imprime nada.
    Hook { agent: claude::Agent },
    /// Statusline de Claude con métricas y comando previo encadenado.
    Statusline {
        agent: claude::Agent,
        #[arg(long)]
        backup: Option<std::path::PathBuf>,
    },
    /// Variables de entorno (.env) sin abrir un IDE; los valores no pasan por el daemon.
    Env {
        #[command(subcommand)]
        action: env::EnvCmd,
    },
    /// Instala, desinstala o consulta la skill de un agente.
    Skill {
        #[command(subcommand)]
        action: skill::SkillCmd,
    },
    /// Instala o quita los hooks de un agente.
    Hooks {
        #[command(subcommand)]
        action: claude::HooksCmd,
    },
    /// Uso vivo de los paneles (tokens, contexto, coste) y cuotas por agente.
    Usage(usage_cmd::UsageArgs),
}

fn main() {
    if let Err(e) = run() {
        eprintln!("galgo: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Hook { agent } => std::process::exit(claude::run_hook(*agent)),
        Cmd::Statusline { agent: claude::Agent::Claude, backup } => std::process::exit(statusline::run(backup.as_deref())),
        Cmd::Statusline { .. } => bail!("statusline solo está disponible para Claude"),
        Cmd::Hooks { action } => return claude::hooks_command(action),
        Cmd::Skill { action } => return skill::run(action),
        Cmd::Env { action } => return env::run(action),
        Cmd::Usage(args) => return usage_cmd::run(args, cli.json),
        _ => {}
    }
    let req = match &cli.cmd {
        Cmd::Spawn { name, manager, inject, cwd, workspace, cmd } => Request::Spawn {
            workspace: workspace.clone(),
            name: name.clone(),
            cmd: cmd.clone(),
            cwd: {
                let base = std::env::current_dir()?;
                let path = cwd.as_ref().map_or(base.clone(), |p| base.join(p));
                Some(path.to_string_lossy().into_owned())
            },
            manager: manager.clone(),
            inject: *inject,
            rows: 40,
            cols: 140,
        },
        Cmd::Ls { .. } => Request::List,
        Cmd::Send { pane, text, no_enter } => {
            Request::Send { pane: pane.clone(), text: text.clone(), enter: !no_enter }
        }
        Cmd::Read { pane } => Request::Read { pane: pane.clone() },
        Cmd::Status { status, pane } => Request::SetStatus { pane: pane.clone(), status: *status },
        Cmd::Done { summary, failed, pane } => {
            Request::Done { pane: pane.clone(), summary: summary.clone(), failed: *failed }
        }
        Cmd::Wait { target, pane, timeout } => Request::Wait {
            waiter: pane.clone(),
            target: target.clone(),
            timeout_secs: *timeout,
            client_ack: true,
        },
        Cmd::Inbox { pane } => Request::Inbox { pane: pane.clone() },
        Cmd::Ack { msg, pane } => Request::Ack { pane: pane.clone(), msg: *msg },
        Cmd::Kill { pane } => Request::Kill { pane: pane.clone() },
        Cmd::Stop => Request::Shutdown,
        Cmd::Hook { .. } | Cmd::Statusline { .. } | Cmd::Hooks { .. } | Cmd::Env { .. } | Cmd::Skill { .. } | Cmd::Usage(_) => unreachable!("se atienden antes"),
    };

    // `stop` no necesita arrancar un daemon que no existe.
    if matches!(cli.cmd, Cmd::Stop) && UnixStream::connect(socket_path()).is_err() {
        println!("no hay daemon en ejecución");
        return Ok(());
    }

    let resp = request(&req)?;
    if !resp.ok {
        bail!("{}", resp.error.unwrap_or_else(|| "error desconocido".into()));
    }
    let ack = match (&cli.cmd, resp.data["msg_id"].as_u64(), resp.data["pane_id"].as_u64()) {
        (Cmd::Wait { .. }, Some(msg), Some(pane)) => Some((pane, msg)),
        _ => None,
    };
    // `ls --workspace W` filtra la respuesta antes de imprimirla, igual en --json que en texto.
    let data = match &cli.cmd {
        Cmd::Ls { workspace: Some(w) } => filter_workspace(resp.data, w),
        _ => resp.data,
    };
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else {
        print_human(&cli.cmd, data)?;
    }
    // Se confirma después de imprimir: si el proceso muere antes, el mensaje sigue pendiente.
    if let Some((pane, msg)) = ack {
        // Si la salida está rota no se confirma: el resultado no llegó a nadie.
        if std::io::Write::flush(&mut std::io::stdout()).is_ok() {
            let _ = request(&Request::Ack { pane: pane.to_string(), msg });
        }
    }
    Ok(())
}

/// Paneles de un workspace concreto. Una respuesta que no es una lista se devuelve tal cual.
fn filter_workspace(data: Value, workspace: &str) -> Value {
    match data {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .filter(|p| p["workspace"].as_str() == Some(workspace))
                .collect(),
        ),
        other => other,
    }
}

/// Encabezado de un grupo: nombre del workspace y resumen de atención de sus paneles.
/// Texto seguro para la terminal: los caracteres de control se muestran como «?» (B2). Un nombre o un
/// workspace no puede cambiar colores ni fabricar líneas en la salida.
fn visible(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

fn workspace_header(workspace: Option<&str>, group: &[&PaneInfo]) -> String {
    let blocked = group.iter().filter(|p| p.status == Status::Blocked).count();
    let pending = group.iter().filter(|p| p.unacked > 0 || p.orphan).count();
    let name = visible(workspace.unwrap_or("sin workspace"));
    let mut notes = Vec::new();
    if blocked > 0 {
        notes.push(format!("{blocked} bloqueado(s)"));
    }
    if pending > 0 {
        notes.push(format!("{pending} con entregas pendientes"));
    }
    if notes.is_empty() {
        format!("── {name} ({})", group.len())
    } else {
        format!("── {name} ({}) · {}", group.len(), notes.join(" · "))
    }
}

fn print_pane(p: &PaneInfo) {
    let mut notes = Vec::new();
    if let Some(m) = &p.manager {
        notes.push(format!("reporta a {}", visible(m)));
    }
    if let (Some(t), Some(s)) = (&p.waiting_on, p.waiting_secs) {
        notes.push(format!("espera a {} hace {s}s", visible(t)));
    }
    if p.orphan {
        notes.push(format!(
            "ENTREGA SIN CONFIRMAR hace {}s ({} mensaje(s))",
            p.oldest_unacked_secs.unwrap_or(0),
            p.unacked
        ));
    } else if p.unacked > 0 {
        notes.push(format!("{} mensaje(s) pendiente(s)", p.unacked));
    }
    println!("{:>3}  {:<20} {:<8} {}", p.id, visible(&p.name), p.status, notes.join(" · "));
}

fn print_human(cmd: &Cmd, data: Value) -> Result<()> {
    match cmd {
        Cmd::Spawn { .. } => {
            let heredados = data["inherited"].as_u64().unwrap_or(0);
            let p: PaneInfo = serde_json::from_value(data)?;
            match &p.workspace {
                Some(w) => println!("panel {} ({}) creado en {}", p.id, visible(&p.name), visible(w)),
                None => println!("panel {} ({}) creado", p.id, visible(&p.name)),
            }
            if heredados > 0 {
                println!(
                    "  {heredados} mensaje(s) de la ejecución anterior de {} llegaron a su inbox; no se escriben en el terminal (galgo inbox --pane {})",
                    visible(&p.name),
                    visible(&p.name)
                );
            }
        }
        Cmd::Ls { .. } => {
            let panes: Vec<PaneInfo> = serde_json::from_value(data)?;
            if panes.is_empty() {
                println!("sin paneles");
            }
            for (ws, group) in galgo_proto::group_by_workspace(&panes) {
                println!("{}", workspace_header(ws.as_deref(), &group));
                for p in group {
                    print_pane(p);
                }
            }
        }
        Cmd::Read { .. } => println!("{}", data["text"].as_str().unwrap_or("")),
        Cmd::Wait { .. } => {
            let from = data["from"].as_str().unwrap_or("?");
            match data["summary"].as_str() {
                Some(s) => println!("{from}: {s}"),
                None => println!("{from}: {}", data["status"].as_str().unwrap_or("terminó")),
            }
        }
        Cmd::Inbox { .. } => {
            let msgs: Vec<Message> = serde_json::from_value(data)?;
            if msgs.is_empty() {
                println!("sin mensajes pendientes");
            }
            for m in msgs {
                println!("#{} {} [{}] hace {}s: {}", m.id, m.from_name, m.kind, m.age_secs, m.summary);
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests_workspace {
    use super::*;
    use serde_json::json;

    fn pane(id: u32, status: Status, ws: Option<&str>, unacked: usize) -> PaneInfo {
        serde_json::from_value(json!({
            "id": id, "name": format!("p{id}"), "status": status, "manager": null, "cmd": [],
            "unacked": unacked, "oldest_unacked_secs": null, "orphan": false,
            "waiting_on": null, "waiting_secs": null, "workspace": ws, "cwd": null
        }))
        .unwrap()
    }

    #[test]
    fn filtra_por_workspace_exacto() {
        let data = json!([
            {"id": 1, "workspace": "inventia/cotizacion"},
            {"id": 2, "workspace": "galgo/main"},
            {"id": 3}
        ]);
        let out = filter_workspace(data, "inventia/cotizacion");
        assert_eq!(out, json!([{"id": 1, "workspace": "inventia/cotizacion"}]));
        // Una respuesta que no es lista (p. ej. un error de otro tipo) pasa tal cual.
        assert_eq!(filter_workspace(json!({"x": 1}), "a"), json!({"x": 1}));
    }

    #[test]
    fn encabezado_resume_atencion() {
        let a = pane(1, Status::Blocked, Some("w"), 0);
        let b = pane(2, Status::Idle, Some("w"), 2);
        let c = pane(3, Status::Idle, Some("w"), 0);
        assert_eq!(workspace_header(Some("w"), &[&a, &b, &c]), "── w (3) · 1 bloqueado(s) · 1 con entregas pendientes");
        assert_eq!(workspace_header(None, &[&c]), "── sin workspace (1)");
    }

    /// B2: un nombre o workspace con secuencias de terminal no llega a la salida como control.
    #[test]
    fn la_salida_no_tiene_caracteres_de_control() {
        let v = visible("ws\u{1b}[31mROJO\n── falso (9)");
        assert!(!v.chars().any(char::is_control), "{v:?}");
        assert_eq!(v, "ws?[31mROJO?── falso (9)");
    }
}
