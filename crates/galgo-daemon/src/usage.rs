//! Uso vivo por panel. Todo en memoria: nunca se guarda en `state.json`.
//!
//! Dos fuentes:
//! - **Transcript** (`Hook` con `source`): `galgo-usage` relee el archivo del agente. Un hilo propio, fuera de
//!   tokio, lo hace cada 2 s y justo después de cada hook.
//! - **Informe del agente** (`UsageReport`, p. ej. la statusline de Claude): contexto, coste y cuotas tal como
//!   el agente los da. Pisa lo que venga del transcript.

use super::Daemon;
use anyhow::Result;
use galgo_proto::{HookSource, LimitReport};
use galgo_usage::{Agent, CostSource, LimitWindow, Limits, PaneUsage, UsageTracker};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{mpsc::{Receiver, RecvTimeoutError, Sender}, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Intervalo del sondeo de transcripts.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// Estado de uso fuera del estado del daemon: no se persiste ni bloquea las demás peticiones.
pub struct UsageLive {
    tracker: Mutex<UsageTracker>,
    tracked: Mutex<BTreeSet<u32>>,
    /// Última cuota informada por agente.
    limits: Mutex<BTreeMap<String, Limits>>,
    wake: Sender<()>,
}

impl UsageLive {
    pub fn new(wake: Sender<()>) -> Self {
        Self {
            tracker: Mutex::new(UsageTracker::new()),
            tracked: Mutex::new(BTreeSet::new()),
            limits: Mutex::new(BTreeMap::new()),
            wake,
        }
    }
}

/// Último informe del agente para un panel.
#[derive(Debug, Clone)]
pub struct ReportData {
    pub agent: String,
    pub model: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_usd: Option<f64>,
    /// Segundos Unix del informe: de aquí sale la edad del dato.
    pub at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn agent_of(name: &str) -> Option<Agent> {
    match name {
        "claude" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        "pi" => Some(Agent::Pi),
        "opencode" => Some(Agent::Opencode),
        _ => None,
    }
}

/// Minutos de cada ventana de cuota conocida. Un nombre desconocido no se guarda.
fn window_minutes(name: &str) -> Option<u64> {
    match name {
        "five_hour" => Some(300),
        "seven_day" => Some(10080),
        _ => None,
    }
}

/// Uso combinado de un panel: el transcript da los totales, el informe del agente pisa contexto y coste.
pub fn merged(usage: Option<&PaneUsage>, report: Option<&ReportData>) -> Option<PaneUsage> {
    let now = now_secs();
    match (usage, report) {
        (None, None) => None,
        (Some(u), None) => Some(u.clone()),
        (base, Some(r)) => {
            let mut u = base.cloned().unwrap_or_else(|| PaneUsage {
                agent: r.agent.clone(),
                model: None,
                input: 0,
                cache_read: 0,
                cache_write: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
                output: 0,
                reasoning: 0,
                cost_usd: None,
                cost_source: CostSource::Unknown,
                context_tokens: None,
                context_window: None,
                context_pct: None,
                updated_secs: 0,
                partial: false,
            });
            if r.model.is_some() {
                u.model = r.model.clone();
            }
            if r.context_tokens.is_some() {
                u.context_tokens = r.context_tokens;
            }
            if r.context_window.is_some() {
                u.context_window = r.context_window;
            }
            if let (Some(t), Some(w)) = (u.context_tokens, u.context_window) {
                if w > 0 {
                    u.context_pct = Some(t as f32 * 100.0 / w as f32);
                }
            }
            if let Some(c) = r.cost_usd {
                u.cost_usd = Some(c);
                u.cost_source = CostSource::Statusline;
            }
            u.updated_secs = now.saturating_sub(r.at);
            Some(u)
        }
    }
}

impl Daemon {
    /// Un hook con fuente: empieza a seguir su transcript y pide un sondeo inmediato.
    pub(super) fn track_hook(&self, pane: u32, src: &HookSource) {
        let (Some(agent), Some(transcript)) = (agent_of(&src.agent), src.transcript.as_deref()) else { return };
        let key = pane.to_string();
        self.usage.tracker.lock().unwrap().track(&key, agent, src.session_id.as_deref().unwrap_or(""), transcript);
        self.usage.tracked.lock().unwrap().insert(pane);
        let _ = self.usage.wake.send(());
    }

    /// Deja de seguir el transcript de un panel (al quitarlo).
    pub(super) fn untrack_usage(&self, pane: u32) {
        self.usage.tracker.lock().unwrap().untrack(&pane.to_string());
        self.usage.tracked.lock().unwrap().remove(&pane);
    }

    /// Relee los transcripts de los paneles seguidos y guarda el último uso de cada uno, en memoria.
    pub(super) fn poll_usage(&self) {
        let ids: Vec<u32> = self.usage.tracked.lock().unwrap().iter().copied().collect();
        for id in ids {
            let key = id.to_string();
            let (usage, limits) = {
                let mut t = self.usage.tracker.lock().unwrap();
                (t.poll(&key), t.limits(&key).cloned())
            };
            if let Some(u) = usage {
                if let Some(p) = self.state.lock().unwrap().panes.get_mut(&id) {
                    p.usage = Some(u);
                }
            }
            if let Some(l) = limits {
                self.usage.limits.lock().unwrap().insert(l.agent.clone(), l);
            }
        }
    }

    /// Un informe del agente: contexto, coste y cuotas. Pisa lo que venga del transcript.
    pub(super) fn usage_report(&self, pane: &str, agent: String, model: Option<String>, context_tokens: Option<u64>, context_window: Option<u64>, cost_usd: Option<f64>, limits: &[LimitReport]) -> Result<()> {
        let at = now_secs();
        {
            let mut st = self.state.lock().unwrap();
            let id = st.resolve(pane)?;
            let p = st.panes.get_mut(&id).unwrap();
            p.usage_report = Some(ReportData { agent: agent.clone(), model, context_tokens, context_window, cost_usd, at });
        }
        if !limits.is_empty() {
            let windows: Vec<LimitWindow> = limits
                .iter()
                .filter_map(|l| {
                    window_minutes(&l.name).map(|minutes| LimitWindow { minutes, used_pct: l.used_pct, resets_at: l.resets_at })
                })
                .collect();
            if !windows.is_empty() {
                let l = Limits { agent: agent.clone(), plan: None, windows, observed_at: at };
                self.usage.limits.lock().unwrap().insert(agent, l);
            }
        }
        Ok(())
    }

    /// Vista agregada del uso vivo y de las cuotas por agente. `group_by`: workspace, agent o model.
    pub(super) fn usage_view(&self, group_by: Option<&str>) -> Result<Value> {
        let by = group_by.unwrap_or("agent");
        if !matches!(by, "workspace" | "agent" | "model") {
            anyhow::bail!("agrupación desconocida: {by} (workspace, agent o model)");
        }
        let mut groups: BTreeMap<String, Value> = BTreeMap::new();
        {
            let st = self.state.lock().unwrap();
            for p in st.panes.values() {
                let Some(u) = merged(p.usage.as_ref(), p.usage_report.as_ref()) else { continue };
                let key = match by {
                    "workspace" => p.workspace.clone().unwrap_or_else(|| "sin workspace".into()),
                    "model" => u.model.clone().unwrap_or_else(|| "desconocido".into()),
                    _ => u.agent.clone(),
                };
                let g = groups.entry(key.clone()).or_insert_with(|| json!({
                    "key": key, "panes": 0, "input": 0u64, "cache_read": 0u64, "cache_write": 0u64,
                    "output": 0u64, "reasoning": 0u64, "cost_usd": 0.0f64, "cost_unknown": 0u64
                }));
                g["panes"] = json!(g["panes"].as_u64().unwrap() + 1);
                for (field, value) in [("input", u.input), ("cache_read", u.cache_read), ("cache_write", u.cache_write), ("output", u.output), ("reasoning", u.reasoning)] {
                    g[field] = json!(g[field].as_u64().unwrap() + value);
                }
                match u.cost_usd {
                    Some(c) => g["cost_usd"] = json!(g["cost_usd"].as_f64().unwrap() + c),
                    None => g["cost_unknown"] = json!(g["cost_unknown"].as_u64().unwrap() + 1),
                }
            }
        }
        let limits: Vec<Limits> = self.usage.limits.lock().unwrap().values().cloned().collect();
        Ok(json!({ "group_by": by, "groups": groups.into_values().collect::<Vec<_>>(), "limits": limits }))
    }
}

/// Hilo de sondeo: cada 2 s, o al recibir un aviso (hook con fuente o informe), relee los transcripts.
pub fn poll_loop(d: std::sync::Arc<Daemon>, rx: Receiver<()>) {
    loop {
        match rx.recv_timeout(POLL_EVERY) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while rx.try_recv().is_ok() {}
        d.poll_usage();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(cost: Option<f64>, tokens: Option<u64>, window: Option<u64>) -> ReportData {
        ReportData { agent: "claude".into(), model: Some("opus".into()), context_tokens: tokens, context_window: window, cost_usd: cost, at: now_secs() }
    }

    /// El informe del agente pisa coste y contexto; el porcentaje sale de tokens y ventana.
    #[test]
    fn el_informe_pisa_contexto_y_coste() {
        let m = merged(None, Some(&report(Some(7.07), Some(600_000), Some(1_000_000)))).unwrap();
        assert_eq!(m.cost_usd, Some(7.07));
        assert_eq!(m.cost_source, CostSource::Statusline);
        assert!((m.context_pct.unwrap() - 60.0).abs() < 0.01);
    }

    /// Sin informe, el transcript manda; sin nada, no hay uso.
    #[test]
    fn sin_informe_manda_el_transcript() {
        assert!(merged(None, None).is_none());
    }

    #[test]
    fn agentes_y_cuotas_conocidos() {
        assert!(agent_of("claude").is_some());
        assert!(agent_of("otro").is_none());
        assert_eq!(window_minutes("five_hour"), Some(300));
        assert_eq!(window_minutes("seven_day"), Some(10080));
        assert_eq!(window_minutes("month"), None);
    }
}
