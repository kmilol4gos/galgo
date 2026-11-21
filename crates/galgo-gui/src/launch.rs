//! Armado de procesos nuevos desde la GUI sin formulario: nombres libres y la petición `Spawn`.

use galgo_proto::Request;

/// Primer nombre libre a partir de `base`: `claude`, `claude-2`, `claude-3`… `taken` incluye los nombres en uso.
pub fn free_name(base: &str, taken: &[&str]) -> String {
    if !taken.contains(&base) {
        return base.to_string();
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.iter().any(|t| t == c))
        .unwrap_or_else(|| base.to_string())
}

/// `Spawn` de un shell en `cwd`. El workspace va vacío: lo deduce el daemon del repo git de `cwd`.
pub fn shell_request(name: &str, shell: &str, cwd: &str, rows: u16, cols: u16) -> Request {
    Request::Spawn {
        name: name.to_string(),
        cmd: vec![shell.to_string()],
        cwd: Some(cwd.to_string()),
        manager: None,
        inject: false,
        rows,
        cols,
        workspace: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nombres_libres_en_secuencia() {
        assert_eq!(free_name("shell", &[]), "shell");
        assert_eq!(free_name("shell", &["shell"]), "shell-2");
        assert_eq!(free_name("shell", &["shell", "shell-2"]), "shell-3");
        // Un hueco se reutiliza.
        assert_eq!(free_name("shell", &["shell", "shell-3"]), "shell-2");
    }

    #[test]
    fn shell_en_el_directorio_sin_workspace_explicito() {
        match shell_request("shell", "/bin/zsh", "/Users/x/proyecto", 40, 140) {
            Request::Spawn { name, cmd, cwd, workspace, manager, .. } => {
                assert_eq!(name, "shell");
                assert_eq!(cmd, vec!["/bin/zsh"]);
                assert_eq!(cwd.as_deref(), Some("/Users/x/proyecto"));
                assert_eq!(workspace, None);
                assert_eq!(manager, None);
            }
            other => panic!("{other:?}"),
        }
    }
}
