//! Fuente del terminal: detección de las Nerd Fonts instaladas, elección del usuario y su persistencia junto al
//! socket de galgo (nunca en la configuración de otras apps). Lógica pura, salvo `installed()` y `load`/`save`.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Familia y prefijo del archivo Regular de cada fuente que sabemos usar. Por orden de preferencia.
pub const CANDIDATES: [(&str, &str); 4] = [
    ("MesloLGLDZ Nerd Font Mono", "MesloLGLDZNerdFontMono"),
    ("JetBrainsMono Nerd Font Mono", "JetBrainsMonoNerdFontMono"),
    ("FiraCode Nerd Font Mono", "FiraCodeNerdFontMono"),
    ("Menlo", "Menlo"),
];

/// Carpetas donde macOS busca fuentes del usuario y del sistema.
pub fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    dirs.push(PathBuf::from("/Library/Fonts"));
    dirs.push(PathBuf::from("/System/Library/Fonts"));
    dirs.push(PathBuf::from("/System/Library/Fonts/Supplemental"));
    dirs
}

/// Familias de `CANDIDATES` que tienen su archivo en alguna de `dirs`, en orden de preferencia.
pub fn detect(dirs: &[PathBuf]) -> Vec<&'static str> {
    let names: Vec<String> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>())
        .collect();
    CANDIDATES
        .iter()
        .filter(|(_, prefijo)| names.iter().any(|n| n.starts_with(prefijo)))
        .map(|(familia, _)| *familia)
        .collect()
}

/// La familia que se usa por defecto: la primera instalada; Menlo si no hay ninguna.
pub fn automatic(installed: &[&'static str]) -> &'static str {
    installed.first().copied().unwrap_or("Menlo")
}

/// Fuente del terminal en uso. Empieza en Menlo; `main` la fija con la detección o con la elección guardada.
static CURRENT: RwLock<&'static str> = RwLock::new("Menlo");

pub fn current() -> &'static str {
    *CURRENT.read().unwrap()
}

pub fn set_current(family: &'static str) {
    *CURRENT.write().unwrap() = family;
}

/// Elección guardada del usuario ("automática" = no hay elección). Archivo junto al socket, 0600.
pub fn load(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > 4096 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("terminal_font").and_then(|v| v.as_str()).map(str::to_string)
}

pub fn save(path: &Path, family: Option<&str>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let body = serde_json::json!({ "terminal_font": family }).to_string();
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carpeta_con(archivos: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("galgo-fuentes-{}-{}", std::process::id(), archivos.len()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for a in archivos {
            std::fs::write(dir.join(a), b"").unwrap();
        }
        dir
    }

    /// Detección: una Nerd Font instalada gana sobre Menlo; sin ninguna, la automática es Menlo.
    #[test]
    fn detecta_nerd_font_y_cae_a_menlo() {
        let dir = carpeta_con(&["JetBrainsMonoNerdFontMono-Regular.ttf", "Menlo.ttc"]);
        let instaladas = detect(std::slice::from_ref(&dir));
        assert_eq!(instaladas, vec!["JetBrainsMono Nerd Font Mono", "Menlo"]);
        assert_eq!(automatic(&instaladas), "JetBrainsMono Nerd Font Mono");
        let vacia = carpeta_con(&["Helvetica.ttc"]);
        assert_eq!(automatic(&detect(std::slice::from_ref(&vacia))), "Menlo");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&vacia);
    }

    /// La elección se guarda y se vuelve a leer; una elección "automática" vuelve a null.
    #[test]
    fn elección_persiste() {
        let dir = carpeta_con(&[]);
        let file = dir.join("gui-prefs.json");
        save(&file, Some("FiraCode Nerd Font Mono")).unwrap();
        assert_eq!(load(&file).as_deref(), Some("FiraCode Nerd Font Mono"));
        save(&file, None).unwrap();
        assert_eq!(load(&file), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
