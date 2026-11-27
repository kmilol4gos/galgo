//! `galgo usage`: uso vivo de los paneles y cuotas por agente, desde el daemon.
//! La salida nunca incluye rutas (ni `cwd` ni rutas de transcript).

use anyhow::{bail, Result};
use clap::Args;
use galgo_proto::{client::request, PaneInfo, Request, Status};
use galgo_usage::{CostSource, Limits, PaneUsage};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Args)]
pub struct UsageArgs {
    /// Agrupa en vez de listar paneles: workspace, agent o model.
    #[arg(long, value_parser = ["workspace", "agent", "model"], conflicts_with_all = ["pane", "limits"])]
    pub by: Option<String>,
    /// Solo este panel (nombre o id).
    #[arg(long, conflicts_with = "limits")]
    pub pane: Option<String>,
    /// Cuotas por agente, con hora de observación y reinicio.
    #[arg(long)]
    pub limits: bool,
}

fn fetch(req: Request) -> Result<Value> {
    let resp = request(&req)?;
    if !resp.ok {
        bail!("{}", resp.error.unwrap_or_else(|| "error desconocido".into()));
    }
    Ok(resp.data)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 345 → "345", 598000 → "598k", 1.4M → "1.4M".
pub fn count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else if n < 1_000_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        format!("{:.1}M", n as f64 / 1e6)
    }
}

/// Edad de un dato en segundos: "hace 42s", "hace 3 min", "hace 2 h".
pub fn age(secs: u64) -> String {
    if secs < 60 {
        format!("hace {secs}s")
    } else if secs < 3600 {
        format!("hace {} min", secs / 60)
    } else if secs < 86_400 {
        format!("hace {} h", secs / 3600)
    } else {
        format!("hace {} d", secs / 86_400)
    }
}

/// Tiempo hasta el reinicio de una cuota: "2h 14m", "38m". Pasado el instante, dice que ya reinició.
pub fn until(resets_at: u64, now: u64) -> String {
    if resets_at <= now {
        return "ya reinició (dato antiguo)".into();
    }
    let s = resets_at - now;
    if s >= 3600 {
        format!("reinicia en {}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("reinicia en {}m", s / 60)
    }
}

/// Origen del coste tal como lo muestra la tabla.
fn origin(src: CostSource) -> &'static str {
    match src {
        CostSource::Agent => "agente",
        CostSource::Statusline => "statusline",
        CostSource::ListPrice => "lista",
        CostSource::Unknown => "sin precio",
    }
}

fn window_label(minutes: u64) -> String {
    match minutes {
        300 => "5h".into(),
        10080 => "7d".into(),
        m => format!("{m}m"),
    }
}

/// Fila de un panel. Sin usage: guiones. Totales parciales llevan `*`.
pub fn pane_row(p: &PaneInfo) -> Vec<String> {
    let dash = || "-".to_string();
    match &p.usage {
        None => vec![p.name.clone(), dash(), dash(), dash(), dash(), dash(), dash()],
        Some(u) => {
            let ctx = match (u.context_pct, u.context_tokens) {
                (Some(pct), Some(t)) => format!("{pct:.0}% {}", count(t)),
                (Some(pct), None) => format!("{pct:.0}%"),
                (None, Some(t)) => count(t),
                (None, None) => dash(),
            };
            let star = if u.partial { "*" } else { "" };
            let tokens = format!("{}/{}/{}{star}", count(u.input), count(u.cache_read), count(u.output));
            let cost = match u.cost_usd {
                Some(c) => format!("{c:.2} $ ({})", origin(u.cost_source)),
                None => "-".into(),
            };
            vec![
                p.name.clone(),
                u.agent.clone(),
                u.model.clone().unwrap_or_else(dash),
                ctx,
                tokens,
                cost,
                age(u.updated_secs),
            ]
        }
    }
}

/// Alinea columnas por caracteres (no por bytes).
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            let pad = widths[i] - c.chars().count();
            s.push_str(c);
            if i + 1 < cells.len() {
                s.push_str(&" ".repeat(pad + 2));
            }
        }
        s.trim_end().to_string()
    };
    let head: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut out = vec![line(&head)];
    out.extend(rows.iter().map(|r| line(r)));
    out.join("\n")
}

/// Paneles vivos (no `exited`), o uno solo si se pidió `--pane`.
fn live_panes(panes: Vec<PaneInfo>, only: Option<&str>) -> Result<Vec<PaneInfo>> {
    let live: Vec<PaneInfo> = panes.into_iter().filter(|p| p.status != Status::Exited).collect();
    match only {
        None => Ok(live),
        Some(name) => {
            let found: Vec<PaneInfo> = live.into_iter().filter(|p| p.name == name || p.id.to_string() == name).collect();
            if found.is_empty() {
                bail!("no hay un panel vivo llamado '{name}'");
            }
            Ok(found)
        }
    }
}

fn pane_table(panes: &[PaneInfo]) -> String {
    let rows: Vec<Vec<String>> = panes.iter().map(pane_row).collect();
    table(&["PANEL", "AGENTE", "MODELO", "CTX", "TOKENS (in/cache/out)", "COSTE", "EDAD"], &rows)
}

fn group_table(groups: &[Value]) -> String {
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|g| {
            let unknown = g["cost_unknown"].as_u64().unwrap_or(0);
            let mut cost = format!("{:.2} $", g["cost_usd"].as_f64().unwrap_or(0.0));
            if unknown > 0 {
                cost.push_str(&format!(" (+{unknown} sin precio)"));
            }
            vec![
                g["key"].as_str().unwrap_or("-").to_string(),
                g["panes"].as_u64().unwrap_or(0).to_string(),
                count(g["input"].as_u64().unwrap_or(0)),
                count(g["cache_read"].as_u64().unwrap_or(0)),
                count(g["output"].as_u64().unwrap_or(0)),
                cost,
            ]
        })
        .collect();
    table(&["GRUPO", "PANELES", "IN", "CACHE", "OUT", "COSTE (lista)"], &rows)
}

