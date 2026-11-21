//! Carpetas abiertas recientemente, para «Nuevo workspace». Es estado de la GUI, no del daemon, y vive junto al
//! socket de galgo (nunca en la configuración de Claude ni de otros agentes).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Cuántas carpetas se recuerdan.
pub const MAX: usize = 12;

/// Pone `path` el primero de la lista, sin duplicados, y recorta a `MAX`.
pub fn push(list: &mut Vec<String>, path: &str) {
    list.retain(|p| p != path);
    list.insert(0, path.to_string());
    list.truncate(MAX);
}

/// Las carpetas que contienen `query` (sin distinguir mayúsculas). Vacía = todas.
pub fn matching<'a>(list: &'a [String], query: &str) -> Vec<&'a String> {
    let q = query.trim().to_lowercase();
    list.iter().filter(|p| q.is_empty() || p.to_lowercase().contains(&q)).collect()
}

/// Archivo de recientes: junto al socket de galgo.
pub fn file() -> PathBuf {
    galgo_proto::socket_path().parent().unwrap_or(Path::new(".")).join("gui-recientes.json")
}

/// Lee la lista. Un archivo que falta, que no se entiende o con carpetas que ya no existen da una lista vacía
/// o filtrada: nunca es un error para la GUI.
pub fn load(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let list: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
    list.into_iter().filter(|p| Path::new(p).is_dir()).take(MAX).collect()
}

/// Escribe la lista de forma atómica y privada (0600).
pub fn save(path: &Path, list: &[String]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string(list).unwrap_or_else(|_| "[]".into()).as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_carpeta_abierta_pasa_al_principio_sin_duplicar() {
        let mut l = vec!["/a".to_string(), "/b".to_string()];
        push(&mut l, "/b");
        assert_eq!(l, vec!["/b", "/a"]);
    }

    #[test]
    fn la_lista_se_recorta() {
        let mut l = Vec::new();
        for i in 0..(MAX + 5) {
            push(&mut l, &format!("/p{i}"));
        }
        assert_eq!(l.len(), MAX);
        assert_eq!(l[0], format!("/p{}", MAX + 4));
    }

    #[test]
    fn busqueda_sin_mayusculas() {
        let l = vec!["/Users/x/inventia".to_string(), "/Users/x/galgo".to_string()];
        assert_eq!(matching(&l, "GAL"), vec![&l[1]]);
        assert_eq!(matching(&l, "  ").len(), 2);
    }

    /// Ida y vuelta por disco, con permisos privados; un archivo roto o con carpetas que faltan no rompe nada.
    #[test]
    fn guarda_y_carga_solo_carpetas_que_existen() {
        let dir = std::env::temp_dir().join(format!("galgo-recientes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gui-recientes.json");
        let real = dir.to_string_lossy().into_owned();
        save(&file, &[real.clone(), "/no/existe/nunca".into()]).unwrap();
        assert_eq!(load(&file), vec![real]);
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&file).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        std::fs::write(&file, "{no es json").unwrap();
        assert!(load(&file).is_empty());
        assert!(load(&dir.join("falta.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
