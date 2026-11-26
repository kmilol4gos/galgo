//! Formulario de "Nuevo panel": estado y armado de la petición `Spawn`. Lógica pura, sin GUI.

use galgo_proto::Request;

/// Comandos predefinidos. `Custom` toma el comando libre del formulario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Claude,
    Pi,
    OpenCode,
    Shell,
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 5] = [Preset::Claude, Preset::Pi, Preset::OpenCode, Preset::Shell, Preset::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Claude => "claude",
            Preset::Pi => "pi",
            Preset::OpenCode => "opencode",
            Preset::Shell => "shell",
            Preset::Custom => "libre",
        }
    }

    pub fn from_label(s: &str) -> Option<Preset> {
        Preset::ALL.into_iter().find(|p| p.label() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPane {
    pub name: String,
    pub preset: Preset,
    /// Comando libre, usado solo con `Preset::Custom`. Se separa por espacios.
    pub custom: String,
    /// Directorio de trabajo. Vacío = el directorio de inicio. Admite `~/…`.
    pub cwd: String,
    /// Panel que recibe el resultado, si hay alguno.
    pub manager: Option<String>,
    /// Workspace del panel ("inventia/cotizacion"). Vacío = que lo deduzca el daemon del repo git.
    pub workspace: String,
}

impl Default for NewPane {
    fn default() -> Self {
        Self { name: String::new(), preset: Preset::Claude, custom: String::new(), cwd: String::new(), manager: None, workspace: String::new() }
    }
}

/// Separa un comando en argumentos como lo haría una shell, sin ejecutar nada: respeta comillas simples,
/// comillas dobles y `\` fuera de comillas simples. `bash -c 'echo hola'` son tres argumentos, no cuatro.
pub fn split_command(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("comillas sin cerrar".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some(x) => { cur.push('\\'); cur.push(x); }
                            None => return Err("comillas sin cerrar".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("comillas sin cerrar".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(x) => cur.push(x),
                    None => return Err("la barra invertida final no tiene carácter".into()),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Entorno del que depende el armado: directorio de inicio y shell del usuario.
pub struct Env<'a> {
    pub home: &'a str,
    pub shell: &'a str,
}

/// Petición `Spawn` para el formulario. El tamaño es el de la cuadrícula de la ventana.
pub fn spawn_request(form: &NewPane, rows: u16, cols: u16, env: &Env) -> Result<Request, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("falta el nombre del panel".into());
    }
    if name.parse::<u32>().is_ok() {
        return Err("el nombre no puede ser solo un número".into());
    }
    let cmd: Vec<String> = match form.preset {
        Preset::Claude => vec!["claude".into()],
        Preset::Pi => vec!["pi".into()],
        Preset::OpenCode => vec!["opencode".into()],
        Preset::Shell => vec![env.shell.into()],
        Preset::Custom => split_command(&form.custom)?,
    };
    if cmd.is_empty() {
        return Err("falta el comando".into());
    }
    let cwd = match form.cwd.trim() {
        "" => env.home.to_string(),
        d if d == "~" => env.home.to_string(),
        d if d.starts_with("~/") => format!("{}/{}", env.home.trim_end_matches('/'), &d[2..]),
        // Una ruta relativa dependería del directorio del daemon, que no es el del usuario.
        d if d.starts_with('/') => d.to_string(),
        _ => return Err("el directorio debe ser absoluto o empezar por ~/".into()),
    };
    Ok(Request::Spawn {
        name: name.to_string(),
        cmd,
        cwd: Some(cwd),
        manager: form.manager.clone(),
        inject: false,
        rows,
        cols,
        workspace: Some(form.workspace.trim().to_string()).filter(|w| !w.is_empty()),
        worktree: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: Env = Env { home: "/Users/ana", shell: "/bin/zsh" };

    fn form(name: &str, preset: Preset) -> NewPane {
        NewPane { name: name.into(), preset, ..Default::default() }
    }

    fn parts(r: Request) -> (String, Vec<String>, Option<String>, Option<String>, u16, u16) {
        match r {
            Request::Spawn { name, cmd, cwd, manager, rows, cols, inject, .. } => {
                assert!(!inject);
                (name, cmd, cwd, manager, rows, cols)
            }
            other => panic!("no es Spawn: {other:?}"),
        }
    }

    #[test]
    fn presets_arman_su_comando() {
        for (preset, cmd) in [
            (Preset::Claude, vec!["claude"]),
            (Preset::Pi, vec!["pi"]),
            (Preset::OpenCode, vec!["opencode"]),
            (Preset::Shell, vec!["/bin/zsh"]),
        ] {
            let (_, got, _, _, _, _) = parts(spawn_request(&form("w", preset), 40, 140, &ENV).unwrap());
            assert_eq!(got, cmd, "{preset:?}");
        }
    }

    #[test]
    fn libre_separa_por_espacios_y_exige_algo() {
        let mut f = form("w", Preset::Custom);
        f.custom = "  bash  --norc  ".into();
        let (_, cmd, _, _, _, _) = parts(spawn_request(&f, 40, 140, &ENV).unwrap());
        assert_eq!(cmd, vec!["bash", "--norc"]);
        f.custom = "   ".into();
        assert_eq!(spawn_request(&f, 40, 140, &ENV).unwrap_err(), "falta el comando");
    }

    #[test]
    fn directorio_por_defecto_y_tilde() {
        let (_, _, cwd, _, _, _) = parts(spawn_request(&form("w", Preset::Claude), 40, 140, &ENV).unwrap());
        assert_eq!(cwd.as_deref(), Some("/Users/ana"));
        let mut f = form("w", Preset::Claude);
        f.cwd = "~/proyectos/x".into();
        let (_, _, cwd, _, _, _) = parts(spawn_request(&f, 40, 140, &ENV).unwrap());
        assert_eq!(cwd.as_deref(), Some("/Users/ana/proyectos/x"));
        f.cwd = "/tmp/otro".into();
        let (_, _, cwd, _, _, _) = parts(spawn_request(&f, 40, 140, &ENV).unwrap());
        assert_eq!(cwd.as_deref(), Some("/tmp/otro"));
    }

    #[test]
    fn manager_y_tamano_de_la_ventana() {
        let mut f = form("hijo", Preset::Pi);
        f.manager = Some("jefe".into());
        let (name, _, _, manager, rows, cols) = parts(spawn_request(&f, 33, 94, &ENV).unwrap());
        assert_eq!((name.as_str(), manager.as_deref(), rows, cols), ("hijo", Some("jefe"), 33, 94));
    }

    #[test]
    fn nombre_invalido() {
        assert_eq!(spawn_request(&form("  ", Preset::Claude), 40, 140, &ENV).unwrap_err(), "falta el nombre del panel");
        assert_eq!(
            spawn_request(&form("42", Preset::Claude), 40, 140, &ENV).unwrap_err(),
            "el nombre no puede ser solo un número"
        );
        let (name, _, _, _, _, _) = parts(spawn_request(&form(" w ", Preset::Claude), 40, 140, &ENV).unwrap());
        assert_eq!(name, "w");
    }

    #[test]
    fn etiquetas_de_preset_ida_y_vuelta() {
        for p in Preset::ALL {
            assert_eq!(Preset::from_label(p.label()), Some(p));
        }
        assert_eq!(Preset::from_label("otro"), None);
    }

    /// B4: las comillas se respetan y una ruta relativa se rechaza en lugar de resolverse contra el daemon.
    #[test]
    fn comando_con_comillas_y_directorio_relativo() {
        assert_eq!(split_command("bash -c 'echo hola'").unwrap(), vec!["bash", "-c", "echo hola"]);
        assert_eq!(split_command(r#""/Applications/Mi App/bin" --x"#).unwrap(), vec!["/Applications/Mi App/bin", "--x"]);
        assert_eq!(split_command("  a   b  ").unwrap(), vec!["a", "b"]);
        assert_eq!(split_command("''").unwrap(), vec![""]);
        assert!(split_command("bash -c 'eco").unwrap_err().contains("sin cerrar"));
        let mut f = form("w", Preset::Custom);
        f.custom = "bash -c 'echo hola'".into();
        let (_, cmd, _, _, _, _) = parts(spawn_request(&f, 40, 140, &ENV).unwrap());
        assert_eq!(cmd, vec!["bash", "-c", "echo hola"]);
        let mut f = form("w", Preset::Claude);
        f.cwd = "proyecto/x".into();
        assert!(spawn_request(&f, 40, 140, &ENV).unwrap_err().contains("absoluto"));
        f.cwd = "~otro/x".into();
        assert!(spawn_request(&f, 40, 140, &ENV).is_err());
    }

    /// Workspace: se pasa al `Spawn`; vacío significa que lo deduzca el daemon.
    #[test]
    fn workspace_del_formulario() {
        let mut f = form("w", Preset::Shell);
        f.workspace = "  inventia/cotizacion ".into();
        match spawn_request(&f, 40, 140, &ENV).unwrap() {
            Request::Spawn { workspace, .. } => assert_eq!(workspace.as_deref(), Some("inventia/cotizacion")),
            other => panic!("{other:?}"),
        }
        match spawn_request(&form("w", Preset::Shell), 40, 140, &ENV).unwrap() {
            Request::Spawn { workspace, .. } => assert_eq!(workspace, None),
            other => panic!("{other:?}"),
        }
    }
}
