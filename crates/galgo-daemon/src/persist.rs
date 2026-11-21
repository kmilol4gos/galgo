//! Estado del daemon entre reinicios: inbox sin confirmar y paneles (sin su proceso).
//!
//! Los paneles no sobreviven: su PTY muere con el daemon. Lo que sí sobrevive es el inbox, para que
//! un resultado no se pierda, y la lista de paneles, para avisar a sus managers de que ya no existen.

use super::{InboxMsg, Pane, State};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Versión del formato. Un archivo de otra versión se ignora con aviso.
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    /// Ejecución del daemon que escribió el archivo. El siguiente arranque usa `epoch + 1`.
    pub epoch: u64,
    pub next_pane: u32,
    pub next_msg: u64,
    pub panes: Vec<SavedPane>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedPane {
    pub id: u32,
    pub name: String,
    pub cmd: Vec<String>,
    pub manager: Option<u32>,
    /// El panel tenía proceso al guardar. Al restaurar, su manager recibe el aviso de que ya no existe.
    pub alive: bool,
    pub inbox: Vec<SavedMsg>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedMsg {
    pub id: u64,
    pub from: u32,
    pub from_name: String,
    pub kind: String,
    pub summary: String,
    /// Segundos desde la época Unix, para que la edad sobreviva al reinicio.
    pub created_unix: u64,
}

/// Ruta del estado: `GALGO_STATE`, o `state.json` junto al socket.
pub fn state_path(socket: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("GALGO_STATE") {
        return p.into();
    }
    socket.parent().unwrap_or_else(|| Path::new(".")).join("state.json")
}

/// Carga el estado. Nunca falla: un archivo ilegible, corrupto o de otra versión se ignora con aviso.
pub fn load(path: &Path) -> Option<Saved> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("galgod: estado ignorado ({}): {e}", path.display());
            return None;
        }
    };
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(s) if s.version == VERSION => Some(s),
        Ok(s) => {
            eprintln!("galgod: estado ignorado ({}): versión {} no soportada", path.display(), s.version);
            None
        }
        Err(e) => {
            eprintln!("galgod: estado ignorado ({}): {e}", path.display());
            None
        }
    }
}

/// Escritura atómica: temporal con permisos 0600 y `rename`. Un corte a medias no deja un archivo roto.
pub fn write(path: &Path, saved: &Saved) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(saved)?;
    let tmp = path.with_file_name(format!("state.json.tmp.{}", std::process::id()));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("no pude crear {}", tmp.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("no pude reemplazar {}", path.display()))?;
    Ok(())
}

