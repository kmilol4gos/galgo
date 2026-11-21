//! Tabla embebida y deliberadamente conservadora; los importes son equivalentes de lista.
use crate::parser::Model;
pub const PRICES_TOML: &str = include_str!("../prices.toml");
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Price { pub input: f64, pub read: f64, pub output: f64, pub write5: Option<f64>, pub write1h: Option<f64> }
/// Lee únicamente el subconjunto fijo del TOML embebido; no acepta entradas externas.
pub(crate) fn price(model: Model) -> Option<Price> {
    lookup(model.name()?)
}

/// Precio por el id que el agente escribe en el transcript (`message.model`). Casa con la fila por su nombre o
/// por su lista `api_ids`; solo filas verificadas.
pub(crate) fn price_id(id: &str) -> Option<Price> {
    lookup(id)
}

fn lookup(name: &str) -> Option<Price> {
    for section in PRICES_TOML.split("[[price]]").skip(1) {
        let mut id = None; let mut verified = false; let mut input = None; let mut api_ids: Vec<&str> = Vec::new();
        let mut read = None; let mut output = None; let mut write5 = None; let mut write1h = None;
        for line in section.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                // Lista de ids de la API: `api_ids = ["claude-sonnet-5-5"]`. Solo se leen comillas simples de valor.
                "api_ids" => api_ids = value.split('"').enumerate().filter(|(i, _)| i % 2 == 1).map(|(_, s)| s).collect(),
                "model" => id = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')),
                "id_verified" => verified = value == "true",
                "input" => input = value.parse().ok(), "cache_read" => read = value.parse().ok(),
                "output" => output = value.parse().ok(), "cache_write_5m" => write5 = value.parse().ok(),
                "cache_write_1h" => write1h = value.parse().ok(), _ => (),
            }
        }
        let matches = id == Some(name) || api_ids.contains(&name);
        if verified && matches { return Some(Price { input: input?, read: read?, output: output?, write5, write1h }); }
    }
    None
}
