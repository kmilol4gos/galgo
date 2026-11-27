//! Pestañas y splits de cada workspace. Lógica pura: sin GUI ni daemon. Las hojas son ids de panel del daemon; el
//! layout vive solo en la GUI (el daemon no sabe de pestañas ni de splits).

use std::collections::{BTreeMap, HashMap};

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

/// Árbol de una pestaña: una hoja es un panel; un split reparte su área en dos partes. `vertical` = lado a lado (a la
/// izquierda y a la derecha); si no, uno encima del otro. `ratio` es la fracción que ocupa `a` (0,5 al crear;
/// se cambia arrastrando el divisor).
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(u32),
    Split { vertical: bool, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Límites de la fracción de un split: ningún lado baja del 12 % del área.
pub const RATIO_MIN: f32 = 0.12;
pub const RATIO_MAX: f32 = 0.88;

/// Camino hasta un split dentro del árbol: `false` = bajar por `a`, `true` = por `b`.
pub type Path = Vec<bool>;

/// Un divisor visible: el camino de su split, su orientación y el área que reparte.
#[derive(Debug, Clone, PartialEq)]
pub struct Divider {
    pub path: Path,
    pub vertical: bool,
    pub area: Rect,
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
                *self = Node::Split { vertical, ratio: 0.5, a: Box::new(Node::Leaf(id)), b: Box::new(Node::Leaf(new)) };
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
            Node::Split { vertical, ratio, a, b } => match (a.without(id), b.without(id)) {
                (None, None) => None,
                (Some(n), None) | (None, Some(n)) => Some(n),
                (Some(a), Some(b)) => Some(Node::Split { vertical, ratio, a: Box::new(a), b: Box::new(b) }),
            },
        }
    }

    /// Rectángulo de cada hoja dentro de `area`, en el orden de `ids`.
    pub fn rects(&self, area: Rect) -> Vec<(u32, Rect)> {
        match self {
            Node::Leaf(id) => vec![(*id, area)],
            Node::Split { vertical, ratio, a, b } => {
                let (ra, rb) = split_at(area, *vertical, *ratio);
                let mut v = a.rects(ra);
                v.extend(b.rects(rb));
                v
            }
        }
    }

    /// Divisores del árbol dentro de `area`, con el camino de cada split.
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        self.collect_dividers(area, &mut Vec::new(), &mut out);
        out
    }

    fn collect_dividers(&self, area: Rect, path: &mut Path, out: &mut Vec<Divider>) {
        if let Node::Split { vertical, ratio, a, b } = self {
            out.push(Divider { path: path.clone(), vertical: *vertical, area });
            let (ra, rb) = split_at(area, *vertical, *ratio);
            path.push(false);
            a.collect_dividers(ra, path, out);
            path.pop();
            path.push(true);
            b.collect_dividers(rb, path, out);
            path.pop();
        }
    }

    /// Cambia la fracción del split en `path`, recortada a los límites. `false` si el camino no lleva a un split.
    pub fn set_ratio(&mut self, path: &[bool], value: f32) -> bool {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => {
                *ratio = value.clamp(RATIO_MIN, RATIO_MAX);
                true
            }
            (Node::Split { a, b, .. }, Some((&side, rest))) => if side { b.set_ratio(rest, value) } else { a.set_ratio(rest, value) },
            _ => false,
        }
    }
}

impl Divider {
    /// Posición (x o y) de la línea del divisor.
    pub fn line(&self, ratio: f32) -> f32 {
        if self.vertical { self.area.x + self.area.w * ratio } else { self.area.y + self.area.h * ratio }
    }

    /// Fracción que corresponde a un punto del puntero arrastrando este divisor.
    pub fn ratio_at(&self, x: f32, y: f32) -> f32 {
        let r = if self.vertical { (x - self.area.x) / self.area.w.max(1.0) } else { (y - self.area.y) / self.area.h.max(1.0) };
        r.clamp(RATIO_MIN, RATIO_MAX)
    }
}

