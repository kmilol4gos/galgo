//! Pestañas y splits de cada workspace. Lógica pura: sin GUI ni daemon. Las hojas son ids de panel del daemon; el
//! layout vive solo en la GUI (el daemon no sabe de pestañas ni de splits).

use std::collections::BTreeMap;

/// Rectángulo en píxeles de la ventana.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Dirección de foco entre splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Árbol de una pestaña: una hoja es un panel; un split reparte su área en dos mitades iguales. `vertical` = lado a
/// lado (a la izquierda y a la derecha); si no, uno encima del otro.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(u32),
    Split { vertical: bool, a: Box<Node>, b: Box<Node> },
}

impl Node {
    pub fn ids(&self) -> Vec<u32> {
        match self {
            Node::Leaf(id) => vec![*id],
            Node::Split { a, b, .. } => {
                let mut v = a.ids();
                v.extend(b.ids());
                v
            }
        }
    }

    /// Sustituye la hoja `id` por un split con `id` y `new`. `false` si `id` no está.
    pub fn split_leaf(&mut self, id: u32, vertical: bool, new: u32) -> bool {
        match self {
            Node::Leaf(x) if *x == id => {
                *self = Node::Split { vertical, a: Box::new(Node::Leaf(id)), b: Box::new(Node::Leaf(new)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split_leaf(id, vertical, new) || b.split_leaf(id, vertical, new),
        }
    }

    /// Quita la hoja `id`. Un split que se queda con una sola hoja se colapsa a esa hoja. `None` si no queda nada.
    pub fn without(self, id: u32) -> Option<Node> {
        match self {
            Node::Leaf(x) if x == id => None,
            Node::Leaf(x) => Some(Node::Leaf(x)),
            Node::Split { vertical, a, b } => match (a.without(id), b.without(id)) {
                (None, None) => None,
                (Some(n), None) | (None, Some(n)) => Some(n),
                (Some(a), Some(b)) => Some(Node::Split { vertical, a: Box::new(a), b: Box::new(b) }),
            },
        }
    }

    /// Rectángulo de cada hoja dentro de `area`, en el orden de `ids`.
    pub fn rects(&self, area: Rect) -> Vec<(u32, Rect)> {
        match self {
            Node::Leaf(id) => vec![(*id, area)],
            Node::Split { vertical, a, b } => {
                let (ra, rb) = halves(area, *vertical);
                let mut v = a.rects(ra);
                v.extend(b.rects(rb));
                v
            }
        }
    }
}

/// Las dos mitades de un área: lado a lado o apiladas.
pub fn halves(area: Rect, vertical: bool) -> (Rect, Rect) {
    if vertical {
        let w = area.w / 2.0;
        (Rect { w, ..area }, Rect { x: area.x + w, w: area.w - w, ..area })
    } else {
        let h = area.h / 2.0;
        (Rect { h, ..area }, Rect { y: area.y + h, h: area.h - h, ..area })
    }
}

/// Vecino en una dirección: el panel cuyo centro está en esa dirección y más cerca (primero por el eje, luego por el
/// perpendicular). `None` si no hay nada en esa dirección.
pub fn neighbor(rects: &[(u32, Rect)], from: u32, dir: Dir) -> Option<u32> {
    let center = |r: &Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let (_, origin) = rects.iter().find(|(id, _)| *id == from)?;
    let (ox, oy) = center(origin);
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, r)| {
            let (cx, cy) = center(r);
            let (main, cross) = match dir {
                Dir::Left => (ox - cx, (cy - oy).abs()),
                Dir::Right => (cx - ox, (cy - oy).abs()),
                Dir::Up => (oy - cy, (cx - ox).abs()),
                Dir::Down => (cy - oy, (cx - ox).abs()),
            };
            (main > 0.0).then_some((*id, main, cross))
        })
        // Pesa más el desvío perpendicular: un panel muy lejos de lado no gana a uno casi alineado.
        .min_by(|a, b| (a.1 + 2.0 * a.2).partial_cmp(&(b.1 + 2.0 * b.2)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(id, _, _)| id)
}

/// Una pestaña: su árbol y el panel que tiene el foco dentro de ella.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub root: Node,
    pub focus: u32,
}

/// Las pestañas de un workspace y cuál está visible.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WsTabs {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

/// Pestañas de todos los workspaces, por clave (la etiqueta del workspace, o "" sin workspace).
#[derive(Debug, Clone, Default)]
pub struct Layouts {
    pub by_key: BTreeMap<String, WsTabs>,
}

impl Layouts {
    /// Alinea el layout con los paneles reales: quita los que ya no existen (y las pestañas vacías), y pone en una
    /// pestaña nueva los que no aparecen todavía. `panes` son (id, clave de workspace).
    pub fn sync(&mut self, panes: &[(u32, String)]) {
        for ws in self.by_key.values_mut() {
            // Cada pestaña se reconstruye sin los paneles que ya no existen; si no le queda ninguno, se va.
            let old = std::mem::take(&mut ws.tabs);
            for tab in old {
                let focus = tab.focus;
                let mut root = Some(tab.root.clone());
                for id in tab.root.ids() {
                    if !panes.iter().any(|(p, _)| *p == id) {
                        root = root.and_then(|r| r.without(id));
                    }
                }
                if let Some(root) = root {
                    ws.tabs.push(Tab { root, focus });
                }
            }
            ws.active = ws.active.min(ws.tabs.len().saturating_sub(1));
        }
        self.by_key.retain(|_, ws| !ws.tabs.is_empty());
        for (id, key) in panes {
            let known = self.by_key.values().any(|ws| ws.tabs.iter().any(|t| t.root.ids().contains(id)));
            if !known {
                self.new_tab(key, *id);
            }
        }
        // Una pestaña cuyo foco ya no existe pasa a su primer panel.
        for ws in self.by_key.values_mut() {
            for tab in &mut ws.tabs {
                if !tab.root.ids().contains(&tab.focus) {
                    tab.focus = tab.root.ids()[0];
                }
            }
        }
    }

