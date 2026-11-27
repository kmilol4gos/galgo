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
/// Métricas de la fuente en uso, en em: (ancho de celda, alto de línea). Por defecto, las de Menlo (8,43 px y 18 px a 14).
static METRICS: RwLock<(f32, f32)> = RwLock::new((8.43 / 14.0, 18.0 / 14.0));

pub fn current() -> &'static str {
    *CURRENT.read().unwrap()
}

/// Ancho de celda y alto de línea de la fuente en uso, en em (punto 3: salen de la fuente, no de Menlo fijo).
pub fn metrics() -> (f32, f32) {
    *METRICS.read().unwrap()
}

/// Fija la fuente en uso y sus métricas, leídas del archivo de la fuente. Si no se pueden leer, se quedan las de Menlo.
pub fn set_current(family: &'static str) {
    *CURRENT.write().unwrap() = family;
    if let Some(m) = file_for(family, &font_dirs()).and_then(|f| metrics_from_file(&f)) {
        *METRICS.write().unwrap() = m;
    }
}

/// Una Nerd Font trae sus propios iconos y separadores, con el avance de una celda: no hace falta fallback.
pub fn is_nerd(family: &str) -> bool {
    family.contains("Nerd Font")
}

/// Archivo de la familia dentro de `dirs` (el de su prefijo, con preferencia por el Regular).
pub fn file_for(family: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let prefijo = CANDIDATES.iter().find(|(f, _)| *f == family)?.1;
    let mut hallados: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect::<Vec<_>>())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefijo)))
        .collect();
    hallados.sort_by_key(|p| !p.to_string_lossy().contains("Regular"));
    hallados.into_iter().next()
}

/// Ancho de celda (em de la letra M) y alto de línea (ascendente − descendente + interlineado) de un archivo de fuente.
pub fn metrics_from_file(path: &Path) -> Option<(f32, f32)> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;
    let upm = face.units_per_em() as f32;
    let adv = face.glyph_index('M').and_then(|g| face.glyph_hor_advance(g))? as f32;
    let alto = (face.ascender() as f32 - face.descender() as f32 + face.line_gap() as f32) / upm;
    (adv > 0.0 && alto > 0.0).then_some((adv / upm, alto))
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
    /// Punto 3: el ancho de celda sale de la fuente. Menlo mide 0,602 em de ancho; si la máquina no tiene Menlo, la
    /// prueba no aplica.
    #[test]
    fn metricas_salen_de_la_fuente() {
        let menlo = PathBuf::from("/System/Library/Fonts/Menlo.ttc");
        if let Some((cell, line)) = metrics_from_file(&menlo) {
            assert!((cell - 0.602).abs() < 0.01, "ancho de celda {cell}");
            assert!(line > 1.0 && line < 1.6, "alto de línea {line}");
        }
    }

    /// Una Nerd Font trae sus iconos con el avance de una celda; Menlo no, y necesita el fallback escalado.
    #[test]
    fn nerd_font_no_necesita_fallback() {
        assert!(is_nerd("MesloLGLDZ Nerd Font Mono"));
        assert!(!is_nerd("Menlo"));
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