/// Divisor bajo el punto (x, y), con `slop` píxeles de margen a cada lado de la línea: el más profundo gana.
pub fn divider_at(root: &Node, area: Rect, x: f32, y: f32, slop: f32) -> Option<Divider> {
    root.dividers(area)
        .into_iter()
        .filter(|d| {
            let ratio = ratio_of(root, &d.path).unwrap_or(0.5);
            let line = d.line(ratio);
            let a = d.area;
            if d.vertical {
                (x - line).abs() <= slop && y >= a.y && y <= a.y + a.h
            } else {
                (y - line).abs() <= slop && x >= a.x && x <= a.x + a.w
            }
        })
        .max_by_key(|d| d.path.len())
}

/// Fracción del split en `path`.
pub fn ratio_of(root: &Node, path: &[bool]) -> Option<f32> {
    match (root, path.split_first()) {
        (Node::Split { ratio, .. }, None) => Some(*ratio),
        (Node::Split { a, b, .. }, Some((&side, rest))) => ratio_of(if side { b } else { a }, rest),
        _ => None,
    }
}

/// Las dos partes de un área según la fracción `ratio` de la primera: lado a lado o apiladas.
pub fn split_at(area: Rect, vertical: bool, ratio: f32) -> (Rect, Rect) {
    if vertical {
        let w = (area.w * ratio).round();
        (Rect { w, ..area }, Rect { x: area.x + w, w: area.w - w, ..area })
    } else {
        let h = (area.h * ratio).round();
        (Rect { h, ..area }, Rect { y: area.y + h, h: area.h - h, ..area })
    }
}

/// Las dos mitades de un área: lado a lado o apiladas.
pub fn halves(area: Rect, vertical: bool) -> (Rect, Rect) {
    split_at(area, vertical, 0.5)
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
    /// Orden de creación en toda la fila: las pestañas de todos los workspaces se muestran por este número, así que
    /// sobrevive a `galgo stop` (se guarda) y no depende de la clave del workspace.
    pub orden: u64,
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
    /// Siguiente número de orden para una pestaña nueva.
    pub siguiente: u64,
    /// Paneles de la disposición restaurada que aún no han vuelto: nombre → (orden de su pestaña, si era la activa).
    pub pendientes: HashMap<String, (u64, bool)>,
}

impl Layouts {
    /// Alinea el layout con los paneles reales: quita los que ya no existen (y las pestañas vacías), y pone en una
    /// pestaña nueva los que no aparecen todavía. `panes` son (id, clave de workspace).
    #[cfg(test)]
    pub fn sync(&mut self, panes: &[(u32, String)]) {
        let sin_nombre: Vec<(u32, String, String)> = panes.iter().map(|(id, k)| (*id, k.clone(), String::new())).collect();
        self.sync_nombrado(&sin_nombre);
    }