    /// Pestaña nueva con el panel `id`. G1: solo se activa si el workspace no tenía pestañas; un panel que llega de
    /// fuera no cambia lo que se ve. Quien crea el panel y quiere verlo llama a `focus`.
    pub fn new_tab(&mut self, key: &str, id: u32) {
        let ws = self.by_key.entry(key.to_string()).or_default();
        let was_empty = ws.tabs.is_empty();
        ws.tabs.push(Tab { root: Node::Leaf(id), focus: id });
        if was_empty {
            ws.active = 0;
        }
    }

    /// Divide la hoja con foco `focus` y pone `new` a su lado (o debajo). El nuevo panel toma el foco.
    pub fn split(&mut self, focus: u32, vertical: bool, new: u32) -> bool {
        for ws in self.by_key.values_mut() {
            for tab in &mut ws.tabs {
                if tab.root.split_leaf(focus, vertical, new) {
                    tab.focus = new;
                    return true;
                }
            }
        }
        false
    }

    /// Lleva el foco a `id`: activa su pestaña y le da el foco dentro de ella.
    pub fn focus(&mut self, id: u32) {
        for ws in self.by_key.values_mut() {
            if let Some(i) = ws.tabs.iter().position(|t| t.root.ids().contains(&id)) {
                ws.active = i;
                ws.tabs[i].focus = id;
                return;
            }
        }
    }

    /// Pestaña del workspace `key` visible, si hay.
    pub fn active_tab(&self, key: &str) -> Option<&Tab> {
        self.by_key.get(key).and_then(|ws| ws.tabs.get(ws.active))
    }

    /// Cambia de pestaña dentro de `key`, dando la vuelta.
    pub fn cycle(&mut self, key: &str, delta: isize) {
        if let Some(ws) = self.by_key.get_mut(key) {
            let n = ws.tabs.len() as isize;
            if n > 0 {
                ws.active = (ws.active as isize + delta).rem_euclid(n) as usize;
            }
        }
    }

    /// Clave del workspace que contiene `id`.
    pub fn key_of(&self, id: u32) -> Option<String> {
        self.by_key.iter().find(|(_, ws)| ws.tabs.iter().any(|t| t.root.ids().contains(&id))).map(|(k, _)| k.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 100.0, h: 50.0 }
    }

    /// Un split lado a lado reparte el ancho en dos mitades; apilado, el alto.
    #[test]
    fn mitades_de_un_split() {
        let mut n = Node::Leaf(1);
        n.split_leaf(1, true, 2);
        let r = n.rects(area());
        assert_eq!(r[0], (1, Rect { x: 0.0, y: 0.0, w: 50.0, h: 50.0 }));
        assert_eq!(r[1], (2, Rect { x: 50.0, y: 0.0, w: 50.0, h: 50.0 }));
        let mut m = Node::Leaf(1);
        m.split_leaf(1, false, 2);
        let r = m.rects(area());
        assert_eq!(r[1], (2, Rect { x: 0.0, y: 25.0, w: 100.0, h: 25.0 }));
    }

