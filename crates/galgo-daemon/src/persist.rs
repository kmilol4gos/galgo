//! Estado del daemon entre reinicios: inbox sin confirmar y paneles (sin su proceso).
//!
//! Los paneles no sobreviven: su PTY muere con el daemon. Lo que sí sobrevive es el inbox, para que
//! un resultado no se pierda, y la lista de paneles, para avisar a sus managers de que ya no existen.

use super::{InboxMsg, Pane, State};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Versión del formato. Un archivo de otra versión se pone a un lado con aviso.
pub const VERSION: u32 = 1;
/// El id de mensaje es `epoch << 32 | contador`: con un epoch de 20 bits el máximo es 2^52 − 1, por debajo de
/// 2^53, así que cabe exacto en un número de JavaScript (Node, Claude Code, una GUI web).
pub const EPOCH_BITS: u32 = 20;
pub const EPOCH_MAX: u64 = (1 << EPOCH_BITS) - 1;
/// Límite de precisión de un número en JavaScript: ningún id puede llegar aquí.
pub const ID_LIMIT: u64 = 1 << 53;

/// Tamaño máximo aceptado al cargar: un archivo mayor no es un estado de galgo.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Largo máximo de un resumen guardado. Un `done --summary` enorme no engorda cada escritura.
const MAX_SUMMARY: usize = 4096;

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    /// Ejecución del daemon que escribió el archivo. Ver `epoch_for`.
    pub epoch: u64,
    pub next_pane: u32,
    pub next_msg: u64,
    pub panes: Vec<SavedPane>,
    /// Tareas delegadas. Un estado anterior no las tiene: se carga sin tareas.
    #[serde(default)]
    pub tasks: Vec<SavedTask>,
    #[serde(default)]
    pub next_task: u64,
}

/// Una tarea delegada tal como se guarda. Ver `Task` en el daemon.
#[derive(Serialize, Deserialize)]
pub struct SavedTask {
    pub id: u64,
    pub from: u32,
    pub to: u32,
    pub from_name: String,
    pub to_name: String,
    pub workspace: Option<String>,
    pub text: String,
    pub context: Option<String>,
    pub status: galgo_proto::TaskStatus,
    pub summary: Option<String>,
    pub files: Vec<String>,
    pub depth: u8,
    pub created_unix: u64,
}

#[derive(Serialize, Deserialize)]
pub struct SavedPane {
    pub id: u32,
    pub name: String,
    pub cmd: Vec<String>,
    pub manager: Option<u32>,
    /// Workspace y directorio. Un estado anterior no los tiene: se carga igual, sin workspace.
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
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

/// Ruta del estado: `GALGO_STATE`, o el socket con extensión `.state.json` (`galgo.sock` →
/// `galgo.state.json`). Así dos daemons en el mismo directorio no comparten archivo.
pub fn state_path(socket: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("GALGO_STATE") {
        return p.into();
    }
    socket.with_extension("state.json")
}

/// Carga el estado. Nunca falla ni se cuelga: si el archivo no es un archivo regular de tamaño razonable,
/// o no se puede leer, o está corrupto o es de otra versión, se ignora con aviso. Los que no sirven se
/// ponen a un lado (`.corrupt-<tiempo>`) en lugar de sobrescribirlos.
/// Estado de la versión anterior (4a1e6ee): `state.json` en el mismo directorio que el socket. Sin
/// `GALGO_STATE`, el archivo nuevo es `galgo.state.json`; el anterior no se buscaba por el nombre del socket.
pub fn legacy_path(socket: &Path) -> Option<PathBuf> {
    if std::env::var_os("GALGO_STATE").is_some() {
        return None;
    }
    Some(socket.parent()?.join("state.json"))
}

/// Carga el estado actual o, si no existe, el de la versión anterior. Si se carga de ahí, devuelve esa
/// ruta: hay que retirarla con `finish_migration` cuando el estado nuevo ya está escrito.
pub fn load_with_migration(state_path: &Path, legacy: Option<&Path>) -> (Option<Saved>, Option<PathBuf>) {
    if state_path.symlink_metadata().is_ok() {
        return (load(state_path), None);
    }
    match legacy {
        Some(old) if old.symlink_metadata().map(|m| m.file_type().is_file()).unwrap_or(false) => {
            (load(old), Some(old.to_path_buf()))
        }
        _ => (None, None),
    }
}

/// Renombra el estado anterior a `state.json.migrated-<segundos>` para que no se vuelva a cargar. Solo
/// si el estado nuevo ya existe. Devuelve la nueva ruta del archivo renombrado.
pub fn finish_migration(old: &Path, state_path: &Path) -> Result<Option<PathBuf>> {
    if !state_path.is_file() || old.symlink_metadata().is_err() {
        return Ok(None);
    }
    let to = old.with_file_name(format!("state.json.migrated-{}", unix_now()));
    std::fs::rename(old, &to).with_context(|| format!("no pude renombrar {}", old.display()))?;
    Ok(Some(to))
}

pub fn load(path: &Path) -> Option<Saved> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("galgod: estado ignorado ({}): {e}", path.display());
            return None;
        }
    };
    if !meta.file_type().is_file() {
        eprintln!("galgod: estado ignorado ({}): no es un archivo regular", path.display());
        return None;
    }
    if meta.len() > MAX_BYTES {
        eprintln!("galgod: estado ignorado ({}): demasiado grande ({} bytes)", path.display(), meta.len());
        return None;
    }
    let mut bytes = Vec::new();
    if let Err(e) = std::fs::File::open(path).and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut bytes)) {
        eprintln!("galgod: estado ignorado ({}): {e}", path.display());
        return None;
    }
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(s) if s.version == VERSION => Some(s),
        Ok(s) => {
            eprintln!("galgod: estado ignorado ({}): versión {} no soportada", path.display(), s.version);
            quarantine(path);
            None
        }
        Err(e) => {
            eprintln!("galgod: estado ignorado ({}): {e}", path.display());
            quarantine(path);
            None
        }
    }
}

