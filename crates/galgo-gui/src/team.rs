//! `.galgo/team.toml`: la plantilla de un equipo de agentes del repo. Parser estricto y puro: no ejecuta nada.
//! Un agente solo puede ser uno de los conocidos (claude, codex, pi, opencode), con un modelo de caracteres seguros,
//! un rol y un aislamiento de la lista cerrada, y un prompt de texto. Lo que no encaja se rechaza entero.

#![allow(dead_code)] // lo usa «Levantar equipo» (siguiente grupo)

use crate::launch::{Agent, Isolation, Role};
use serde::Deserialize;

/// Archivo dentro del repo, relativo a su raíz.
pub const FILE: &str = ".galgo/team.toml";
/// Tamaño máximo del archivo y límites del equipo.
pub const MAX_BYTES: u64 = 16 * 1024;
pub const MAX_AGENTS: usize = 8;
pub const MAX_PROMPT: usize = 2000;
const MAX_MODEL: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Team {
    pub name: String,
    pub agents: Vec<TeamAgent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TeamAgent {
    pub name: String,
    pub provider: Agent,
    pub model: Option<String>,
    pub role: Role,
    pub isolation: Isolation,
    pub prompt: Option<String>,
}

/// Forma del TOML tal cual. Cualquier clave que no esté aquí es un error (`deny_unknown_fields`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTeam {
    name: Option<String>,
    #[serde(default)]
    agent: Vec<RawAgent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: String,
    provider: String,
    model: Option<String>,
    role: Option<String>,
    isolation: Option<String>,
    prompt: Option<String>,
}

/// Proveedores que se pueden lanzar desde un equipo: solo los agentes conocidos, nunca un comando libre.
fn provider(text: &str) -> Option<Agent> {
    match text {
        "claude" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        "pi" => Some(Agent::Pi),
        "opencode" => Some(Agent::OpenCode),
        _ => None,
    }
}

/// Modelo con caracteres seguros (letras, cifras, `.`, `_`, `-`, `:`, `/`): viaja como un argumento, sin shell.
fn valid_model(text: &str) -> bool {
    (1..=MAX_MODEL).contains(&text.chars().count()) && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'))
}

/// Prompt de texto: sin caracteres de control (salvo saltos de línea y tabuladores), y acotado.
fn valid_prompt(text: &str) -> bool {
    text.chars().count() <= MAX_PROMPT && text.chars().all(|c| !c.is_control() || matches!(c, '\n' | '\t'))
}

/// Lee y valida un equipo. Un error describe el primer problema, con el nombre del agente si lo hay.
pub fn parse(text: &str) -> Result<Team, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err(format!("team.toml pesa más de {} KiB", MAX_BYTES / 1024));
    }
    let raw: RawTeam = toml::from_str(text).map_err(|e| format!("team.toml no es válido: {e}"))?;
    let name = raw.name.unwrap_or_else(|| "equipo".into());
    if !crate::launch::valid_agent_name(&name) {
        return Err("el nombre del equipo solo puede tener letras, cifras, - y _ (hasta 32)".into());
    }
    if raw.agent.is_empty() {
        return Err("el equipo no tiene agentes".into());
    }
    if raw.agent.len() > MAX_AGENTS {
        return Err(format!("el equipo tiene más de {MAX_AGENTS} agentes"));
    }
    let mut agents: Vec<TeamAgent> = Vec::new();
    for a in raw.agent {
        if !crate::launch::valid_agent_name(&a.name) {
            return Err(format!("nombre de agente no válido: {:?}", a.name));
        }
        if agents.iter().any(|x| x.name == a.name) {
            return Err(format!("nombre de agente repetido: {}", a.name));
        }
        let Some(provider) = provider(&a.provider) else {
            return Err(format!("{}: proveedor desconocido {:?} (claude, codex, pi u opencode)", a.name, a.provider));
        };
        if let Some(m) = &a.model {
            if !valid_model(m) {
                return Err(format!("{}: modelo no válido {:?}", a.name, m));
            }
        }
        let role = match a.role.as_deref() {
            None => Role::Libre,
            Some(r) => Role::parse(r).ok_or_else(|| format!("{}: rol desconocido {:?} (manager, worker, reviewer o libre)", a.name, r))?,
        };
        let isolation = match a.isolation.as_deref() {
            None => Isolation::Shared,
            Some(i) => Isolation::parse(i).ok_or_else(|| format!("{}: aislamiento desconocido {:?} (shared o worktree)", a.name, i))?,
        };
        if let Some(p) = &a.prompt {
            if !valid_prompt(p) {
                return Err(format!("{}: el prompt tiene caracteres de control o pasa de {MAX_PROMPT} caracteres", a.name));
            }
        }
        agents.push(TeamAgent { name: a.name, provider, model: a.model, role, isolation, prompt: a.prompt });
    }
    if agents.iter().filter(|a| a.role == Role::Manager).count() > 1 {
        return Err("el equipo tiene más de un manager".into());
    }
    Ok(Team { name, agents })
}

