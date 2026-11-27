//! Persistencia de los workspaces: pestañas y splits por nombre de panel (los ids cambian al reiniciar el daemon).
//! Funciones puras: la GUI decide cuándo guardar y restaurar; aquí solo se convierte de y a JSON.

use crate::layout::{Node, Tab, WsTabs};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Límites de lo que se restaura: un archivo editado a mano no puede llenar la memoria.
const MAX_WORKSPACES: usize = 64;
const MAX_TABS: usize = 32;

/// Árbol de un layout con los nombres de sus paneles. `None` si ninguna hoja tiene nombre conocido.
pub fn tree_to_json(node: &Node, nombre: &dyn Fn(u32) -> Option<String>) -> Option<Value> {
    match node {
        Node::Leaf(id) => nombre(*id).map(|n| json!({ "leaf": n })),
        Node::Split { vertical, ratio, a, b } => match (tree_to_json(a, nombre), tree_to_json(b, nombre)) {
            (Some(a), Some(b)) => Some(json!({ "split": { "v": vertical, "r": ratio, "a": a, "b": b } })),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        },
    }
}

/// Árbol desde JSON, resolviendo cada nombre con `id_de`. Una hoja que no existe se quita; un split con un lado
/// vacío se queda con el otro. Cualquier forma inesperada da `None`.
pub fn json_to_tree(v: &Value, id_de: &dyn Fn(&str) -> Option<u32>) -> Option<Node> {
    if let Some(name) = v.get("leaf").and_then(Value::as_str) {
        return id_de(name).map(Node::Leaf);
    }
    let split = v.get("split")?;
    let vertical = split.get("v")?.as_bool()?;
    let ratio = split.get("r")?.as_f64()? as f32;
    let a = split.get("a").and_then(|x| json_to_tree(x, id_de));
    let b = split.get("b").and_then(|x| json_to_tree(x, id_de));
    match (a, b) {
        (Some(a), Some(b)) => Some(Node::Split { vertical, ratio: ratio.clamp(0.12, 0.88), a: Box::new(a), b: Box::new(b) }),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// Estado guardable de todos los workspaces con pestañas: su clave, si es explícito (con nombre y carpeta propios),
/// la pestaña activa y cada pestaña con el nombre de su panel enfocado y su árbol.
pub fn snapshot(
    by_key: &BTreeMap<String, WsTabs>,
    explicit: &BTreeMap<String, String>,
    nombre: &dyn Fn(u32) -> Option<String>,
) -> Value {
    let mut out = Vec::new();
    for (key, ws) in by_key {
        let mut tabs = Vec::new();
        for tab in &ws.tabs {
            let Some(tree) = tree_to_json(&tab.root, nombre) else { continue };
            let focus = nombre(tab.focus).or_else(|| tab.root.ids().first().and_then(|id| nombre(*id)));
            tabs.push(json!({ "focus": focus, "tree": tree }));
        }
        if tabs.is_empty() {
            continue;
        }
        let folder = explicit.get(key);
        out.push(json!({
            "key": key,
            "explicit": folder.is_some(),
            "folder": folder,
            "active": ws.active,
            "tabs": tabs,
        }));
    }
    json!({ "workspaces": out })
}

/// Lo restaurado: los workspaces explícitos (clave → carpeta) y sus pestañas. Los paneles que ya no existen se quitan.
pub struct Restored {
    pub explicit: BTreeMap<String, String>,
    pub by_key: BTreeMap<String, WsTabs>,
}

/// Quita las hojas repetidas de un árbol: un id aparece una sola vez (en la primera hoja que lo tenga). Un split que
/// se queda sin hojas desaparece; el resto se mantiene.
fn sin_repetidos(n: Node, vistos: &mut std::collections::HashSet<u32>) -> Option<Node> {
    match n {
        Node::Leaf(id) => vistos.insert(id).then_some(Node::Leaf(id)),
        Node::Split { vertical, ratio, a, b } => match (sin_repetidos(*a, vistos), sin_repetidos(*b, vistos)) {
            (Some(a), Some(b)) => Some(Node::Split { vertical, ratio, a: Box::new(a), b: Box::new(b) }),
            (Some(solo), None) | (None, Some(solo)) => Some(solo),
            (None, None) => None,
        },
    }
}

pub fn restore(v: &Value, id_de: &dyn Fn(&str) -> Option<u32>) -> Restored {
    let mut out = Restored { explicit: BTreeMap::new(), by_key: BTreeMap::new() };
    let Some(list) = v.get("workspaces").and_then(Value::as_array) else { return out };
    // review-15 B4: un panel está en una sola hoja de todo lo restaurado (si no, se pintaría dos veces).
    let mut vistos = std::collections::HashSet::new();
    for ws in list.iter().take(MAX_WORKSPACES) {
        let Some(key) = ws.get("key").and_then(Value::as_str).filter(|k| !k.is_empty() && k.len() <= 200) else { continue };
        let mut tabs = Vec::new();
        for t in ws.get("tabs").and_then(Value::as_array).into_iter().flatten().take(MAX_TABS) {
            let Some(root) = t.get("tree").and_then(|tree| json_to_tree(tree, id_de)).and_then(|r| sin_repetidos(r, &mut vistos)) else { continue };
            let focus = t.get("focus").and_then(Value::as_str).and_then(id_de).filter(|f| root.ids().contains(f)).unwrap_or(root.ids()[0]);
            tabs.push(Tab { root, focus });
        }
        if tabs.is_empty() {
            continue;
        }
        let active = (ws.get("active").and_then(Value::as_u64).unwrap_or(0) as usize).min(tabs.len() - 1);
        if let Some(folder) = ws.get("folder").and_then(Value::as_str).filter(|_| ws.get("explicit").and_then(Value::as_bool) == Some(true)) {
            out.explicit.insert(key.to_string(), folder.to_string());
        }
        out.by_key.insert(key.to_string(), WsTabs { tabs, active });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nombres(id: u32) -> Option<String> {
        ["", "shell", "claude", "codex"].get(id as usize).filter(|s| !s.is_empty()).map(|s| s.to_string())
    }
    fn ids(name: &str) -> Option<u32> {
        ["", "shell", "claude", "codex"].iter().position(|s| *s == name && !s.is_empty()).map(|i| i as u32)
    }

    /// Un árbol con splits sale por nombres y vuelve igual con los mismos ids.
    #[test]
    fn el_arbol_ida_y_vuelta_por_nombres() {
        let arbol = Node::Split {
            vertical: true,
            ratio: 0.6,
            a: Box::new(Node::Leaf(1)),
            b: Box::new(Node::Split { vertical: false, ratio: 0.5, a: Box::new(Node::Leaf(2)), b: Box::new(Node::Leaf(3)) }),
        };
        let json = tree_to_json(&arbol, &nombres).unwrap();
        assert_eq!(json_to_tree(&json, &ids), Some(arbol));
    }

    /// Un panel que ya no existe se quita del árbol; el split se queda con el otro lado.
    #[test]
    fn un_panel_desaparecido_se_quita() {
        let json = json!({ "split": { "v": true, "r": 0.5, "a": { "leaf": "shell" }, "b": { "leaf": "gone" } } });
        assert_eq!(json_to_tree(&json, &ids), Some(Node::Leaf(1)));
        assert_eq!(json_to_tree(&json!({ "leaf": "gone" }), &ids), None);
    }

    /// Lo guardado y lo restaurado conservan la clave, la carpeta de los explícitos, la pestaña activa y el foco.
    #[test]
    fn workspaces_sobreviven_al_reinicio() {
        let mut by_key = BTreeMap::new();
        by_key.insert(
            "nexo".to_string(),
            WsTabs {
                tabs: vec![
                    Tab { root: Node::Leaf(1), focus: 1 },
                    Tab { root: Node::Split { vertical: true, ratio: 0.5, a: Box::new(Node::Leaf(2)), b: Box::new(Node::Leaf(3)) }, focus: 3 },
                ],
                active: 1,
            },
        );
        let mut explicit = BTreeMap::new();
        explicit.insert("nexo".to_string(), "/tmp/nexo".to_string());
        let guardado = snapshot(&by_key, &explicit, &nombres);
        let r = restore(&guardado, &ids);
        assert_eq!(r.explicit.get("nexo").map(String::as_str), Some("/tmp/nexo"));
        let ws = &r.by_key["nexo"];
        assert_eq!(ws.active, 1);
        assert_eq!(ws.tabs[1].focus, 3);
        assert_eq!(ws.tabs.len(), 2);
    }

    /// review-15 B4: un panel que aparece dos veces (en dos hojas o en dos pestañas) queda en una sola hoja.
    #[test]
    fn panel_repetido_queda_en_una_hoja() {
        let json = json!({ "workspaces": [
            { "key": "a", "explicit": false, "active": 0, "tabs": [
                { "focus": "shell", "tree": { "split": { "v": true, "r": 0.5, "a": { "leaf": "shell" }, "b": { "leaf": "shell" } } } },
                { "focus": "claude", "tree": { "leaf": "claude" } }
            ] },
            { "key": "b", "explicit": false, "active": 0, "tabs": [ { "focus": "claude", "tree": { "leaf": "claude" } } ] }
        ] });
        let r = restore(&json, &ids);
        let todas: Vec<u32> = r.by_key.values().flat_map(|ws| ws.tabs.iter().flat_map(|t| t.root.ids())).collect();
        assert_eq!(todas.iter().filter(|id| **id == 1).count(), 1, "shell aparece una vez");
        assert_eq!(todas.iter().filter(|id| **id == 2).count(), 1, "claude aparece una vez");
    }

    /// Entradas malformadas o fuera de límite no rompen la restauración.
    #[test]
    fn datos_raros_se_ignoran() {
        let r = restore(&json!({ "workspaces": [ 7, { "key": "", "tabs": [] }, { "key": "x", "tabs": [{ "tree": { "nada": 1 } }] } ] }), &ids);
        assert!(r.by_key.is_empty() && r.explicit.is_empty());
        assert!(restore(&json!({}), &ids).by_key.is_empty());
    }
}
