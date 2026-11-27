//! Preferencias de la GUI, en `gui-prefs.json` junto al socket. Cada clave se lee y se escribe por separado: guardar
//! una no pisa las demás. Escritura atómica con permisos 0600, como el resto del estado local.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Mapa de preferencias tal como está en disco. Un archivo ausente, ilegible o demasiado grande cuenta como vacío.
fn read(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return Default::default() };
    if !meta.file_type().is_file() || meta.len() > 4096 {
        return Default::default();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Valor de una clave, si existe.
pub fn get(path: &Path, key: &str) -> Option<serde_json::Value> {
    read(path).remove(key)
}

/// Guarda una clave dejando las demás intactas. `null` borra el valor (vuelve al predeterminado).
pub fn set(path: &Path, key: &str, value: serde_json::Value) -> std::io::Result<()> {
    let mut map = read(path);
    if value.is_null() {
        map.remove(key);
    } else {
        map.insert(key.to_string(), value);
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let body = serde_json::Value::Object(map).to_string();
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guardar una clave no borra las demás; `null` la quita.
    #[test]
    fn cada_clave_se_guarda_sin_pisar_las_demas() {
        let dir = std::env::temp_dir().join(format!("galgo-prefs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("gui-prefs.json");
        let _ = std::fs::remove_file(&path);
        set(&path, "terminal_font", serde_json::json!("Menlo")).unwrap();
        set(&path, "sidebar", serde_json::json!(true)).unwrap();
        assert_eq!(get(&path, "terminal_font"), Some(serde_json::json!("Menlo")));
        assert_eq!(get(&path, "sidebar"), Some(serde_json::json!(true)));
        set(&path, "terminal_font", serde_json::Value::Null).unwrap();
        assert_eq!(get(&path, "terminal_font"), None);
        assert_eq!(get(&path, "sidebar"), Some(serde_json::json!(true)), "la otra clave sigue");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
