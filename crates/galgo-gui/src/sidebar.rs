//! Árbol de la barra lateral: Proyecto > Workspace > paneles, y la lista «Te necesitan». Lógica pura.

use galgo_proto::{PaneInfo, Status};
use std::collections::BTreeMap;

/// Un workspace de la barra: su etiqueta completa ("repo/rama", o `None`) y el nombre que se muestra.
pub struct Workspace<'a> {
    pub label: Option<String>,
    pub name: String,
    pub panes: Vec<&'a PaneInfo>,
}

/// Un proyecto: el repo (lo que va antes de la barra en la etiqueta del workspace).
pub struct Project<'a> {
    pub name: String,
    pub workspaces: Vec<Workspace<'a>>,
}

/// "repo/rama" → ("repo", "rama"). Sin barra, el proyecto es la etiqueta y el workspace también. Sin etiqueta, el
/// proyecto se llama «sin proyecto».
pub fn split_label(label: Option<&str>) -> (String, String) {
    match label {
        None => ("sin proyecto".into(), "sin workspace".into()),
        Some(l) => match l.split_once('/') {
            Some((repo, branch)) => (repo.to_string(), branch.to_string()),
            None => (l.to_string(), l.to_string()),
        },
    }
}

/// Proyectos por nombre, y dentro de cada uno sus workspaces por etiqueta. «sin proyecto» va al final.
pub fn tree(panes: &[PaneInfo]) -> Vec<Project<'_>> {
    let mut by_project: BTreeMap<(bool, String), BTreeMap<String, Workspace<'_>>> = BTreeMap::new();
    for p in panes {
        let (project, name) = split_label(p.workspace.as_deref());
        let sortable = (p.workspace.is_none(), project);
        let key = p.workspace.clone().unwrap_or_default();
        let ws = by_project.entry(sortable).or_default().entry(key).or_insert_with(|| Workspace {
            label: p.workspace.clone(),
            name,
            panes: Vec::new(),
        });
        ws.panes.push(p);
    }
    by_project
        .into_iter()
        .map(|((_, name), workspaces)| Project { name, workspaces: workspaces.into_values().collect() })
        .collect()
}

/// Todos los workspaces en el orden en que aparecen en la barra: el orden de los atajos Cmd+1..9.
pub fn workspaces(panes: &[PaneInfo]) -> Vec<Workspace<'_>> {
    tree(panes).into_iter().flat_map(|p| p.workspaces).collect()
}


/// Prioridad de un agente en la lista «agents»: bloqueado primero, luego error, trabajando, terminado e inactivo.
pub fn priority(status: Status) -> u8 {
    match status {
        Status::Blocked => 0,
        Status::Failed | Status::Exited => 1,
        Status::Working => 2,
        Status::Done => 3,
        Status::Idle => 4,
    }
}

/// Todos los paneles en el orden de «agents»: por prioridad y, a igual prioridad, por id.
pub fn agents_sorted(panes: &[PaneInfo]) -> Vec<&PaneInfo> {
    let mut v: Vec<&PaneInfo> = panes.iter().collect();
    v.sort_by_key(|p| (priority(p.status), p.id));
    v
}

/// Proveedor del agente: el que midió el uso si lo hay; si no, el nombre del programa que lanza el panel.
pub fn provider(p: &PaneInfo) -> String {
    if let Some(u) = &p.usage {
        return u.agent.clone();
    }
    let program = p.cmd.first().map(String::as_str).unwrap_or("");
    let base = program.rsplit('/').next().unwrap_or("");
    match base {
        "claude" | "codex" | "pi" | "opencode" => base.to_string(),
        _ => "shell".to_string(),
    }
}

/// Modelo del agente, si se conoce (lo da el uso medido).
pub fn model(p: &PaneInfo) -> Option<String> {
    p.usage.as_ref().and_then(|u| u.model.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use galgo_proto::Status;

    fn pane(id: u32, ws: Option<&str>, status: Status) -> PaneInfo {
        PaneInfo {
            id,
            name: format!("p{id}"),
            status,
            manager: None,
            cmd: vec![],
            unacked: 0,
            oldest_unacked_secs: None,
            orphan: false,
            waiting_on: None,
            waiting_secs: None,
            workspace: ws.map(str::to_string),
            cwd: None,
            usage: None,
            title: None,
        }
    }

    #[test]
    fn repo_y_rama() {
        assert_eq!(split_label(Some("inventia/cotizacion")), ("inventia".into(), "cotizacion".into()));
        assert_eq!(split_label(Some("nexo")), ("nexo".into(), "nexo".into()));
        assert_eq!(split_label(None).0, "sin proyecto");
    }

    /// Un proyecto reúne sus workspaces aunque sus nombres de rama se ordenen de otra forma; «sin proyecto» al final.
    #[test]
    fn proyectos_agrupan_sus_workspaces_y_sin_proyecto_va_al_final() {
        let panes = vec![
            pane(1, Some("a/main"), Status::Idle),
            pane(2, None, Status::Idle),
            pane(3, Some("a-b/dev"), Status::Idle),
            pane(4, Some("a/feat"), Status::Working),
        ];
        let arbol = tree(&panes);
        let nombres: Vec<&str> = arbol.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(nombres, vec!["a", "a-b", "sin proyecto"]);
        let de_a: Vec<&str> = arbol[0].workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(de_a, vec!["feat", "main"], "dentro del proyecto, por rama");
        // El orden de los atajos es el de la barra.
        let orden: Vec<u32> = workspaces(&panes).iter().flat_map(|w| w.panes.iter().map(|p| p.id)).collect();
        assert_eq!(orden, vec![4, 1, 3, 2]);
    }


    #[test]
    fn agentes_por_prioridad() {
        let panes = vec![pane(1, None, Status::Idle), pane(2, None, Status::Working), pane(3, None, Status::Blocked), pane(4, None, Status::Done)];
        let ids: Vec<u32> = agents_sorted(&panes).iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![3, 2, 4, 1], "bloqueado, trabajando, terminado, inactivo");
    }

    #[test]
    fn proveedor_desde_el_comando_o_el_uso() {
        let mut p = pane(1, None, Status::Idle);
        p.cmd = vec!["/usr/local/bin/codex".into(), "--x".into()];
        assert_eq!(provider(&p), "codex");
        p.cmd = vec!["/bin/zsh".into()];
        assert_eq!(provider(&p), "shell");
        assert_eq!(model(&p), None);
    }
}