/// Aparta un archivo que no sirve, sin borrarlo: queda `<nombre>.corrupt-<segundos>` para revisarlo.
fn quarantine(path: &Path) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let to = path.with_file_name(format!("{name}.corrupt-{}", unix_now()));
    match std::fs::rename(path, &to) {
        Ok(()) => eprintln!("galgod: el estado anterior queda en {}", to.display()),
        Err(e) => eprintln!("galgod: no pude apartar el estado corrupto: {e}"),
    }
}

/// Escritura atómica: temporal con nombre aleatorio, creado con `create_new` (no sigue enlaces ni
/// reutiliza un archivo ajeno), permisos 0600, `fsync` y `rename`. El directorio del estado es del usuario.
pub fn write(path: &Path, saved: &Saved) -> Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let bytes = serde_json::to_vec(saved)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "state.json".into());
    let tmp = path.with_file_name(format!(".{name}.tmp.{}.{:016x}", std::process::id(), random_u64()));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("no pude crear {}", tmp.display()))?;
        if let Err(e) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    std::fs::rename(&tmp, path).with_context(|| format!("no pude reemplazar {}", path.display()))?;
    Ok(())
}

/// Crea el directorio del estado con 0700, o comprueba que uno existente no sea escribible por otros.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o022 != 0 {
        bail!("el directorio del estado {} es escribible por otros; no guardo nada ahí", dir.display());
    }
    Ok(())
}

/// Epoch de esta ejecución. No sale del archivo de estado (si falta o está corrupto, volvería a 1 y los
/// ids viejos chocarían con los nuevos): sale de la hora de arranque, y nunca es menor que el anterior + 1.
/// Cabe en 32 bits, porque va en la parte alta del id de mensaje.
///
/// Probabilidad de colisión: el epoch tiene 20 bits y, si sale de la hora, da la vuelta cada ~12,1 días. Dos
/// mensajes solo pueden chocar si el contador también coincide: el contador sigue desde el estado guardado,
/// así que solo se repite si el estado se pierde y el epoch vuelve a darse mientras un mensaje de aquel
/// epoch sigue sin confirmar (~1 entre 10⁶ por reinicio sin estado, según la hora).
pub fn epoch_for(saved: Option<&Saved>) -> u64 {
    let now = unix_now() % EPOCH_MAX + 1;
    match saved {
        // Distinto del anterior: si la hora no avanzó lo suficiente, se toma el siguiente (con vuelta).
        Some(s) if s.epoch >= 1 && s.epoch <= EPOCH_MAX && now <= s.epoch => s.epoch % EPOCH_MAX + 1,
        _ => now,
    }
}

