//! Atención de los paneles: qué necesita una mirada del usuario y qué está "sin leer".
//! Lógica pura, sin GUI.

use galgo_proto::{PaneInfo, Status};
use std::collections::{BTreeMap, BTreeSet};

/// Motivo por el que un panel pide atención. `None` si no pide nada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Espera una decisión humana: siempre se muestra mientras dure.
    Blocked,
    /// Terminó su turno y nadie lo ha mirado desde entonces.
    DoneUnread,
    /// Entregas sin confirmar que llevan demasiado tiempo.
    Orphan,
}

/// Insignia de un panel, dado su estado y si está sin leer.
pub fn badge(pane: &PaneInfo, unread: bool) -> Option<Badge> {
    if pane.orphan {
        Some(Badge::Orphan)
    } else if pane.status == Status::Blocked {
        Some(Badge::Blocked)
    } else if pane.status == Status::Done && unread {
        Some(Badge::DoneUnread)
    } else {
        None
    }
}

/// Un panel que acaba de pasar a un estado que requiere atención, sin estar seleccionado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub pane: u32,
    pub name: String,
    pub status: Status,
}

#[derive(Debug, Default)]
pub struct Attention {
    /// Último estado visto de cada panel. El primer avistamiento solo fija la referencia.
    last: BTreeMap<u32, Status>,
    /// Paneles que pasaron a `done` o `blocked` sin estar seleccionados, hasta que se seleccionan.
    unread: BTreeSet<u32>,
}

impl Attention {
    /// Registra la lista actual de paneles. Devuelve las alertas nuevas: transiciones a `done` o
    /// `blocked` de paneles que no están seleccionados.
    pub fn observe(&mut self, panes: &[PaneInfo], selected: Option<u32>) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for p in panes {
            if let Some(&prev) = self.last.get(&p.id) {
                if prev != p.status && matches!(p.status, Status::Done | Status::Blocked) && Some(p.id) != selected {
                    self.unread.insert(p.id);
                    alerts.push(Alert { pane: p.id, name: p.name.clone(), status: p.status });
                }
            }
            if Some(p.id) == selected {
                self.unread.remove(&p.id);
            }
        }
        self.last = panes.iter().map(|p| (p.id, p.status)).collect();
        self.unread.retain(|id| self.last.contains_key(id));
        alerts
    }

    /// El usuario seleccionó el panel: deja de estar sin leer.
    pub fn mark_read(&mut self, pane: u32) {
        self.unread.remove(&pane);
    }

    pub fn is_unread(&self, pane: u32) -> bool {
        self.unread.contains(&pane)
    }

    /// Número de paneles que piden atención (para el título de la ventana).
    pub fn count(&self, panes: &[PaneInfo]) -> usize {
        panes.iter().filter(|p| badge(p, self.is_unread(p.id)).is_some()).count()
    }
}


/// Título de la ventana: "galgo" o "galgo (N)" cuando N paneles piden atención.
pub fn window_title(count: usize) -> String {
    if count == 0 { "galgo".into() } else { format!("galgo ({count})") }
}

/// Texto de la notificación de macOS para una alerta.
pub fn notification_text(name: &str, status: Status) -> String {
    match status {
        Status::Blocked => format!("{name} necesita una decisión"),
        _ => format!("{name} terminó su turno"),
    }
}