/// Lo que se guarda del estado actual: solo los mensajes sin confirmar.
pub fn snapshot(st: &State) -> Saved {
    let now_unix = unix_now();
    Saved {
        version: VERSION,
        epoch: st.epoch,
        next_pane: st.next_pane,
        next_msg: st.next_msg,
        panes: st
            .panes
            .values()
            .map(|p| SavedPane {
                id: p.id,
                name: p.name.clone(),
                cmd: p.cmd.clone(),
                manager: p.manager,
                alive: !p.ghost && !p.exited,
                inbox: p
                    .inbox
                    .iter()
                    .filter(|m| !m.acked)
                    .map(|m| SavedMsg {
                        id: m.id,
                        from: m.from,
                        from_name: m.from_name.clone(),
                        kind: m.kind.to_string(),
                        summary: m.summary.clone(),
                        created_unix: now_unix.saturating_sub(m.created.elapsed().as_secs()),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Carga `saved` en `st`: los paneles vuelven como fantasmas y sus managers reciben el aviso.
pub fn restore(st: &mut State, saved: Saved) {
    st.next_pane = saved.next_pane;
    st.next_msg = saved.next_msg;
    let now_unix = unix_now();
    let mut lost = Vec::new();
    for sp in saved.panes {
        if sp.alive {
            lost.push((sp.id, sp.name.clone(), sp.manager));
        }
        let inbox = sp.inbox.into_iter().map(|m| InboxMsg {
            id: m.id,
            from: m.from,
            from_name: m.from_name,
            kind: static_kind(&m.kind),
            summary: m.summary,
            created: instant_for(now_unix, m.created_unix),
            acked: false,
            claimed: false,
            hold_until: None,
            last_inject: None,
        });
        let pane = Pane::ghost(sp.id, sp.name, sp.cmd, sp.manager, inbox.collect());
        st.panes.insert(sp.id, pane);
    }
    // El aviso va después de cargar todos los paneles, para que el nombre del que se fue esté disponible.
    for (id, name, manager) in lost {
        if let Some(m) = manager {
            if st.panes.contains_key(&m) {
                st.push(m, id, "exited", format!("el daemon se reinició; el panel {name} ya no existe"));
            }
        }
    }
    // Los paneles restaurados no tienen proceso: ya no están "vivos" en el próximo guardado.
    for p in st.panes.values_mut() {
        p.exited = true;
    }
}

fn static_kind(kind: &str) -> &'static str {
    match kind {
        "done" => "done",
        "failed" => "failed",
        "blocked" => "blocked",
        "exited" => "exited",
        _ => "done",
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Instante de creación equivalente a una marca de tiempo Unix guardada.
fn instant_for(now_unix: u64, created_unix: u64) -> Instant {
    let age = Duration::from_secs(now_unix.saturating_sub(created_unix));
    Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("galgo-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    fn saved_with_inbox() -> Saved {
        Saved {
            version: VERSION,
            epoch: 3,
            next_pane: 5,
            next_msg: 9,
            panes: vec![
                SavedPane { id: 1, name: "mgr".into(), cmd: vec!["bash".into()], manager: None, alive: true, inbox: vec![] },
                SavedPane {
                    id: 2,
                    name: "w".into(),
                    cmd: vec!["claude".into()],
                    manager: Some(1),
                    alive: true,
                    inbox: vec![SavedMsg { id: (2 << 32) | 4, from: 2, from_name: "w".into(), kind: "done".into(), summary: "hecho".into(), created_unix: unix_now() }],
                },
            ],
        }
    }

    #[test]
    fn ausente_corrupto_o_de_otra_version_se_ignora() {
        let path = tmp("ignora");
        assert!(load(&path).is_none(), "sin archivo: nada que cargar, sin aviso");
        std::fs::write(&path, "{no es json").unwrap();
        assert!(load(&path).is_none(), "corrupto: se ignora");
        std::fs::write(&path, r#"{"version":99,"epoch":1,"next_pane":0,"next_msg":0,"panes":[]}"#).unwrap();
        assert!(load(&path).is_none(), "otra versión: se ignora");
    }

    #[test]
    fn escritura_atomica_con_permisos_privados() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("permisos");
        write(&path, &saved_with_inbox()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(load(&path).unwrap().epoch, 3);
    }

    /// Un panel vivo vuelve como fantasma, su manager recibe el aviso y el mensaje sigue en el inbox.
    #[test]
    fn restaura_inbox_y_avisa_al_manager() {
        let mut st = State { epoch: 4, ..State::default() };
        restore(&mut st, saved_with_inbox());
        let mgr = &st.panes[&1];
        assert!(mgr.ghost && mgr._master.is_none());
        let inbox = &st.panes[&1].inbox;
        assert_eq!(inbox.len(), 1, "solo el aviso de que w ya no existe");
        assert!(inbox[0].summary.contains("el panel w ya no existe"));
        assert_eq!(st.panes[&2].inbox[0].summary, "hecho");
        assert_eq!(st.panes[&2].inbox[0].id, (2 << 32) | 4);
        assert_eq!(st.next_pane, 5);
        // El aviso de arranque consume un id: el contador sigue donde estaba.
        assert_eq!(st.next_msg, 10);
    }
}