    /// Como `sync`, pero con el nombre de cada panel (`(id, clave, nombre)`): un panel que vuelve tras un reinicio
    /// recupera el orden y la pestaña activa que tenía en la disposición guardada.
    pub fn sync_nombrado(&mut self, panes: &[(u32, String, String)]) {
        for ws in self.by_key.values_mut() {
            // Cada pestaña se reconstruye sin los paneles que ya no existen; si no le queda ninguno, se va.
            let old = std::mem::take(&mut ws.tabs);
            for tab in old {
                let (focus, orden) = (tab.focus, tab.orden);
                let mut root = Some(tab.root.clone());
                for id in tab.root.ids() {
                    if !panes.iter().any(|(p, _, _)| *p == id) {
                        root = root.and_then(|r| r.without(id));
                    }
                }
                if let Some(root) = root {
                    ws.tabs.push(Tab { root, focus, orden });
                }
            }
            ws.active = ws.active.min(ws.tabs.len().saturating_sub(1));
        }
        self.by_key.retain(|_, ws| !ws.tabs.is_empty());
        for (id, key, nombre) in panes {
            let known = self.by_key.values().any(|ws| ws.tabs.iter().any(|t| t.root.ids().contains(id)));
            if known {
                continue;
            }
            match self.pendientes.remove(nombre) {
                Some((orden, activa)) => {
                    let i = self.push_tab_con(key, Node::Leaf(*id), *id, Some(orden));
                    if activa {
                        if let Some(ws) = self.by_key.get_mut(key) {
                            ws.active = i;
                        }
                    }
                }
                None => self.new_tab(key, *id),
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
        let vacio = self.by_key.get(key).map_or(true, |ws| ws.tabs.is_empty());
        self.push_tab(key, Node::Leaf(id), id);
        if vacio {
            if let Some(ws) = self.by_key.get_mut(key) {
                ws.active = 0;
            }
        }
    }

    /// Añade una pestaña al final del workspace `key` con el siguiente número de orden. Devuelve su índice.
    pub fn push_tab(&mut self, key: &str, root: Node, focus: u32) -> usize {
        self.push_tab_con(key, root, focus, None)
    }

    /// Como `push_tab`, con un orden concreto (el guardado) o, si no, el siguiente.
    fn push_tab_con(&mut self, key: &str, root: Node, focus: u32, orden: Option<u64>) -> usize {
        let orden = match orden {
            Some(o) => {
                self.siguiente = self.siguiente.max(o + 1);
                o
            }
            None => {
                self.siguiente += 1;
                self.siguiente - 1
            }
        };
        let ws = self.by_key.entry(key.to_string()).or_default();
        ws.tabs.push(Tab { root, focus, orden });
        ws.tabs.len() - 1
    }

    /// Guarda los paneles que aún no han vuelto de la disposición restaurada, con su orden.
    pub fn recordar_pendientes(&mut self, pendientes: HashMap<String, (u64, bool)>) {
        if let Some(mas_alto) = pendientes.values().map(|(o, _)| o + 1).max() {
            self.siguiente = self.siguiente.max(mas_alto);
        }
        self.pendientes = pendientes;
    }

    /// Carga pestañas restauradas (con su orden guardado) y deja el contador por encima de todas.
    pub fn cargar(&mut self, restauradas: BTreeMap<String, WsTabs>) {
        for (key, ws) in restauradas {
            let mas_alto = ws.tabs.iter().map(|t| t.orden + 1).max().unwrap_or(0);
            self.siguiente = self.siguiente.max(mas_alto);
            self.by_key.insert(key, ws);
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

    /// Cambia la fracción de un split de la pestaña visible de `key`.
    pub fn set_ratio(&mut self, key: &str, path: &[bool], value: f32) -> bool {
        match self.by_key.get_mut(key).and_then(|ws| ws.tabs.get_mut(ws.active)) {
            Some(tab) => tab.root.set_ratio(path, value),
            None => false,
        }
    }

    /// Pestañas de todos los workspaces en el orden de la fila: (clave, índice dentro de su workspace).
    pub fn en_orden(&self) -> Vec<(&String, usize)> {
        let mut v: Vec<(u64, &String, usize)> = self
            .by_key
            .iter()
            .flat_map(|(k, ws)| ws.tabs.iter().enumerate().map(move |(i, t)| (t.orden, k, i)))
            .collect();
        v.sort_by_key(|t| t.0);
        v.into_iter().map(|(_, k, i)| (k, i)).collect()
    }

    /// Clave del workspace que contiene `id`.
    pub fn key_of(&self, id: u32) -> Option<String> {
        self.by_key.iter().find(|(_, ws)| ws.tabs.iter().any(|t| t.root.ids().contains(&id))).map(|(k, _)| k.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La fila sigue el orden de creación aunque los workspaces tengan claves distintas (no el orden alfabético).
    #[test]
    fn la_fila_sigue_el_orden_de_creacion_entre_workspaces() {
        let mut l = Layouts::default();
        l.new_tab("zeta", 1);
        l.new_tab("alfa", 2);
        l.new_tab("zeta", 3);
        let orden: Vec<(String, usize)> = l.en_orden().into_iter().map(|(k, i)| (k.clone(), i)).collect();
        assert_eq!(orden, vec![("zeta".to_string(), 0), ("alfa".to_string(), 0), ("zeta".to_string(), 1)]);
    }

    /// Un panel que vuelve después de la restauración recupera su orden (aunque llegue antes que otro que iba delante)
    /// y, si era la pestaña activa, sigue activa.
    #[test]
    fn el_panel_que_vuelve_recupera_su_orden_y_la_activa() {
        let mut l = Layouts::default();
        let mut pend = HashMap::new();
        pend.insert("shell".to_string(), (0u64, true));
        pend.insert("tres".to_string(), (2u64, false));
        l.recordar_pendientes(pend);
        // Llega primero «tres» y luego «shell», como en una relanzada real.
        l.sync_nombrado(&[(10, String::new(), "tres".into()), (11, String::new(), "shell".into())]);
        let ws = &l.by_key[""];
        assert_eq!(ws.tabs[ws.active].focus, 11, "la activa sigue siendo shell");
        let orden: Vec<u32> = l.en_orden().into_iter().map(|(k, i)| l.by_key[k].tabs[i].focus).collect();
        assert_eq!(orden, vec![11, 10], "shell (orden 0) antes que tres (orden 2)");
        l.new_tab("", 12);
        assert_eq!(l.en_orden().last().map(|(k, i)| l.by_key[*k].tabs[*i].focus), Some(12), "lo nuevo va al final");
    }

    /// Tras restaurar, el orden guardado manda y las pestañas nuevas van detrás de todas.
    #[test]
    fn el_orden_restaurado_se_conserva_y_lo_nuevo_va_al_final() {
        let tab = |id: u32, orden: u64| Tab { root: Node::Leaf(id), focus: id, orden };
        let mut por_clave = BTreeMap::new();
        por_clave.insert("alfa".to_string(), WsTabs { tabs: vec![tab(2, 1)], active: 0 });
        por_clave.insert("zeta".to_string(), WsTabs { tabs: vec![tab(1, 0), tab(3, 2)], active: 1 });
        let mut l = Layouts::default();
        l.cargar(por_clave);
        let orden: Vec<(String, usize)> = l.en_orden().into_iter().map(|(k, i)| (k.clone(), i)).collect();
        assert_eq!(orden, vec![("zeta".to_string(), 0), ("alfa".to_string(), 0), ("zeta".to_string(), 1)]);
        assert_eq!(l.by_key["zeta"].active, 1, "la activa se mantiene");
        l.new_tab("alfa", 9);
        assert_eq!(l.en_orden().last().map(|(k, i)| (k.as_str(), *i)), Some(("alfa", 1)));
    }

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

    /// Arrastrar un divisor cambia la fracción de su split (con límites) y el reparto de los rectángulos; el divisor
    /// se encuentra con unos píxeles de margen, y el más profundo gana.
    #[test]
    fn divisores_arrastrables() {
        let mut n = Node::Leaf(1);
        n.split_leaf(1, true, 2);
        n.split_leaf(2, false, 3);
        let area = area();
        let d = divider_at(&n, area, 51.0, 10.0, 3.0).expect("divisor vertical en x=50");
        assert_eq!(d.path, Vec::<bool>::new());
        assert!(d.vertical);
        let h = divider_at(&n, area, 75.0, 24.0, 3.0).expect("divisor horizontal del lado derecho");
        assert_eq!(h.path, vec![true]);
        assert!(divider_at(&n, area, 20.0, 24.0, 3.0).is_none(), "a la izquierda no hay divisor horizontal");
        assert!(n.set_ratio(&[], d.ratio_at(30.0, 0.0)));
        assert_eq!(n.rects(area)[0].1.w, 30.0);
        n.set_ratio(&[], 0.01);
        assert_eq!(ratio_of(&n, &[]), Some(RATIO_MIN), "con límite");
        assert!(!n.set_ratio(&[false], 0.5), "el lado a es una hoja");
    }
}