/// Script de `osascript` que muestra la notificación. Escapa comillas y barras invertidas.
pub fn notification_script(name: &str, status: Status) -> String {
    let body = notification_text(name, status).replace('\\', "\\\\").replace('"', "\\\"");
    format!("display notification \"{body}\" with title \"galgo\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u32, name: &str, status: Status) -> PaneInfo {
        PaneInfo {
            id,
            name: name.into(),
            status,
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
            title: None,
            agent: false,
        }
    }

    #[test]
    fn primer_avistamiento_no_alerta() {
        let mut a = Attention::default();
        let alerts = a.observe(&[pane(1, "w", Status::Done)], None);
        assert!(alerts.is_empty());
        assert!(!a.is_unread(1));
    }

    #[test]
    fn transicion_a_done_sin_seleccionar_queda_sin_leer() {
        let mut a = Attention::default();
        a.observe(&[pane(1, "w", Status::Working)], None);
        let alerts = a.observe(&[pane(1, "w", Status::Done)], None);
        assert_eq!(alerts, vec![Alert { pane: 1, name: "w".into(), status: Status::Done }]);
        assert!(a.is_unread(1));
        assert_eq!(a.count(&[pane(1, "w", Status::Done)]), 1);
    }

    #[test]
    fn seleccionado_no_queda_sin_leer() {
        let mut a = Attention::default();
        a.observe(&[pane(1, "w", Status::Working)], Some(1));
        let alerts = a.observe(&[pane(1, "w", Status::Done)], Some(1));
        assert!(alerts.is_empty());
        assert!(!a.is_unread(1));
    }

    #[test]
    fn seleccionar_limpia_la_marca() {
        let mut a = Attention::default();
        a.observe(&[pane(1, "w", Status::Working)], None);
        a.observe(&[pane(1, "w", Status::Done)], None);
        a.mark_read(1);
        assert!(!a.is_unread(1));
        // Sigue en done sin cambios: no vuelve a alertar.
        assert!(a.observe(&[pane(1, "w", Status::Done)], None).is_empty());
    }

    #[test]
    fn blocked_alerta_y_se_muestra_aunque_se_haya_mirado() {
        let mut a = Attention::default();
        a.observe(&[pane(1, "w", Status::Working)], None);
        let alerts = a.observe(&[pane(1, "w", Status::Blocked)], None);
        assert_eq!(alerts.len(), 1);
        let p = pane(1, "w", Status::Blocked);
        assert_eq!(badge(&p, false), Some(Badge::Blocked));
        assert_eq!(badge(&pane(1, "w", Status::Done), false), None);
        assert_eq!(badge(&pane(1, "w", Status::Done), true), Some(Badge::DoneUnread));
    }

    #[test]
    fn orphan_tiene_prioridad_y_se_cuenta() {
        let mut p = pane(1, "w", Status::Done);
        p.orphan = true;
        assert_eq!(badge(&p, false), Some(Badge::Orphan));
        let a = Attention::default();
        assert_eq!(a.count(&[p, pane(2, "x", Status::Working)]), 1);
    }

    #[test]
    fn un_panel_que_desaparece_deja_de_contar() {
        let mut a = Attention::default();
        a.observe(&[pane(1, "w", Status::Working)], None);
        a.observe(&[pane(1, "w", Status::Done)], None);
        a.observe(&[], None);
        assert!(!a.is_unread(1));
    }

    #[test]
    fn titulo_con_contador() {
        assert_eq!(window_title(0), "galgo");
        assert_eq!(window_title(2), "galgo (2)");
    }

    #[test]
    fn notificacion_escapa_el_texto() {
        assert_eq!(notification_text("w", Status::Done), "w terminó su turno");
        assert_eq!(notification_text("w", Status::Blocked), "w necesita una decisión");
        let s = notification_script("a\"b", Status::Done);
        assert_eq!(s, "display notification \"a\\\"b terminó su turno\" with title \"galgo\"");
    }

    /// Resumen por grupo: cuenta bloqueados, sin leer y sin confirmar; vacío si no hay nada.
    #[test]
    fn resumen_de_grupo() {
        let mut a = Attention::default();
        a.observe(&[pane(1, "x", Status::Working), pane(2, "y", Status::Working)], None);
        a.observe(&[pane(1, "x", Status::Done), pane(2, "y", Status::Blocked)], None);
        let p1 = pane(1, "x", Status::Done);
        let p2 = pane(2, "y", Status::Blocked);
        let mut p3 = pane(3, "z", Status::Idle);
        p3.orphan = true;
    }
}
