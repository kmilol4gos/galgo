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

/// Opción del desplegable de modelo que no pasa ningún modelo: el que trae el agente por defecto (M2).
pub const DEFAULT_MODEL: &str = "predeterminado";

/// Lo que se puede lanzar desde el lanzador: un agente conocido o un shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Pi,
    OpenCode,
    Shell,
}

impl Agent {
    pub const ALL: [Agent; 5] = [Agent::Claude, Agent::Codex, Agent::Pi, Agent::OpenCode, Agent::Shell];

    /// Etiqueta del botón y base del nombre autogenerado.
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
            Agent::OpenCode => "opencode",
            Agent::Shell => "shell",
        }
    }

    /// Comando que ejecuta el panel. El shell es el del usuario.
    pub fn command(self, shell: &str) -> Vec<String> {
        match self {
            Agent::Shell => vec![shell.to_string()],
            other => vec![other.label().to_string()],
        }
    }

    /// Modelos que se ofrecen en el lanzador. Vacío = el que trae el agente por defecto.
    pub fn models(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["opus", "sonnet", "haiku"],
            Agent::Codex => &["gpt-5.6-terra", "gpt-5.5"],
            Agent::Pi | Agent::OpenCode | Agent::Shell => &[],
        }
    }

    /// Comando con el modelo elegido: `claude --model opus`, `codex -m gpt-5.6-terra`, `pi --model …`. Sin modelo, el
    /// de siempre. opencode no tiene flag de modelo en su comando principal (verificado con `opencode --help`): se
    /// lanza sin modelo aunque se pida uno (B6).
    pub fn command_with(self, shell: &str, model: Option<&str>) -> Vec<String> {
        let base = self.command(shell);
        match (model, self) {
            (Some(m), Agent::Claude) => vec![base[0].clone(), "--model".into(), m.into()],
            (Some(m), Agent::Codex) => vec![base[0].clone(), "-m".into(), m.into()],
            (Some(m), Agent::Pi) => vec![base[0].clone(), "--model".into(), m.into()],
            _ => base,
        }
    }
}

/// `Spawn` de un proceso en `cwd`, dentro de `workspace` si lo hay (si no, lo deduce el daemon del repo git).
pub fn spawn_request(name: &str, cmd: Vec<String>, cwd: &str, workspace: Option<String>, rows: u16, cols: u16) -> Request {
    Request::Spawn {
        name: name.to_string(),
        cmd,
        cwd: Some(cwd.to_string()),
        manager: None,
        inject: false,
        rows,
        cols,
        workspace,
        worktree: None,
    }
}

/// Un shell en `cwd`, dentro del workspace dado.
pub fn shell_request(name: &str, shell: &str, cwd: &str, workspace: Option<String>, rows: u16, cols: u16) -> Request {
    spawn_request(name, Agent::Shell.command(shell), cwd, workspace, rows, cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modelo_elegido_va_en_el_comando() {
        assert_eq!(Agent::Claude.command_with("/bin/zsh", Some("opus")), vec!["claude", "--model", "opus"]);
        assert_eq!(Agent::Codex.command_with("/bin/zsh", Some("gpt-5.5")), vec!["codex", "-m", "gpt-5.5"]);
        assert_eq!(Agent::Claude.command_with("/bin/zsh", None), vec!["claude"]);
        assert!(Agent::Shell.models().is_empty());
        assert_eq!(Agent::Shell.command_with("/bin/zsh", Some("x")), vec!["/bin/zsh"]);
        // B6: opencode no acepta modelo en su comando principal; no se le pasa un flag inventado.
        assert_eq!(Agent::OpenCode.command_with("/bin/zsh", Some("x")), vec!["opencode"]);
    }

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
        match shell_request("shell", "/bin/zsh", "/Users/x/proyecto", None, 40, 140) {
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

    /// Cada agente lanza su comando; el shell usa el del usuario; el workspace se conserva.
    #[test]
    fn cada_agente_lanza_su_comando() {
        assert_eq!(Agent::Claude.command("/bin/zsh"), vec!["claude"]);
        assert_eq!(Agent::Codex.command("/bin/zsh"), vec!["codex"]);
        assert_eq!(Agent::Pi.command("/bin/zsh"), vec!["pi"]);
        assert_eq!(Agent::OpenCode.command("/bin/zsh"), vec!["opencode"]);
        assert_eq!(Agent::Shell.command("/bin/zsh"), vec!["/bin/zsh"]);
        match spawn_request("claude-2", Agent::Claude.command("/bin/zsh"), "/x", Some("repo/main".into()), 40, 140) {
            Request::Spawn { name, cmd, workspace, .. } => {
                assert_eq!(name, "claude-2");
                assert_eq!(cmd, vec!["claude"]);
                assert_eq!(workspace.as_deref(), Some("repo/main"));
            }
            other => panic!("{other:?}"),
        }
    }
}

/// Rol de un agente dentro de su workspace. El manager recibe los resultados de los demás; un worker o un revisor
/// trabaja para un manager; un agente libre no tiene relación con ninguno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Manager,
    Worker,
    Reviewer,
    Libre,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Manager, Role::Worker, Role::Reviewer, Role::Libre];

    /// Nombre estable (se guarda en las preferencias y en `team.toml`).
    pub fn label(self) -> &'static str {
        match self {
            Role::Manager => "manager",
            Role::Worker => "worker",
            Role::Reviewer => "reviewer",
            Role::Libre => "libre",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.label() == text)
    }

    /// Un rol que trabaja para un manager (y por tanto necesita uno).
    pub fn needs_manager(self) -> bool {
        matches!(self, Role::Worker | Role::Reviewer)
    }
}