fn limits_lines(limits: &[Limits], now: u64) -> String {
    if limits.is_empty() {
        return "sin cuotas observadas".into();
    }
    limits
        .iter()
        .map(|l| {
            let windows: Vec<String> = l
                .windows
                .iter()
                .map(|w| {
                    let reset = w.resets_at.map(|r| format!(" ({})", until(r, now))).unwrap_or_default();
                    format!("{} {:.0}%{reset}", window_label(w.minutes), w.used_pct)
                })
                .collect();
            format!("{}  {}  · observado {}", l.agent, windows.join(" · "), age(now.saturating_sub(l.observed_at)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Salida JSON sin rutas: solo nombres, estado, workspace y uso.
fn pane_json(panes: &[PaneInfo]) -> Value {
    let rows: Vec<Value> = panes
        .iter()
        .map(|p| json!({ "id": p.id, "name": p.name, "status": p.status.as_str(), "workspace": p.workspace, "usage": p.usage }))
        .collect();
    json!({ "panes": rows })
}

pub fn run(args: &UsageArgs, json_out: bool) -> Result<()> {
    let view = fetch(Request::Usage { group_by: args.by.clone() })?;
    let limits: Vec<Limits> = serde_json::from_value(view["limits"].clone()).unwrap_or_default();
    let now = now_secs();

    if args.limits {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({ "limits": view["limits"] }))?);
        } else {
            println!("{}", limits_lines(&limits, now));
        }
        return Ok(());
    }

    if let Some(by) = &args.by {
        let groups = view["groups"].as_array().cloned().unwrap_or_default();
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({ "by": by, "groups": groups }))?);
        } else if groups.is_empty() {
            println!("sin uso medido");
        } else {
            println!("{}", group_table(&groups));
        }
        return Ok(());
    }

    let panes: Vec<PaneInfo> = serde_json::from_value(fetch(Request::List)?)?;
    let panes = live_panes(panes, args.pane.as_deref())?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&pane_json(&panes))?);
        return Ok(());
    }
    if panes.is_empty() {
        println!("sin paneles vivos");
        return Ok(());
    }
    println!("{}", pane_table(&panes));
    if panes.iter().any(|p| p.usage.as_ref().is_some_and(|u: &PaneUsage| u.partial)) {
        println!("* totales parciales: el archivo es mayor que la cola leída");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use galgo_usage::CostSource;

    fn pane(name: &str, usage: Option<PaneUsage>) -> PaneInfo {
        PaneInfo {
            id: 3,
            name: name.into(),
            status: Status::Working,
            manager: None,
            cmd: vec!["claude".into()],
            unacked: 0,
            oldest_unacked_secs: None,
            orphan: false,
            waiting_on: None,
            waiting_secs: None,
            workspace: Some("inventia/cotizacion".into()),
            cwd: Some("/Users/alguien/secreto".into()),
            usage,
            title: None,
            agent: false,
        }
    }

    fn usage(partial: bool) -> PaneUsage {
        PaneUsage {
            agent: "claude".into(),
            model: Some("opus-5.5".into()),
            input: 600_000,
            cache_read: 1_400_000,
            cache_write: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 12_000,
            reasoning: 0,
            cost_usd: Some(7.07),
            cost_source: CostSource::Statusline,
            context_tokens: Some(598_000),
            context_window: Some(1_000_000),
            context_pct: Some(59.8),
            updated_secs: 42,
            partial,
        }
    }

    #[test]
    fn formato_de_cifras_y_edades() {
        assert_eq!(count(345), "345");
        assert_eq!(count(598_000), "598k");
        assert_eq!(count(1_400_000), "1.4M");
        assert_eq!(age(42), "hace 42s");
        assert_eq!(age(180), "hace 3 min");
        assert_eq!(until(1_000 + 2 * 3600 + 14 * 60, 1_000), "reinicia en 2h 14m");
        assert_eq!(until(500, 1_000), "ya reinició (dato antiguo)");
    }

    /// Una fila muestra contexto, tokens y coste con su origen; sin uso, guiones.
    #[test]
    fn fila_con_uso_y_sin_uso() {
        let row = pane_row(&pane("worker-a", Some(usage(false))));
        assert_eq!(row[0], "worker-a");
        assert_eq!(row[3], "60% 598k");
        assert_eq!(row[4], "600k/1.4M/12k");
        assert_eq!(row[5], "7.07 $ (statusline)");
        let empty = pane_row(&pane("worker-b", None));
        assert!(empty[1..].iter().all(|c| c == "-"));
    }

    #[test]
    fn totales_parciales_se_marcan() {
        assert!(pane_row(&pane("p", Some(usage(true))))[4].ends_with('*'));
    }

    /// La salida JSON no contiene rutas (ni `cwd` ni rutas de transcript).
    #[test]
    fn json_sin_rutas() {
        let text = pane_json(&[pane("worker-a", Some(usage(false)))]).to_string();
        assert!(!text.contains("cwd"), "{text}");
        assert!(!text.contains("/Users"), "{text}");
        assert!(text.contains("worker-a"));
    }

    #[test]
    fn tabla_alinea_por_caracteres() {
        let t = table(&["A", "BB"], &[vec!["ñu".into(), "x".into()], vec!["abc".into(), "yy".into()]]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "A    BB");
        assert_eq!(lines[1], "ñu   x");
    }
}
