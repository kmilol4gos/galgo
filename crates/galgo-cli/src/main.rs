//! galgo: CLI para controlar paneles y agentes a través del daemon.

mod claude;

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
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Lista paneles. Marca los que tienen entregas sin confirmar.
    Ls,
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
    /// Instala o quita los hooks de un agente.
    Hooks {
        #[command(subcommand)]
        action: claude::HooksCmd,
    },
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
        Cmd::Hook { .. } => std::process::exit(claude::run_hook()),
        Cmd::Hooks { action } => return claude::hooks_command(action),
        _ => {}
    }
    let req = match &cli.cmd {
        Cmd::Spawn { name, manager, inject, cwd, cmd } => Request::Spawn {
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
        Cmd::Ls => Request::List,
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
        },
        Cmd::Inbox { pane } => Request::Inbox { pane: pane.clone() },
        Cmd::Ack { msg, pane } => Request::Ack { pane: pane.clone(), msg: *msg },
        Cmd::Kill { pane } => Request::Kill { pane: pane.clone() },
        Cmd::Stop => Request::Shutdown,
        Cmd::Hook { .. } | Cmd::Hooks { .. } => unreachable!("se atienden antes"),
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
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&resp.data)?);
        return Ok(());
    }
    print_human(&cli.cmd, resp.data)
}

fn print_human(cmd: &Cmd, data: Value) -> Result<()> {
    match cmd {
        Cmd::Spawn { .. } => {
            let p: PaneInfo = serde_json::from_value(data)?;
            println!("panel {} ({}) creado", p.id, p.name);
        }
        Cmd::Ls => {
            let panes: Vec<PaneInfo> = serde_json::from_value(data)?;
            if panes.is_empty() {
                println!("sin paneles");
            }
            for p in panes {
                let mut notes = Vec::new();
                if let Some(m) = &p.manager {
                    notes.push(format!("reporta a {m}"));
                }
                if let (Some(t), Some(s)) = (&p.waiting_on, p.waiting_secs) {
                    notes.push(format!("espera a {t} hace {s}s"));
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
                println!("{:>3}  {:<20} {:<8} {}", p.id, p.name, p.status, notes.join(" · "));
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
