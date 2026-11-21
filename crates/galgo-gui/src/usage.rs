//! Texto y colores del uso en la GUI: línea bajo cada panel, detalle del panel seleccionado, total por
//! workspace y cuotas del pie. Funciones puras: no tocan el daemon ni el reloj (el instante llega como
//! argumento), para poder probarlas.

use galgo_proto::PaneInfo;
use galgo_usage::{CostSource, Limits, PaneUsage};
use iced::Color;

pub const AMBER: Color = crate::theme::BLOCKED;
pub const RED: Color = crate::theme::FAILED;

/// Cifras con coma decimal: 598000 → "598k", 1400000 → "1,4M".
pub fn count(n: u64) -> String {
    let text = if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else if n < 1_000_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        format!("{:.1}M", n as f64 / 1e6)
    };
    text.replace('.', ",")
}

/// Importe con coma decimal: 7.07 → "7,07 $".
pub fn money(c: f64) -> String {
    format!("{c:.2} $").replace('.', ",")
}

/// Edad de un dato: "hace 42 s", "hace 3 min", "hace 2 h".
pub fn age(secs: u64) -> String {
    if secs < 60 {
        format!("hace {secs} s")
    } else if secs < 3600 {
        format!("hace {} min", secs / 60)
    } else if secs < 86_400 {
        format!("hace {} h", secs / 3600)
    } else {
        format!("hace {} d", secs / 86_400)
    }
}

/// Color del contexto: ámbar desde el 80 %, rojo desde el 95 %. Sin dato o por debajo, ninguno.
pub fn ctx_color(pct: f32) -> Option<Color> {
    if pct >= 95.0 {
        Some(RED)
    } else if pct >= 80.0 {
        Some(AMBER)
    } else {
        None
    }
}

fn origin(src: CostSource) -> &'static str {
    match src {
        CostSource::Agent => "agente",
        CostSource::Statusline => "statusline",
        CostSource::ListPrice => "lista",
        CostSource::Unknown => "sin precio",
    }
}

/// Línea bajo el panel: «ctx 60 % · 7,07 $». Sin contexto ni coste, no hay línea.
pub fn pane_line(u: &PaneUsage) -> Option<(String, Option<Color>)> {
    let mut parts = Vec::new();
    let mut color = None;
    if let Some(pct) = u.context_pct {
        parts.push(format!("ctx {pct:.0} %"));
        color = ctx_color(pct);
    }
    if let Some(c) = u.cost_usd {
        parts.push(money(c));
    }
    (!parts.is_empty()).then(|| (parts.join(" · "), color))
}

/// Detalle del panel seleccionado: agente, modelo, contexto, tokens, coste con su origen y edad del dato.
pub fn detail_line(u: &PaneUsage) -> String {
    let mut parts = vec![u.agent.clone()];
    if let Some(m) = &u.model {
        parts.push(m.clone());
    }
    match (u.context_pct, u.context_tokens) {
        (Some(pct), Some(t)) => parts.push(format!("contexto {pct:.0} % ({} tok)", count(t))),
        (Some(pct), None) => parts.push(format!("contexto {pct:.0} %")),
        (None, Some(t)) => parts.push(format!("contexto {} tok", count(t))),
        (None, None) => {}
    }
    if u.input + u.cache_read + u.output > 0 {
        parts.push(format!("entrada {} · caché {} · salida {}", count(u.input), count(u.cache_read), count(u.output)));
    }
    parts.push(match u.cost_usd {
        Some(c) => format!("{} ({})", money(c), origin(u.cost_source)),
        None => "coste desconocido".into(),
    });
    if u.partial {
        parts.push("totales parciales".into());
    }
    parts.push(age(u.updated_secs));
    parts.join(" · ")
}

/// Total de un workspace para su cabecera: coste de los paneles con precio y tokens totales.
/// Sin ningún panel con uso, no hay total.
pub fn group_total(panes: &[&PaneInfo]) -> Option<String> {
    let usages: Vec<&PaneUsage> = panes.iter().filter_map(|p| p.usage.as_ref()).collect();
    if usages.is_empty() {
        return None;
    }
    // fold y no sum(): el suma de un iterador vacío de f64 da -0.0 y se vería «-0,00 $».
    let cost: f64 = usages.iter().filter_map(|u| u.cost_usd).fold(0.0, |a, c| a + c);
    let unknown = usages.iter().filter(|u| u.cost_usd.is_none()).count();
    let tokens: u64 = usages.iter().map(|u| u.input + u.cache_read + u.output).sum();
    let mut text = format!("total {}", money(cost));
    if tokens > 0 {
        text.push_str(&format!(" · {} tok", count(tokens)));
    }
    if unknown > 0 {
        text.push_str(&format!(" · {unknown} sin precio"));
    }
    Some(text)
}

/// Filas de una cuota: etiqueta de la ventana, porcentaje y texto de reinicio y de edad del dato.
pub struct LimitRow {
    pub label: String,
    pub pct: f32,
    pub detail: String,
}

fn window_label(minutes: u64) -> String {
    match minutes {
        300 => "5 h".into(),
        10080 => "7 d".into(),
        m => format!("{m} min"),
    }
}