/// Huella del contenido (FNV-1a de 64 bits, estable entre versiones del programa). Sirve para saber si el equipo
/// que se confirmó es el mismo que hay ahora: si cambia, vuelve a pedir confirmación.
pub fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Equipos integrados: plantillas que escribe la propia app (no vienen de ningún repo), así que no piden confianza.
/// Cada agente es uno de los conocidos; los workers van en su propio worktree para no pisarse.
pub fn presets() -> Vec<(&'static str, Team)> {
    let a = |name: &str, provider: Agent, role: Role, isolation: Isolation| TeamAgent {
        name: name.into(),
        provider,
        model: None,
        role,
        isolation,
        prompt: None,
    };
    vec![
        (
            "Claude manager + Codex worker + Claude revisor",
            Team {
                name: "claude-codex-revisor".into(),
                agents: vec![
                    a("jefe", Agent::Claude, Role::Manager, Isolation::Shared),
                    a("codex", Agent::Codex, Role::Worker, Isolation::Worktree),
                    a("revisor", Agent::Claude, Role::Reviewer, Isolation::Shared),
                ],
            },
        ),
        (
            "Claude manager + PI worker",
            Team {
                name: "claude-pi".into(),
                agents: vec![a("jefe", Agent::Claude, Role::Manager, Isolation::Shared), a("pi", Agent::Pi, Role::Worker, Isolation::Worktree)],
            },
        ),
        (
            "opencode + Claude revisor",
            Team {
                name: "opencode-revisor".into(),
                agents: vec![a("opencode", Agent::OpenCode, Role::Libre, Isolation::Worktree), a("revisor", Agent::Claude, Role::Reviewer, Isolation::Shared)],
            },
        ),
    ]
}