    /// Cerrar una hoja colapsa su split: el hermano ocupa todo el espacio.
    #[test]
    fn cerrar_colapsa_el_split() {
        let mut n = Node::Leaf(1);
        n.split_leaf(1, true, 2);
        n.split_leaf(2, false, 3);
        let n = n.without(2).expect("quedan paneles");
        assert_eq!(n.ids(), vec![1, 3]);
        assert_eq!(n.without(1).expect("queda uno").ids(), vec![3], "un solo panel ocupa todo");
        assert!(Node::Leaf(9).without(9).is_none(), "sin paneles no queda nada");
    }

    /// El foco se mueve al vecino en la dirección pedida, o no se mueve si no hay nada.
    #[test]
    fn foco_por_direccion() {
        let mut n = Node::Leaf(1);
        n.split_leaf(1, true, 2);
        n.split_leaf(2, false, 3);
        let r = n.rects(area());
        assert_eq!(neighbor(&r, 1, Dir::Right), Some(2), "a la derecha de 1, el de arriba (empate: el primero)");
        assert_eq!(neighbor(&r, 2, Dir::Left), Some(1));
        assert_eq!(neighbor(&r, 3, Dir::Up), Some(2));
        assert_eq!(neighbor(&r, 2, Dir::Down), Some(3));
        assert_eq!(neighbor(&r, 1, Dir::Left), None);
    }

    /// Un panel nuevo nace en su pestaña; al desaparecer el último de una pestaña, la pestaña se va.
    #[test]
    fn sync_crea_pestanas_y_quita_las_vacias() {
        let mut l = Layouts::default();
        l.sync(&[(1, "repo/main".into()), (2, "repo/main".into()), (3, String::new())]);
        assert_eq!(l.by_key["repo/main"].tabs.len(), 2, "cada panel nuevo en su pestaña");
        assert_eq!(l.by_key[""].tabs.len(), 1);
        l.sync(&[(2, "repo/main".into()), (3, String::new())]);
        assert_eq!(l.by_key["repo/main"].tabs.len(), 1, "la pestaña de 1 ya no existe");
        l.sync(&[(3, String::new())]);
        assert!(!l.by_key.contains_key("repo/main"), "el workspace sin pestañas desaparece");
    }

    /// Dividir pone el foco en el panel nuevo; cambiar de pestaña da la vuelta; el foco de un panel activa su pestaña.
    #[test]
    fn dividir_y_pestanas() {
        let mut l = Layouts::default();
        l.sync(&[(1, "w".into())]);
        assert!(l.split(1, true, 2));
        assert_eq!(l.active_tab("w").unwrap().focus, 2);
        l.sync(&[(1, "w".into()), (2, "w".into()), (5, "w".into())]);
        assert_eq!(l.by_key["w"].tabs.len(), 2, "el 5 llega en pestaña nueva");
        // G1: el panel 5 llega de fuera y no cambia la pestaña visible.
        assert_eq!(l.by_key["w"].active, 0, "sync no activa una pestaña nueva en un workspace existente");
        l.cycle("w", 1);
        assert_eq!(l.key_of(5).as_deref(), Some("w"));
        assert_eq!(l.by_key["w"].active, 1);
        l.cycle("w", 1);
        assert_eq!(l.by_key["w"].active, 0, "da la vuelta");
        l.focus(5);
        assert_eq!(l.by_key["w"].active, 1);
        assert_eq!(l.active_tab("w").unwrap().focus, 5);
    }

    /// Si el panel con foco desaparece, la pestaña pasa a otro de sus paneles.
    #[test]
    fn foco_perdido_pasa_a_otro_panel_de_la_pestana() {
        let mut l = Layouts::default();
        l.sync(&[(1, "w".into())]);
        l.split(1, true, 2);
        l.sync(&[(1, "w".into()), (2, "w".into())]);
        assert_eq!(l.active_tab("w").unwrap().focus, 2);
        l.sync(&[(1, "w".into())]);
        assert_eq!(l.active_tab("w").unwrap().focus, 1);
        assert_eq!(l.active_tab("w").unwrap().root, Node::Leaf(1));
    }
}