/// Reinicio en texto: «reinicia en 2 h 14 min», o aviso si el instante ya pasó (el dato es antiguo).
fn reset_text(resets_at: u64, now: u64) -> String {
    if resets_at <= now {
        return "reinicio pasado (dato antiguo)".into();
    }
    let s = resets_at - now;
    if s >= 3600 {
        format!("reinicia en {} h {} min", s / 3600, (s % 3600) / 60)
    } else {
        format!("reinicia en {} min", s / 60)
    }
}

pub fn limit_rows(l: &Limits, now: u64) -> Vec<LimitRow> {
    let observed = age(now.saturating_sub(l.observed_at));
    l.windows
        .iter()
        .map(|w| {
            let reset = w.resets_at.map(|r| reset_text(r, now)).unwrap_or_else(|| "sin hora de reinicio".into());
            LimitRow {
                label: window_label(w.minutes),
                pct: w.used_pct,
                detail: format!("{reset} · dato {observed}"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use galgo_usage::LimitWindow;

    fn usage(pct: Option<f32>, cost: Option<f64>, source: CostSource) -> PaneUsage {
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
            cost_usd: cost,
            cost_source: source,
            context_tokens: pct.map(|_| 598_000),
            context_window: pct.map(|_| 1_000_000),
            context_pct: pct,
            updated_secs: 42,
            partial: false,
        }
    }

    /// La línea bajo el panel tiene el formato de la especificación: «ctx 60 % · 7,07 $».
    #[test]
    fn linea_bajo_el_panel() {
        let (text, color) = pane_line(&usage(Some(59.8), Some(7.07), CostSource::Statusline)).unwrap();
        assert_eq!(text, "ctx 60 % · 7,07 $");
        assert!(color.is_none());
    }

    /// Ámbar desde el 80 %, rojo desde el 95 %; por debajo, ningún color.
    #[test]
    fn umbrales_de_contexto() {
        assert!(ctx_color(79.9).is_none());
        assert_eq!(ctx_color(80.0), Some(AMBER));
        assert_eq!(ctx_color(94.9), Some(AMBER));
        assert_eq!(ctx_color(95.0), Some(RED));
        let (_, color) = pane_line(&usage(Some(96.0), None, CostSource::Unknown)).unwrap();
        assert_eq!(color, Some(RED));
    }

    /// Sin datos no hay línea ni texto de coste.
    #[test]
    fn sin_dato_no_hay_linea() {
        assert!(pane_line(&usage(None, None, CostSource::Unknown)).is_none());
        let (text, _) = pane_line(&usage(None, Some(1.5), CostSource::ListPrice)).unwrap();
        assert_eq!(text, "1,50 $");
    }

    #[test]
    fn detalle_con_origen_y_edad() {
        let d = detail_line(&usage(Some(59.8), Some(7.07), CostSource::Statusline));
        assert!(d.starts_with("claude · opus-5.5 · contexto 60 % (598k tok)"), "{d}");
        assert!(d.contains("7,07 $ (statusline)"), "{d}");
        assert!(d.ends_with("hace 42 s"), "{d}");
    }

    #[test]
    fn total_de_workspace_suma_con_precio_y_cuenta_los_sin_precio() {
        let a = pane(usage(Some(10.0), Some(2.0), CostSource::Agent));
        let b = pane(usage(None, None, CostSource::Unknown));
        let total = group_total(&[&a, &b]).unwrap();
        assert!(total.starts_with("total 2,00 $"), "{total}");
        assert!(total.ends_with("1 sin precio"), "{total}");
        assert!(group_total(&[&pane_without()]).is_none());
    }

    #[test]
    fn cuotas_con_reinicio_y_edad() {
        let l = Limits {
            agent: "claude".into(),
            plan: None,
            windows: vec![LimitWindow { minutes: 300, used_pct: 15.0, resets_at: Some(1_000 + 2 * 3600 + 14 * 60) }],
            observed_at: 1_000 - 180,
        };
        let rows = limit_rows(&l, 1_000);
        assert_eq!(rows[0].label, "5 h");
        assert_eq!(rows[0].pct, 15.0);
        assert_eq!(rows[0].detail, "reinicia en 2 h 14 min · dato hace 3 min");
    }

    /// Sin precio en ningún panel, el total no es -0,00 $ y no muestra tokens cero.
    #[test]
    fn total_sin_precio_no_es_menos_cero() {
        let mut u = usage(None, None, CostSource::Unknown);
        u.input = 0;
        u.cache_read = 0;
        u.output = 0;
        let b = pane(u);
        let total = group_total(&[&b]).unwrap();
        assert_eq!(total, "total 0,00 $ · 1 sin precio", "{total}");
    }

    #[test]
    fn cifras_con_coma() {
        assert_eq!(count(1_400_000), "1,4M");
        assert_eq!(count(598_000), "598k");
        assert_eq!(money(7.07), "7,07 $");
    }

    fn pane(usage: PaneUsage) -> PaneInfo {
        let mut p = pane_without();
        p.usage = Some(usage);
        p
    }

    fn pane_without() -> PaneInfo {
        PaneInfo {
            id: 1,
            name: "p".into(),
            status: galgo_proto::Status::Idle,
            manager: None,
            cmd: vec![],
            unacked: 0,
            oldest_unacked_secs: None,
            orphan: false,
            waiting_on: None,
            waiting_secs: None,
            workspace: None,
            cwd: None,
            usage: None,
        }
    }
}
