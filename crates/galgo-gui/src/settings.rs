//! Ajustes (Cmd+,): secciones y órdenes de la CLI que usa la ventana. Lógica pura: no lee ni escribe la configuración
//! de los agentes; eso lo hace `galgo hooks` cuando el usuario pulsa un botón.

use std::path::PathBuf;

/// Secciones del panel de ajustes, en el orden de la barra izquierda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    General,
    Interface,
    Terminal,
    Shortcuts,
    Integrations,
    Notifications,
    Usage,
    Developer,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::General,
        Section::Interface,
        Section::Terminal,
        Section::Shortcuts,
        Section::Integrations,
        Section::Notifications,
        Section::Usage,
        Section::Developer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Interface => "Interface",
            Section::Terminal => "Terminal",
            Section::Shortcuts => "Shortcuts",
            Section::Integrations => "Integrations",
            Section::Notifications => "Notifications",
            Section::Usage => "Usage",
            Section::Developer => "Developer",
        }
    }
}

/// Agentes con integración de hooks en la CLI.
pub const AGENTS: [&str; 3] = ["claude", "codex", "pi"];

/// Argumentos de `galgo hooks status <agente>`.
pub fn status_args(agent: &str) -> Vec<String> {
    vec!["hooks".into(), "status".into(), agent.into()]
}

/// Argumentos de `galgo hooks install <agente>`: primero `--dry-run` (el plan), y `--yes` solo si el usuario lo pide.
pub fn install_args(agent: &str, apply: bool) -> Vec<String> {
    let mut v: Vec<String> = vec!["hooks".into(), "install".into(), agent.into()];
    v.push(if apply { "--yes".into() } else { "--dry-run".into() });
    v
}

/// Atajos de la ventana, tal como se muestran en la sección Shortcuts.
pub const SHORTCUTS: [(&str, &str); 14] = [
    ("Cmd+N", "Nuevo workspace"),
    ("Cmd+T", "Nueva pestaña en el workspace"),
    ("Cmd+W", "Cerrar el panel enfocado"),
    ("Cmd+1 … Cmd+9", "Ir al workspace n"),
    ("Cmd+Shift+U", "Siguiente panel que necesita atención"),
    ("Cmd+Shift+L", "Lanzar agente"),
    ("Cmd+K  ·  Cmd+Shift+P", "Paleta de comandos"),
    ("Cmd+D  ·  Cmd+Shift+D", "Dividir a la derecha · hacia abajo"),
    ("Cmd+[  ·  Cmd+]", "Pestaña anterior · siguiente"),
    ("Cmd+Opt+flechas", "Mover el foco entre paneles"),
    ("Cmd+Shift+Enter", "Maximizar el panel enfocado"),
    ("Cmd+,", "Ajustes"),
    ("Cmd+C  ·  Cmd+V", "Copiar selección · pegar"),
    ("Esc", "Cerrar paleta, lanzador o ajustes"),
];

/// Ruta del binario que escribirá los hooks. Los hooks guardan la ruta del propio binario: si es una copia dentro
/// de la `.app` (App Translocation, o un bundle que se mueve), desaparece y los hooks fallan en silencio. Por eso
/// se prefiere `~/.galgo/bin/galgo` y se rechaza cualquier ruta dentro de un `.app`.
pub fn hooks_binary() -> Result<String, String> {
    if let Some(bin) = std::env::var_os("GALGO_BIN") {
        return checked_binary(&PathBuf::from(bin));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let stable = PathBuf::from(home).join(".galgo/bin/galgo");
        if stable.is_file() {
            return checked_binary(&stable);
        }
    }
    let found = std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("galgo")).find(|c| c.is_file()))
        .ok_or_else(|| "no encuentro galgo: instálalo en ~/.galgo/bin/galgo".to_string())?;
    checked_binary(&found)
}

/// Acepta una ruta fuera de cualquier `.app` y de App Translocation; el resto se rechaza con el motivo.
pub fn checked_binary(path: &std::path::Path) -> Result<String, String> {
    let text = path.to_string_lossy().into_owned();
    if text.contains("AppTranslocation") || text.contains(".app/Contents/") {
        return Err(format!("{text} está dentro de la app y puede ser temporal; instala galgo en ~/.galgo/bin/galgo"));
    }
    Ok(text)
}

/// Ejecuta `galgo` con `args` y devuelve su salida (stdout y stderr juntos), con el binario de `hooks_binary`.
pub fn run_galgo(args: &[String]) -> Result<String, String> {
    let bin = hooks_binary()?;
    let out = std::process::Command::new(&bin).args(args).output().map_err(|e| format!("no pude ejecutar {bin}: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim().to_string();
    if out.status.success() {
        Ok(if text.is_empty() { "listo".into() } else { text })
    } else {
        Err(if text.is_empty() { "la CLI terminó con error".into() } else { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secciones_con_nombres_unicos() {
        let mut nombres: Vec<&str> = Section::ALL.iter().map(|s| s.label()).collect();
        nombres.sort();
        nombres.dedup();
        assert_eq!(nombres.len(), Section::ALL.len());
    }

    /// Instalar pide primero el plan (`--dry-run`); `--yes` solo cuando el usuario confirma.
    #[test]
    fn instalar_muestra_el_plan_antes_de_escribir() {
        assert_eq!(install_args("claude", false), vec!["hooks", "install", "claude", "--dry-run"]);
        assert_eq!(install_args("claude", true), vec!["hooks", "install", "claude", "--yes"]);
        assert_eq!(status_args("codex"), vec!["hooks", "status", "codex"]);
    }

    #[test]
    fn atajos_incluyen_los_basicos() {
        assert!(SHORTCUTS.iter().any(|(k, _)| *k == "Cmd+N"));
        assert!(SHORTCUTS.iter().any(|(k, _)| *k == "Cmd+,"));
    }

    /// A2 (review-14): los hooks no se escriben con una copia dentro de la `.app`; la ruta de `~/.galgo/bin` sí vale.
    #[test]
    fn ruta_de_hooks_fuera_de_la_app() {
        assert!(checked_binary(std::path::Path::new("/private/var/folders/x/AppTranslocation/U/d/galgo.app/Contents/MacOS/galgo")).is_err());
        assert!(checked_binary(std::path::Path::new("/Applications/galgo.app/Contents/MacOS/galgo")).is_err());
        assert_eq!(checked_binary(std::path::Path::new("/Users/x/.galgo/bin/galgo")).unwrap(), "/Users/x/.galgo/bin/galgo");
    }
}
