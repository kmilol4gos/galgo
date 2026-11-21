//! Tabla embebida y deliberadamente conservadora; los importes son equivalentes de lista.
use crate::parser::Model;
pub const PRICES_TOML: &str = include_str!("../prices.toml");
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Price { pub input: f64, pub read: f64, pub output: f64, pub write5: Option<f64>, pub write1h: Option<f64> }
/// Lee únicamente el subconjunto fijo del TOML embebido; no acepta entradas externas.
pub(crate) fn price(model: Model) -> Option<Price> {
    let name = model.name()?;
    for section in PRICES_TOML.split("[[price]]").skip(1) {
        let mut id = None; let mut verified = false; let mut input = None;
        let mut read = None; let mut output = None; let mut write5 = None; let mut write1h = None;
        for line in section.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "model" => id = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')),
                "id_verified" => verified = value == "true",
                "input" => input = value.parse().ok(), "cache_read" => read = value.parse().ok(),
                "output" => output = value.parse().ok(), "cache_write_5m" => write5 = value.parse().ok(),
                "cache_write_1h" => write1h = value.parse().ok(), _ => (),
            }
        }
        if verified && id == Some(name) { return Some(Price { input: input?, read: read?, output: output?, write5, write1h }); }
    }
    None
}