/// La confianza que el usuario dio a un equipo: el repo y la huella confirmada.
pub fn trusted(trust: &serde_json::Value, repo: &str, fp: &str) -> bool {
    trust.get(repo).and_then(serde_json::Value::as_str) == Some(fp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUENO: &str = r#"
name = "cotizacion"

[[agent]]
name = "jefe"
provider = "claude"
model = "opus"
role = "manager"
prompt = "Coordina la cotización. Delega en los workers."

[[agent]]
name = "codex-1"
provider = "codex"
model = "gpt-5.5"
role = "worker"
isolation = "worktree"

[[agent]]
name = "revisor"
provider = "claude"
role = "reviewer"
"#;

    /// Un equipo válido se lee con sus valores y con los por defecto donde no se pone nada.
    #[test]
    fn equipo_valido_se_lee() {
        let t = parse(BUENO).unwrap();
        assert_eq!(t.name, "cotizacion");
        assert_eq!(t.agents.len(), 3);
        assert_eq!(t.agents[0].role, Role::Manager);
        assert_eq!(t.agents[1].isolation, Isolation::Worktree);
        assert_eq!(t.agents[2].isolation, Isolation::Shared, "por defecto, misma carpeta");
        assert_eq!(t.agents[2].model, None, "por defecto, el modelo del agente");
    }

    /// Lo desconocido se rechaza: claves de más, proveedores fuera de la lista, roles y aislamientos inventados.
    #[test]
    fn lo_desconocido_se_rechaza() {
        assert!(parse("[[agent]]\nname=\"a\"\nprovider=\"claude\"\ncomando=\"rm -rf /\"").is_err(), "clave de más");
        assert!(parse("[[agent]]\nname=\"a\"\nprovider=\"bash\"").is_err(), "proveedor que no es un agente conocido");
        assert!(parse("[[agent]]\nname=\"a\"\nprovider=\"claude\"\nrole=\"jefe\"").is_err(), "rol inventado");
        assert!(parse("[[agent]]\nname=\"a\"\nprovider=\"claude\"\nisolation=\"root\"").is_err(), "aislamiento inventado");
        assert!(parse("").is_err(), "sin agentes");
    }

    /// Modelos y prompts: caracteres seguros y tamaño acotado; un nombre con espacios no entra.
    #[test]
    fn modelos_prompts_y_nombres_acotados() {
        assert!(parse("[[agent]]\nname=\"a\"\nprovider=\"claude\"\nmodel=\"opus; rm -rf\"").is_err());
        assert!(parse("[[agent]]\nname=\"a\"\nprovider=\"claude\"\nprompt=\"hola\u{7}\"").is_err(), "control en el prompt");
        assert!(parse(&format!("[[agent]]\nname=\"a\"\nprovider=\"claude\"\nprompt=\"{}\"", "x".repeat(MAX_PROMPT + 1))).is_err());
        assert!(parse("[[agent]]\nname=\"con espacio\"\nprovider=\"claude\"").is_err());
        assert!(parse("name=\"dos\"\n[[agent]]\nname=\"a\"\nprovider=\"claude\"\n[[agent]]\nname=\"a\"\nprovider=\"codex\"").is_err(), "nombre repetido");
    }

    /// Límites: como mucho un manager y un número máximo de agentes.
    #[test]
    fn limites_de_manager_y_agentes() {
        let dos_managers = "[[agent]]\nname=\"a\"\nprovider=\"claude\"\nrole=\"manager\"\n[[agent]]\nname=\"b\"\nprovider=\"codex\"\nrole=\"manager\"";
        assert!(parse(dos_managers).is_err());
        let muchos: String = (0..=MAX_AGENTS).map(|i| format!("[[agent]]\nname=\"a{i}\"\nprovider=\"claude\"\n")).collect();
        assert!(parse(&muchos).is_err());
    }

    /// La huella cambia con el contenido; la confianza solo vale para la huella confirmada.
    /// Cada preset integrado es un equipo válido (pasa el mismo parser que un team.toml) y tiene como mucho un manager.
    #[test]
    fn los_presets_son_equipos_validos() {
        for (_, team) in presets() {
            assert!(crate::launch::valid_agent_name(&team.name));
            assert!(team.agents.iter().filter(|a| a.role == Role::Manager).count() <= 1);
            assert!(!team.agents.is_empty() && team.agents.len() <= MAX_AGENTS);
        }
    }

    #[test]
    fn huella_y_confianza() {
        assert_ne!(fingerprint(BUENO), fingerprint(&format!("{BUENO}\n# cambio")));
        assert_eq!(fingerprint(BUENO), fingerprint(BUENO));
        let mut trust = serde_json::json!({});
        let fp = fingerprint(BUENO);
        assert!(!trusted(&trust, "/repo", &fp));
        trust["/repo"] = serde_json::json!(fp.clone());
        assert!(trusted(&trust, "/repo", &fp));
        assert!(!trusted(&trust, "/repo", &fingerprint("otro")), "un equipo cambiado pide confirmación otra vez");
        assert!(!trusted(&trust, "/otro", &fp), "la confianza es por repo");
    }
}