/// Lo que se guarda del estado actual: solo los mensajes sin confirmar. Los fantasmas sin mensajes
/// pendientes no se guardan (no tienen nada que conservar), y los resúmenes se acotan.
pub fn snapshot(st: &State) -> Saved {
    let now_unix = unix_now();
    Saved {
        version: VERSION,
        epoch: st.epoch,
        next_pane: st.next_pane,
        next_msg: st.next_msg,
        tasks: st
            .tasks
            .iter()
            .map(|t| SavedTask {
                id: t.id,
                from: t.from,
                to: t.to,
                from_name: t.from_name.clone(),
                to_name: t.to_name.clone(),
                workspace: t.workspace.clone(),
                text: t.text.chars().take(MAX_SUMMARY).collect(),
                context: t.context.as_ref().map(|c| c.chars().take(MAX_SUMMARY).collect()),
                status: t.status,
                summary: t.summary.as_ref().map(|s| s.chars().take(MAX_SUMMARY).collect()),
                files: t.files.clone(),
                depth: t.depth,
                created_unix: t.created_unix,
            })
            .collect(),
        next_task: st.next_task,
        panes: st
            .panes
            .values()
            .filter(|p| !p.ghost || p.inbox.iter().any(|m| !m.acked))
            .map(|p| SavedPane {
                id: p.id,
                name: p.name.clone(),
                cmd: p.cmd.clone(),
                manager: p.manager,
                workspace: p.workspace.clone(),
                cwd: p.cwd_vivo(),
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
                        summary: m.summary.chars().take(MAX_SUMMARY).collect(),
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
    st.next_task = saved.next_task;
    st.tasks = saved
        .tasks
        .into_iter()
        .map(|t| super::Task {
            id: t.id,
            from: t.from,
            to: t.to,
            from_name: t.from_name,
            to_name: t.to_name,
            workspace: t.workspace,
            text: t.text,
            context: t.context,
            status: t.status,
            summary: t.summary,
            files: t.files,
            depth: t.depth,
            created_unix: t.created_unix,
        })
        .collect();
    let now_unix = unix_now();
    // Un panel vivo se pierde con el daemon. Vuelve como fantasma si tiene trabajo (mensajes sin confirmar o tareas
    // abiertas) o si es el manager de uno que vuelve (sin él, ese fantasma no tendría a quién avisar ni su nombre
    // de manager). Un shell suelto no reaparece como pestaña muerta.
    let mut necesario: std::collections::HashSet<u32> = saved
        .panes
        .iter()
        .filter(|sp| !sp.inbox.is_empty() || st.tasks.iter().any(|t| t.status.abierta() && (t.from == sp.id || t.to == sp.id)))
        .map(|sp| sp.id)
        .collect();
    loop {
        let antes = necesario.len();
        for sp in &saved.panes {
            if let Some(m) = sp.manager.filter(|_| necesario.contains(&sp.id)) {
                necesario.insert(m);
            }
        }
        if necesario.len() == antes {
            break;
        }
    }
    let mut lost = Vec::new();
    for sp in saved.panes {
        if sp.alive {
            lost.push((sp.id, sp.name.clone(), sp.manager));
            if !necesario.contains(&sp.id) {
                continue;
            }
        }
        let mut inbox = Vec::new();
        for m in sp.inbox {
            // Un id de un formato anterior (≥ 2^53) no cabe en JavaScript: el mensaje recibe un id nuevo.
            let id = if m.id < ID_LIMIT { m.id } else { st.next_msg_id() };
            inbox.push(InboxMsg {
                id,
                from: m.from,
                from_name: m.from_name,
                kind: static_kind(&m.kind),
                summary: m.summary,
                created: instant_for(now_unix, m.created_unix),
                acked: false,
                claimed: false,
                hold_until: None,
                last_inject: None,
                inherited: false,
            });
        }
        let pane = Pane::ghost(sp.id, sp.name, sp.cmd, sp.manager, inbox, sp.workspace, sp.cwd);
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
    // M3: las tareas abiertas no tienen destino vivo tras el reinicio: fallan (y el remitente lo sabe) y se podan las viejas.
    st.fallar_tareas_sin_destino();
    st.podar_tareas(now_unix);
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

/// Número aleatorio para nombres temporales: el reloj en nanosegundos mezclado con el pid.
fn random_u64() -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32) ^ 0x9E37_79B9_7F4A_7C15
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
        // Como el socket: el directorio del estado es privado.
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        dir.join("state.json")
    }

    fn saved_with_inbox() -> Saved {
        Saved {
            version: VERSION,
            epoch: 3,
            next_pane: 5,
            next_msg: 9,
            tasks: vec![],
            next_task: 0,
            panes: vec![
                SavedPane {
                    id: 1,
                    name: "mgr".into(),
                    cmd: vec!["bash".into()],
                    manager: None,
                    workspace: None,
                    cwd: None,
                    alive: true,
                    inbox: vec![SavedMsg { id: (1 << 32) | 1, from: 1, from_name: "mgr".into(), kind: "done".into(), summary: "previo".into(), created_unix: unix_now() }],
                },
                SavedPane {
                    id: 2,
                    name: "w".into(),
                    cmd: vec!["claude".into()],
                    manager: Some(1),
                    workspace: Some("inventia/cotizacion".into()),
                    cwd: None,
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

    /// M2 y B3: un archivo corrupto se aparta (no se sobrescribe), y un no-regular no se lee.
    #[test]
    fn corrupto_se_aparta_y_un_fifo_no_se_lee() {
        let path = tmp("aparta");
        std::fs::write(&path, "{corrupto").unwrap();
        assert!(load(&path).is_none());
        assert!(!path.exists(), "el original queda apartado");
        let dir = path.parent().unwrap();
        let apartados = std::fs::read_dir(dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-")).count();
        assert_eq!(apartados, 1);

        let fifo = dir.join("fifo.json");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Un FIFO sin escritor bloquearía la lectura: no debe leerse nunca.
        assert!(load(&fifo).is_none());
    }

    /// M2: el epoch no depende del archivo: sin archivo o con uno corrupto, nunca vuelve a 1.
    #[test]
    fn epoch_no_vuelve_a_uno_sin_archivo() {
        let sin = epoch_for(None);
        assert!((1..=EPOCH_MAX).contains(&sin), "sin estado, el epoch sale de la hora y cabe en 20 bits");
        let viejo = saved_with_inbox();
        assert!(epoch_for(Some(&viejo)) >= 4, "siempre sube respecto del anterior");

    }

    #[test]
    fn escritura_atomica_con_permisos_privados() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("permisos");
        write(&path, &saved_with_inbox()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(load(&path).unwrap().epoch, 3);
        // No queda ningún temporal suelto.
        let sueltos = std::fs::read_dir(path.parent().unwrap()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".tmp.")).count();
        assert_eq!(sueltos, 0);
    }

    /// B1: un temporal que alguien dejó en el camino no se sigue ni se reutiliza.
    #[test]
    fn no_sigue_un_enlace_en_el_camino_del_temporal() {
        let path = tmp("enlace");
        let victima = path.with_file_name("victima.txt");
        std::fs::write(&victima, "contenido ajeno").unwrap();
        // Un atacante no puede adivinar el nombre aleatorio; aun así, el destino del enlace queda intacto.
        std::os::unix::fs::symlink(&victima, path.with_file_name(".state.json.tmp.0.0000000000000000")).unwrap();
        write(&path, &saved_with_inbox()).unwrap();
        assert_eq!(std::fs::read_to_string(&victima).unwrap(), "contenido ajeno");
    }

    /// B1: un directorio del estado escribible por otros se rechaza.
    #[test]
    fn rechaza_directorio_escribible_por_otros() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("abierto");
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(write(&path, &saved_with_inbox()).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// M1: un fantasma sin mensajes no se guarda; uno con mensajes sí.
    #[test]
    fn fantasmas_vacios_no_se_guardan() {
        let mut st = State { epoch: 2, ..State::default() };
        restore(&mut st, saved_with_inbox());
        // El fantasma "w" tiene un mensaje y el "mgr" tiene el aviso: ambos se guardan.
        assert_eq!(snapshot(&st).panes.len(), 2);
        // Sin mensajes pendientes, el fantasma desaparece del siguiente guardado.
        for p in st.panes.values_mut() {
            for m in p.inbox.iter_mut() {
                m.acked = true;
            }
        }
        assert!(snapshot(&st).panes.is_empty());
    }

    /// Un panel vivo vuelve como fantasma, su manager recibe el aviso y el mensaje sigue en el inbox.
    #[test]
    fn restaura_inbox_y_avisa_al_manager() {
        let mut st = State { epoch: 4, ..State::default() };
        restore(&mut st, saved_with_inbox());
        let mgr = &st.panes[&1];
        assert!(mgr.ghost && mgr._master.is_none());
        // Su mensaje previo, más el aviso de que w ya no existe.
        let inbox = &st.panes[&1].inbox;
        assert_eq!(inbox.len(), 2, "el mensaje previo y el aviso");
        assert!(inbox.iter().any(|m| m.summary.contains("el panel w ya no existe")));
        assert_eq!(st.panes[&2].inbox[0].summary, "hecho");
        assert_eq!(st.panes[&2].inbox[0].id, (2 << 32) | 4);
        assert_eq!(st.next_pane, 5);
        // El aviso de arranque consume un id: el contador sigue donde estaba.
        assert_eq!(st.next_msg, 10);
    }

    /// Un resumen enorme se guarda acotado.
    #[test]
    fn resumen_acotado_al_guardar() {
        let mut st = State { epoch: 2, ..State::default() };
        restore(&mut st, saved_with_inbox());
        st.panes.get_mut(&2).unwrap().inbox[0].summary = "x".repeat(MAX_SUMMARY * 3);
        let guardado = snapshot(&st);
        let resumen = &guardado.panes.iter().find(|p| p.id == 2).unwrap().inbox[0].summary;
        assert_eq!(resumen.chars().count(), MAX_SUMMARY);
    }

    /// Un epoch sale siempre en 1..=EPOCH_MAX, aunque falte el estado o esté corrupto (no es una constante).
    #[test]
    fn epoch_en_rango_y_distinto_del_anterior() {
        let e = epoch_for(None);
        assert!(e >= 1 && e <= EPOCH_MAX);
        // Un estado anterior con el mismo epoch que la hora de ahora: el nuevo es otro.
        let now = unix_now() % EPOCH_MAX + 1;
        let mut viejo = saved_with_inbox();
        viejo.epoch = now;
        assert_ne!(epoch_for(Some(&viejo)), now);
        // Un estado de otro formato (epoch enorme) no se toma como base.
        viejo.epoch = 7_000_000_000;
        let e2 = epoch_for(Some(&viejo));
        assert!(e2 >= 1 && e2 <= EPOCH_MAX);
    }

    /// Un mensaje de un formato anterior (id ≥ 2^53) recibe un id nuevo al cargar; los demás se conservan.
    #[test]
    fn renumera_ids_grandes_al_cargar() {
        let mut saved = saved_with_inbox();
        saved.panes[1].inbox[0].id = 7_710_670_798_423_326_721; // formato anterior: > 2^53
        let mut st = State { epoch: 5, ..State::default() };
        restore(&mut st, saved);
        let ids: Vec<u64> = st.panes[&2].inbox.iter().map(|m| m.id).collect();
        assert!(ids.iter().all(|&i| i < ID_LIMIT), "{ids:?}");
        assert_eq!(st.panes[&2].inbox[0].summary, "hecho");
    }

    /// M5: un estado de la versión anterior se carga, sus pendientes llegan al inbox y el archivo se retira
    /// solo después de escribir el nuevo.
    #[test]
    fn migra_el_estado_de_la_version_anterior_sin_perder_pendientes() {
        let dir = tmp("migracion");
        let legacy = dir.with_file_name("state.json");
        let mut saved = saved_with_inbox();
        saved.panes[1].inbox[0].summary = "pendiente-antiguo".into();
        write(&legacy, &saved).unwrap();
        let socket = dir.with_file_name("galgo.sock");
        let state_path = socket.with_extension("state.json");
        assert!(!state_path.exists());
        let (loaded, from) = load_with_migration(&state_path, legacy_path(&socket).as_deref());
        assert_eq!(from.as_deref(), Some(legacy.as_path()));
        let mut st = State { epoch: 9, ..State::default() };
        restore(&mut st, loaded.expect("el estado anterior se carga"));
        assert!(st.panes.values().any(|p| p.inbox.iter().any(|m| m.summary == "pendiente-antiguo")));
        // Sin el estado nuevo escrito, no se retira nada.
        assert!(finish_migration(&legacy, &state_path).unwrap().is_none());
        assert!(legacy.exists());
        write(&state_path, &snapshot(&st)).unwrap();
        let moved = finish_migration(&legacy, &state_path).unwrap().expect("se retira");
        assert!(!legacy.exists() && moved.exists());
        // Ya con el estado nuevo, la carga usa ese y no el anterior.
        let (again, from) = load_with_migration(&state_path, legacy_path(&socket).as_deref());
        assert!(again.is_some() && from.is_none());
    }
}