/// Dónde trabaja un agente: en la carpeta del workspace, o en un worktree propio con su rama.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    Shared,
    Worktree,
}

impl Isolation {
    pub const ALL: [Isolation; 2] = [Isolation::Shared, Isolation::Worktree];

    pub fn label(self) -> &'static str {
        match self {
            Isolation::Shared => "misma carpeta",
            Isolation::Worktree => "worktree propio",
        }
    }

    pub fn parse(text: &str) -> Option<Isolation> {
        match text {
            "shared" => Some(Isolation::Shared),
            "worktree" => Some(Isolation::Worktree),
            _ => None,
        }
    }
}

/// Nombre que puede dar un agente: letras, cifras, `-` y `_`, de 1 a 32 caracteres. Es también la rama del worktree.
pub fn valid_agent_name(name: &str) -> bool {
    (1..=32).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Petición `Spawn` de un agente con rol y aislamiento: `manager` recibe sus resultados; con `worktree`, el daemon
/// crea la rama de ese nombre y arranca el panel dentro.
#[allow(clippy::too_many_arguments)]
pub fn agent_request(
    name: &str,
    cmd: Vec<String>,
    cwd: &str,
    workspace: Option<String>,
    manager: Option<String>,
    worktree: Option<String>,
    rows: u16,
    cols: u16,
) -> Request {
    Request::Spawn {
        name: name.to_string(),
        cmd,
        cwd: Some(cwd.to_string()),
        manager,
        inject: false,
        rows,
        cols,
        workspace,
        worktree,
    }
}

#[cfg(test)]
mod agente_tests {
    use super::*;

    /// Rol y aislamiento: nombres estables que van y vuelven; un valor desconocido no entra.
    #[test]
    fn roles_y_aislamiento_con_nombres_estables() {
        for r in Role::ALL {
            assert_eq!(Role::parse(r.label()), Some(r));
        }
        assert_eq!(Role::parse("jefe"), None);
        assert!(Role::Worker.needs_manager() && Role::Reviewer.needs_manager());
        assert!(!Role::Manager.needs_manager() && !Role::Libre.needs_manager());
        assert_eq!(Isolation::parse("worktree"), Some(Isolation::Worktree));
        assert_eq!(Isolation::parse("root"), None);
    }

    /// Nombres de agente: solo letras, cifras, `-` y `_`; vacío o con espacios o símbolos, no.
    #[test]
    fn nombres_de_agente_acotados() {
        assert!(valid_agent_name("claude-manager_2"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name("con espacio"));
        assert!(!valid_agent_name("a;rm -rf"));
        assert!(!valid_agent_name(&"x".repeat(33)));
    }

    /// La petición lleva el manager y el worktree tal cual.
    #[test]
    fn la_peticion_lleva_manager_y_worktree() {
        let r = agent_request("rev", vec!["claude".into()], "/tmp/x", Some("ws".into()), Some("jefe".into()), Some("rev".into()), 40, 140);
        match r {
            Request::Spawn { name, manager, worktree, workspace, .. } => {
                assert_eq!(name, "rev");
                assert_eq!(manager.as_deref(), Some("jefe"));
                assert_eq!(worktree.as_deref(), Some("rev"));
                assert_eq!(workspace.as_deref(), Some("ws"));
            }
            other => panic!("{other:?}"),
        }
    }
}
