//! galgod: daemon que posee los PTYs, el estado de pantalla y la entrega de mensajes entre agentes.

mod persist;
mod usage;

use anyhow::{anyhow, bail, Context, Result};
use galgo_proto::{HookEvent, Message, PaneInfo, Request, Response, Status};
use galgo_usage::PaneUsage;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Notify,
};

/// Cuántas escrituras pueden esperar en la cola de un panel antes de rechazar la siguiente.
const INPUT_QUEUE: usize = 8;
/// errno ESRCH: el proceso no existe (igual en Linux y macOS).
const ESRCH: i32 = 3;
/// Tamaño máximo de un panel. Sin tope, un cliente podría pedir una pantalla enorme que agota la memoria
/// del daemon (y de todos los paneles con él).
const MAX_ROWS: u16 = 500;
const MAX_COLS: u16 = 1000;

/// Valida filas y columnas de un panel (`Spawn` y `Resize`).
fn check_size(rows: u16, cols: u16) -> Result<()> {
    if rows == 0 || cols == 0 {
        bail!("rows y cols deben ser mayores que 0");
    }
    if rows > MAX_ROWS || cols > MAX_COLS {
        bail!("tamaño demasiado grande: como máximo {MAX_ROWS} filas y {MAX_COLS} columnas");
    }
    Ok(())
}

struct InboxMsg {
    id: u64,
    from: u32,
    from_name: String,
    kind: &'static str,
    summary: String,
    created: Instant,
    acked: bool,
    /// Entregado a una espera en curso; se confirma o se libera según la escritura.
    claimed: bool,
    /// Con `client_ack`: la reserva vence si el cliente no confirma a tiempo.
    hold_until: Option<Instant>,
    last_inject: Option<Instant>,
    /// Llegó con un fantasma reutilizado del mismo manager. Se lee con `galgo inbox`, nunca se inyecta.
    inherited: bool,
}

impl InboxMsg {
    fn is_held(&self) -> bool {
        self.claimed && self.hold_until.map_or(true, |t| t > Instant::now())
    }
}

struct Pane {
    id: u32,
    name: String,
    cmd: Vec<String>,
    status: Status,
    manager: Option<u32>,
    inject: bool,
    screen: Arc<Mutex<vt100::Parser>>,
    /// Generación de la pantalla. Se incrementa bajo el candado de `screen`, en cada escritura del
    /// lector y en cada `Resize`, así que un snapshot que la lea siempre corresponde a esa generación.
    gen: Arc<AtomicU64>,
    /// Cola de escritura al PTY. La atiende un hilo propio: nunca se escribe en un hilo de tokio.
    input: SyncSender<Vec<u8>>,
    /// Para matar el proceso sin tener el hijo (el hilo de salida lo espera).
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// PID del proceso; es también el id del grupo de procesos (el PTY lo crea con setsid).
    pid: Option<u32>,
    /// El proceso terminó y se recolectó: ya no se le señala nada.
    exited: bool,
    /// Hubo entrada enviada al panel desde su último resultado entregado.
    touched_since_report: bool,
    _master: Option<Box<dyn MasterPty + Send>>,
    /// Panel restaurado tras un reinicio del daemon: existe en la lista y en el inbox, pero su proceso murió.
    ghost: bool,
    /// Workspace del panel ("<proyecto>/<rama>" o un nombre libre) y su directorio de trabajo.
    workspace: Option<String>,
    cwd: Option<String>,
    /// Uso del transcript (lo rellena el hilo de sondeo) y último informe del agente. Solo en memoria.
    usage: Option<PaneUsage>,
    usage_report: Option<usage::ReportData>,
    inbox: Vec<InboxMsg>,
    last_summary: Option<String>,
    /// Último motivo de bloqueo avisado al manager; un motivo nuevo se avisa aunque ya estuviera blocked.
    last_blocked: Option<String>,
    /// Procesos que seguían en la sesión del panel cuando el líder salió (el líder aún sin recolectar,
    /// así que su pid no pudo reutilizarse). Solo estos se barren después: nunca un pid nuevo.
    orphans: Vec<i32>,
    waiting_on: Option<(u32, Instant)>,
    /// Ya entregó su resultado en este turno (`galgo done` o el evento de fin de turno).
    /// Solo se limpia con un prompt nuevo o con un envío con Enter.
    reported: bool,
}

/// Killer de un panel sin proceso: nada que matar.
#[derive(Debug)]
struct GhostKiller;

impl ChildKiller for GhostKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(GhostKiller)
    }
}

impl Pane {
    /// Panel restaurado: sin PTY. Las órdenes que lo necesitan fallan con un error claro.
    fn ghost(
        id: u32,
        name: String,
        cmd: Vec<String>,
        manager: Option<u32>,
        inbox: Vec<InboxMsg>,
        workspace: Option<String>,
        cwd: Option<String>,
    ) -> Pane {
        // Entrada cerrada desde el principio: escribir en un panel sin proceso da "no acepta entrada".
        let (input, rx) = sync_channel::<Vec<u8>>(1);
        drop(rx);
        Pane {
            id,
            name,
            cmd,
            status: Status::Exited,
            manager,
            inject: false,
            screen: Arc::new(Mutex::new(vt100::Parser::new(std::num::NonZeroU16::new(24).unwrap(), std::num::NonZeroU16::new(80).unwrap(), 0))),
            gen: Arc::new(AtomicU64::new(fresh_gen())),
            input,
            killer: Box::new(GhostKiller),
            pid: None,
            exited: true,
            touched_since_report: false,
            _master: None,
            ghost: true,
            workspace,
            cwd,
            usage: None,
            usage_report: None,
            inbox,
            last_summary: None,
            last_blocked: None,
            orphans: Vec::new(),
            waiting_on: None,
            reported: false,
        }
    }
}

fn no_proceso(name: &str) -> anyhow::Error {
    anyhow!("el panel {name} no tiene proceso: se restauró tras reiniciar el daemon")
}

#[derive(Default)]
struct State {
    /// Ejecución actual del daemon. Los ids de mensaje llevan el epoch en la parte alta.
    epoch: u64,
    next_pane: u32,
    next_msg: u64,
    panes: BTreeMap<u32, Pane>,
    /// Nombres que un spawn en curso ya reservó y aún no tienen panel.
    reserved: HashSet<String>,
    /// Se incrementa con cada cambio persistente (inbox, paneles). El guardado solo escribe si cambió.
    version: u64,
}

impl State {
    /// Marca un cambio persistente.
    fn touch(&mut self) {
        self.version += 1;
    }

    /// Id nuevo de mensaje: el epoch en la parte alta, así un ack de otra ejecución no lo confunde.
    fn next_msg_id(&mut self) -> u64 {
        self.next_msg += 1;
        (self.epoch << 32) | (self.next_msg & 0xFFFF_FFFF)
    }

    fn resolve(&self, r: &str) -> Result<u32> {
        if let Ok(id) = r.parse::<u32>() {
            if self.panes.contains_key(&id) {
                return Ok(id);
            }
        }
        self.panes
            .values()
            .find(|p| p.name == r)
            .map(|p| p.id)
            .ok_or_else(|| anyhow!("panel no encontrado: {r}"))
    }

    /// Si `name` es el de un fantasma: `Some((su id, heredar))`. Hereda su inbox solo si el manager es el mismo
    /// (mismo rol). Con otro manager, sus pendientes van a su antiguo manager (B3); si ese no existe, se rechaza
    /// sin tocar nada: nunca se descarta un resultado.
    fn ghost_plan(&self, name: &str, manager: Option<u32>) -> Result<Option<(u32, bool)>> {
        let Some(g) = self.panes.values().find(|g| g.ghost && g.name == name) else { return Ok(None) };
        if manager == Some(g.id) {
            bail!("un panel no puede ser su propio manager");
        }
        let inherit = g.manager == manager;
        let pending = g.inbox.iter().filter(|m| !m.acked).count();
        let manager_alive = g.manager.is_some_and(|x| self.panes.contains_key(&x));
        if !inherit && pending > 0 && !manager_alive {
            bail!(
                "el fantasma {name} tiene {pending} mensaje(s) sin confirmar y no tiene manager vivo al que pasarlos; confírmalos con galgo ack, quítalo con galgo kill {name} o reusalo con su mismo manager"
            );
        }
        Ok(Some((g.id, inherit)))
    }

    /// Quita el fantasma `gid` y lo sustituye por `new_id`: hereda su inbox (marcado) o entrega sus pendientes a
    /// su antiguo manager. Todo lo que apuntaba a `gid` pasa a apuntar a `new_id`. Devuelve cuántos heredó.
    fn absorb_ghost(&mut self, gid: u32, new_id: u32, inherit: bool, pane: &mut Pane) -> usize {
        let Some(g) = self.panes.remove(&gid) else { return 0 };
        let mut inherited = 0;
        if inherit {
            let mut inbox = g.inbox;
            for m in inbox.iter_mut().filter(|m| !m.acked) {
                m.inherited = true;
                inherited += 1;
            }
            pane.inbox = inbox;
        } else if let Some(t) = g.manager.filter(|t| self.panes.contains_key(t)) {
            if let Some(target) = self.panes.get_mut(&t) {
                target.inbox.extend(g.inbox.into_iter().filter(|m| !m.acked));
            }
        }
        for p in self.panes.values_mut() {
            if p.manager == Some(gid) {
                p.manager = Some(new_id);
            }
            if let Some((t, _)) = p.waiting_on.as_mut() {
                if *t == gid {
                    *t = new_id;
                }
            }
        }
        inherited
    }

    fn push(&mut self, to: u32, from: u32, kind: &'static str, summary: String) {
        let id = self.next_msg_id();
        self.touch();
        let from_name = self.panes.get(&from).map(|p| p.name.clone()).unwrap_or_default();
        if let Some(p) = self.panes.get_mut(&to) {
            p.inbox.push(InboxMsg {
                id,
                from,
                from_name,
                kind,
                summary,
                created: Instant::now(),
                acked: false,
                claimed: false,
                hold_until: None,
                last_inject: None,
                inherited: false,
            });
        }
    }

    fn info(&self, p: &Pane, orphan_secs: u64) -> PaneInfo {
        let oldest = p
            .inbox
            .iter()
            .filter(|m| !m.acked)
            .map(|m| m.created.elapsed().as_secs())
            .max();
        PaneInfo {
            id: p.id,
            name: p.name.clone(),
            status: p.status,
            manager: p.manager.and_then(|m| self.panes.get(&m)).map(|m| m.name.clone()),
            cmd: p.cmd.clone(),
            unacked: p.inbox.iter().filter(|m| !m.acked).count(),
            oldest_unacked_secs: oldest,
            orphan: oldest.is_some_and(|s| s >= orphan_secs),
            waiting_on: p.waiting_on.and_then(|(t, _)| self.panes.get(&t)).map(|t| t.name.clone()),
            waiting_secs: p.waiting_on.map(|(_, at)| at.elapsed().as_secs()),
            workspace: p.workspace.clone(),
            cwd: p.cwd.clone(),
            usage: usage::merged(p.usage.as_ref(), p.usage_report.as_ref()),
        }
    }
}

struct Daemon {
    state: Mutex<State>,
    /// Dónde se guarda el estado entre reinicios, y un candado para escribirlo en orden.
    state_path: PathBuf,
    persist_lock: Mutex<()>,
    /// Avisa al hilo de guardado de que hubo cambios. Enviar no bloquea.
    persist_tx: std::sync::mpsc::Sender<()>,
    /// Uso vivo por panel y cuotas: en memoria, fuera de `state` y de `state.json`.
    usage: usage::UsageLive,
    notify: Notify,
    socket: PathBuf,
    orphan_secs: u64,
    ack_secs: u64,
    retry: Duration,
    /// Tiempo máximo para escribir una respuesta; un cliente que no lee no bloquea la entrega.
    write_timeout: Duration,
}

/// Libera un nombre reservado si el spawn falla antes de crear el panel.
struct NameReservation {
    d: Arc<Daemon>,
    name: Option<String>,
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        if let Some(n) = self.name.take() {
            self.d.state.lock().unwrap().reserved.remove(&n);
        }
    }
}

/// Al cancelarse una espera (el cliente se fue), deja de figurar como esperando.
struct WaitingGuard {
    d: Arc<Daemon>,
    waiter: u32,
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        if let Some(p) = self.d.state.lock().unwrap().panes.get_mut(&self.waiter) {
            p.waiting_on = None;
        }
    }
}

fn verb(kind: &str) -> &'static str {
    match kind {
        "done" => "terminó",
        "failed" => "falló",
        "blocked" => "necesita una decisión",
        _ => "salió sin reportar resultado",
    }
}

/// Un mensaje se escribe en el terminal solo si es nuevo para este panel: no confirmado, sin reserva y no
/// heredado de una ejecución de un fantasma (B3).
fn is_injectable(m: &InboxMsg) -> bool {
    !m.acked && !m.is_held() && !m.inherited
}

/// Límite de longitud para nombres y workspaces, y texto sin caracteres de control (B2).
const MAX_LABEL: usize = 128;

fn check_text(what: &str, text: &str, max: usize) -> Result<()> {
    if text.chars().any(char::is_control) {
        bail!("{what} no puede tener caracteres de control");
    }
    if text.chars().count() > max {
        bail!("{what} tiene más de {max} caracteres");
    }
    Ok(())
}

/// Quita los caracteres de control y acota la longitud: para lo que deduce git, que no se valida.
fn clean_label(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(MAX_LABEL).collect()
}

/// Texto seguro para escribir en la terminal de otro panel: sin caracteres de control.
/// Un `\r` ejecutaría una línea y un Ctrl-C (`\x03`) interrumpiría al manager.
fn sanitize(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn input_error(e: TrySendError<Vec<u8>>) -> anyhow::Error {
    match e {
        TrySendError::Full(_) => anyhow!("el panel no está leyendo su entrada"),
        TrySendError::Disconnected(_) => anyhow!("el panel ya no acepta entrada"),
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Variables que identifican la sesión o el panel que lanzó el daemon. Se quitan del entorno de cada
/// panel: no son configuración del usuario. Lo que no está aquí (proveedor, tokens, límites, esfuerzo…)
/// se conserva. `CLAUDE_EFFORT` se conserva a propósito: es una preferencia, no una identidad.
/// Generación inicial de un panel: de la hora de arranque, no de 0. Así una generación vieja de otro
/// panel o de otro daemon no coincide por casualidad con la de uno nuevo (un `unchanged` falso).
/// Ocupa 40 bits para que el token (`generación << 16 | offset`) quepa en 56.
fn fresh_gen() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    (nanos >> 20) & ((1 << 40) - 1)
}

/// Token que el cliente guarda y devuelve en `since`: la generación y el desplazamiento. Así un
/// `unchanged` solo vale para la misma vista (mismo offset) y no para cualquier otra.
fn token(gen: u64, offset: u16) -> u64 {
    (gen << 16) | offset as u64
}

/// Retardo del guardado: los cambios que llegan seguidos se escriben juntos.
const PERSIST_DELAY: Duration = Duration::from_millis(200);

/// Hilo de guardado: espera un cambio, deja pasar el retardo para agrupar los que vengan, y escribe.
fn persist_loop(d: Arc<Daemon>, rx: std::sync::mpsc::Receiver<()>) {
    let mut last = None;
    while rx.recv().is_ok() {
        std::thread::sleep(PERSIST_DELAY);
        while rx.try_recv().is_ok() {}
        d.write_state(&mut last, false);
    }
}

/// Deduce el workspace de un directorio dentro de un repo git: "<repo>/<rama>". Sin git, o con el
/// HEAD desligado, devuelve `None` o solo el nombre del repo. Nunca falla: git ausente no es un error.
fn workspace_for(cwd: Option<&str>) -> Option<String> {
    let dir = cwd?;
    let top = git_out(dir, &["rev-parse", "--show-toplevel"])?;
    let branch = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let name = clean_label(&workspace_name(&top, &branch));
    (!name.is_empty()).then_some(name)
}

/// Variables de git que cambian a qué repositorio apunta un comando. Se quitan del entorno del daemon: un
/// `GIT_DIR` heredado daría a todos los paneles la rama de otro repo (B1).
const GIT_ENV_REMOVE: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// Tiempo máximo de una consulta a git. Un volumen que no responde no puede retener el spawn (M3).
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

fn git_out(dir: &str, args: &[&str]) -> Option<String> {
    git_program("git", dir, args)
}

/// Ejecuta `program -C dir args` con tiempo límite. Si vence, el proceso se mata y no hay resultado.
fn git_program(program: &str, dir: &str, args: &[&str]) -> Option<String> {
    git_program_with(program, dir, args, &[])
}

/// Como `git_program`, con variables extra que el entorno del proceso hereda antes de limpiar las de git.
fn git_program_with(program: &str, dir: &str, args: &[&str], inherited: &[(&str, &str)]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0");
    for (k, v) in inherited {
        cmd.env(k, v);
    }
    for var in GIT_ENV_REMOVE {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = child.stdout.take()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut out, &mut text).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// "<repo>/<rama>" a partir de la raíz del repo y la rama. El HEAD desligado ("HEAD") deja solo el repo.
fn workspace_name(top: &str, branch: &str) -> String {
    let repo = std::path::Path::new(top).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| top.to_string());
    if branch.is_empty() || branch == "HEAD" { repo } else { format!("{repo}/{branch}") }
}

fn is_session_marker(key: &str) -> bool {
    const EXACT: [&str; 11] = [
        "CLAUDECODE",
        "CLAUDE_PID",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_CHILD_NOTES_DIR",
        "CLAUDE_CODE_SSE_PORT",
    ];
    // Todas las variables de herdr describen el panel de herdr donde nació el daemon.
    EXACT.contains(&key) || key.starts_with("HERDR_")
}

fn to_response(res: Result<Value>) -> Response {
    match res {
        Ok(v) => Response::ok(v),
        Err(e) => Response::err(format!("{e:#}")),
    }
}

impl Daemon {
    #[allow(clippy::too_many_arguments)]
    fn spawn_pane(
        self: &Arc<Self>,
        name: String,
        cmd: Vec<String>,
        cwd: Option<String>,
        manager: Option<String>,
        inject: bool,
        rows: u16,
        cols: u16,
        workspace: Option<String>,
    ) -> Result<(u32, usize)> {
        check_size(rows, cols)?;
        // El workspace llega ya resuelto: el explícito, o el que deduce git fuera de este hilo (`handle`).
        let workspace = workspace.filter(|w| !w.trim().is_empty());
        if let Some(w) = &workspace {
            check_text("el workspace", w, MAX_LABEL)?;
        }
        check_text("el nombre", &name, MAX_LABEL)?;
        let cwd_saved = cwd.clone();
        if name.trim().is_empty() || name.parse::<u32>().is_ok() {
            bail!("nombre inválido: no puede estar vacío ni ser solo un número");
        }
        if cmd.is_empty() {
            bail!("falta el comando a ejecutar");
        }
        if let Some(dir) = &cwd {
            if !Path::new(dir).is_dir() {
                bail!("el directorio {dir} no existe");
            }
        }
        // La comprobación del nombre y su reserva van en la misma sección crítica:
        // dos spawns concurrentes con el mismo nombre no pueden pasar los dos.
        let (id, manager_id, ghost_plan, mut reservation) = {
            let mut st = self.state.lock().unwrap();
            if st.panes.values().any(|p| !p.ghost && p.name == name) || st.reserved.contains(&name) {
                bail!("ya existe un panel llamado {name}");
            }
            let m = match manager {
                Some(r) => Some(st.resolve(&r)?),
                None => None,
            };
            // Antes de tocar nada: si el nombre es de un fantasma con pendientes que no se pueden entregar, se rechaza.
            let ghost_plan = st.ghost_plan(&name, m)?;
            st.next_pane += 1;
            st.reserved.insert(name.clone());
            let reservation = NameReservation { d: Arc::clone(self), name: Some(name.clone()) };
            (st.next_pane, m, ghost_plan, reservation)
        };

        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("no pude abrir el PTY")?;
        let mut builder = CommandBuilder::new(&cmd[0]);
        builder.args(&cmd[1..]);
        if let Some(dir) = cwd {
            builder.cwd(dir);
        }
        // El daemon puede haberse arrancado desde una sesión de Claude Code o de herdr, que dejan
        // marcadores de identidad de sesión. Un agente en el panel los vería y se creería otra sesión.
        for (key, _) in std::env::vars_os() {
            if is_session_marker(&key.to_string_lossy()) {
                builder.env_remove(&key);
            }
        }
        builder.env("TERM", "xterm-256color");
        builder.env("GALGO_PANE", id.to_string());
        builder.env("GALGO_PANE_NAME", &name);
        builder.env("GALGO_SOCKET", self.socket.as_os_str());
        let mut child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("no pude ejecutar {}", cmd[0]))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (v_rows, v_cols) = (
            std::num::NonZeroU16::new(rows).context("rows debe ser mayor que 0")?,
            std::num::NonZeroU16::new(cols).context("cols debe ser mayor que 0")?,
        );
        let screen = Arc::new(Mutex::new(vt100::Parser::new(v_rows, v_cols, 2000)));
        let gen = Arc::new(AtomicU64::new(fresh_gen()));

        let (input, input_rx) = sync_channel::<Vec<u8>>(INPUT_QUEUE);
        std::thread::spawn(move || {
            for chunk in input_rx {
                if writer.write_all(&chunk).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        });

        let pane = Pane {
            id,
            name: name.clone(),
            cmd,
            status: Status::Idle,
            manager: manager_id,
            inject,
            screen: Arc::clone(&screen),
            gen: Arc::clone(&gen),
            input,
            killer,
            pid,
            exited: false,
            touched_since_report: false,
            _master: Some(pair.master),
            ghost: false,
            workspace,
            cwd: cwd_saved,
            usage: None,
            usage_report: None,
            inbox: Vec::new(),
            last_summary: None,
            last_blocked: None,
            orphans: Vec::new(),
            waiting_on: None,
            reported: false,
        };
        let mut inherited = 0usize;
        {
            let mut st = self.state.lock().unwrap();
            st.reserved.remove(&name);
            let mut pane = pane;
            if let Some((gid, inherit)) = ghost_plan {
                inherited = st.absorb_ghost(gid, id, inherit, &mut pane);
                self.untrack_usage(gid);
            }
            st.panes.insert(id, pane);
            st.touch();
        }
        reservation.name = None;

        let d = Arc::clone(self);
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut parser = screen.lock().unwrap();
                        parser.process(&buf[..n]);
                        gen.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            // El hijo se recolecta aquí, sin candados: no queda <defunct> y sabemos cómo terminó.
            // Antes de recolectar al hijo, el panel queda marcado como terminado: un kill que llegue
            // ahora ya no le envía señales, así que nunca actúa sobre un pid recolectado.
            d.mark_closed(id);
            let code = child.wait().ok().map(|s| s.exit_code());
            d.on_exit(id, code);
        });
        Ok((id, inherited))
    }

    fn mark_closed(&self, id: u32) {
        let sid = {
            let mut st = self.state.lock().unwrap();
            let Some(p) = st.panes.get_mut(&id) else { return };
            p.exited = true;
            let pid = p.pid;
            st.touch();
            pid
        };
        // El líder todavía no se recolectó: su pid sigue siendo suyo y la lista de la sesión es fiable.
        if let Some(sid) = sid {
            let me = unsafe { libc::getpid() };
            let found: Vec<i32> = session_pids(sid as i32).into_iter().filter(|&p| p != me && p != sid as i32).collect();
            if let Some(p) = self.state.lock().unwrap().panes.get_mut(&id) {
                p.orphans = found;
            }
        }
    }

    /// El proceso del panel terminó. Siempre queda `exited` (conservando el último resumen).
    /// Avisa al manager si no hubo resultado en este turno o si hubo actividad después.
    fn on_exit(&self, id: u32, code: Option<u32>) {
        let manager = {
            let mut st = self.state.lock().unwrap();
            st.touch();
            let Some(p) = st.panes.get_mut(&id) else { return };
            let notify = !p.reported || p.touched_since_report;
            p.exited = true;
            p.status = Status::Exited;
            let manager = p.manager;
            if notify {
                if let Some(m) = manager {
                    let why = match code {
                        Some(c) => format!("salió con código {c}"),
                        None => "salió sin código de salida".into(),
                    };
                    st.push(m, id, "exited", why);
                }
            }
            manager
        };
        self.notify.notify_waiters();
        if let Some(m) = manager {
            self.try_inject(m);
        }
        self.mark_dirty();
    }

    /// Encola en el panel los mensajes pendientes si pidió `inject` y está `idle`.
    /// No escribe: la escritura la hace el hilo del panel. Nunca bloquea.
    fn try_inject(&self, pane: u32) {
        let mut st = self.state.lock().unwrap();
        let retry = self.retry;
        let Some(p) = st.panes.get_mut(&pane) else { return };
        if !p.inject || p.status != Status::Idle {
            return;
        }
        let mut lines = Vec::new();
        let mut ids = Vec::new();
        for m in p.inbox.iter_mut().filter(|m| is_injectable(m)) {
            if m.last_inject.is_none_or(|t| t.elapsed() >= retry) {
                m.last_inject = Some(Instant::now());
                lines.push(sanitize(&format!(
                    "[galgo] {} {}: {}",
                    m.from_name,
                    verb(m.kind),
                    m.summary
                )));
                ids.push(m.id);
            }
        }
        if lines.is_empty() {
            return;
        }
        let mut chunk = Vec::new();
        for l in &lines {
            chunk.extend_from_slice(l.as_bytes());
            chunk.push(b'\r');
        }
        if p.input.try_send(chunk).is_err() {
            // No cupo en la cola: se reintenta en el siguiente ciclo, no en `retry` segundos.
            for m in p.inbox.iter_mut().filter(|m| ids.contains(&m.id)) {
                m.last_inject = None;
            }
        }
    }

    /// Busca un mensaje del objetivo sin confirmar, o el estado final del objetivo.
    /// No confirma nada: la confirmación llega con `confirm`, tras escribir la respuesta.
    fn check_wait(&self, waiter: u32, target: u32) -> Option<(Value, Option<u64>)> {
        let mut st = self.state.lock().unwrap();
        if let Some(w) = st.panes.get_mut(&waiter) {
            if let Some(m) = w.inbox.iter_mut().find(|m| !m.acked && !m.is_held() && m.from == target) {
                // Reserva el mensaje: otra espera no lo recibe mientras esta respuesta se escribe.
                m.claimed = true;
                m.hold_until = None;
                let v = json!({
                    "from": m.from_name, "kind": m.kind, "summary": m.summary, "delivered": "wait",
                    "msg_id": m.id, "pane_id": waiter
                });
                return Some((v, Some(m.id)));
            }
        }
        let t = st.panes.get(&target)?;
        if matches!(t.status, Status::Done | Status::Failed | Status::Exited) {
            let v = json!({ "from": t.name, "kind": "status", "status": t.status, "summary": t.last_summary });
            return Some((v, None));
        }
        None
    }

    /// Confirma un mensaje. Se llama solo después de que la respuesta se escribió al cliente.
    fn confirm(&self, pane: u32, msg: u64) {
        self.set_claim(pane, msg, true);
    }

    /// La respuesta se escribió pero el cliente aún no confirma: se retiene un tiempo.
    fn hold(&self, pane: u32, msg: u64, secs: u64) {
        let mut st = self.state.lock().unwrap();
        if let Some(p) = st.panes.get_mut(&pane) {
            for m in p.inbox.iter_mut().filter(|m| m.id == msg && !m.acked) {
                m.hold_until = Some(Instant::now() + Duration::from_secs(secs));
            }
        }
    }

    /// Libera un mensaje reservado cuando la respuesta no llegó al cliente: queda entregable.
    fn release(&self, pane: u32, msg: u64) {
        self.set_claim(pane, msg, false);
    }

    fn set_claim(&self, pane: u32, msg: u64, confirmed: bool) {
        {
            let mut st = self.state.lock().unwrap();
            if let Some(p) = st.panes.get_mut(&pane) {
                for m in p.inbox.iter_mut().filter(|m| m.id == msg) {
                    m.claimed = false;
                    m.hold_until = None;
                    if confirmed {
                        m.acked = true;
                    }
                }
            }
            // M1: una confirmación es un cambio persistente; sin esto no llega al disco hasta el siguiente.
            if confirmed {
                st.touch();
            }
        }
        self.notify.notify_waiters();
    }

    /// Espera a que el objetivo entregue algo o termine. Devuelve la respuesta y, si era un
    /// mensaje, el id que hay que confirmar cuando la respuesta llegue al cliente.
    async fn wait_for(
        self: &Arc<Self>,
        waiter: String,
        target: String,
        timeout_secs: u64,
    ) -> (Result<Value>, Option<(u32, u64)>) {
        let (w, t) = {
            let mut st = self.state.lock().unwrap();
            let (w, t) = match (st.resolve(&waiter), st.resolve(&target)) {
                (Ok(w), Ok(t)) => (w, t),
                (Err(e), _) | (_, Err(e)) => return (Err(e), None),
            };
            st.panes.get_mut(&w).unwrap().waiting_on = Some((t, Instant::now()));
            (w, t)
        };
        // Si el futuro se cancela (el cliente se desconectó), la marca se limpia igualmente.
        let _clear = WaitingGuard { d: Arc::clone(self), waiter: w };

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some((v, msg)) = self.check_wait(w, t) {
                return (Ok(v), msg.map(|m| (w, m)));
            }
            tokio::select! {
                _ = &mut notified => {}
                // Una reserva sin ack vence sola: hay que volver a mirar.
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = tokio::time::sleep_until(deadline) => {
                    return (Err(anyhow!("tiempo agotado esperando al panel")), None);
                }
            }
        }
    }

    /// Atiende una solicitud. `wait` devuelve además el mensaje a confirmar tras responder.
    async fn dispatch(self: &Arc<Self>, req: Request) -> (Result<Value>, Option<(u32, u64)>) {
        // Solo lo que puede cambiar el inbox o la lista de paneles avisa al guardado. Un `Send` no cambia
        // nada persistente (el texto no se guarda), y las lecturas tampoco: no se escribe nada por ellos.
        let mutating = matches!(
            req,
            Request::Spawn { .. } | Request::Done { .. } | Request::Hook { .. }
                | Request::Ack { .. } | Request::Kill { .. } | Request::Wait { .. }
        );
        let out = match req {
            Request::Wait { waiter, target, timeout_secs, .. } => {
                self.wait_for(waiter, target, timeout_secs).await
            }
            other => (self.handle(other).await, None),
        };
        if mutating {
            self.mark_dirty();
        }
        out
    }

    /// Marca el estado como cambiado. El hilo de guardado escribe poco después, agrupando los cambios
    /// seguidos. No bloquea: no hay escritura en el hilo de la petición.
    fn mark_dirty(&self) {
        let _ = self.persist_tx.send(());
    }

    /// Escribe ahora, sin esperar al hilo (arranque y apagado).
    fn persist_now(&self) {
        let mut last = None;
        self.write_state(&mut last, true);
    }

    /// Escribe el estado si cambió desde la última escritura (`last` recuerda la versión escrita).
    /// Un fallo se avisa en el log: el daemon sigue funcionando.
    fn write_state(&self, last: &mut Option<u64>, force: bool) {
        let _order = self.persist_lock.lock().unwrap();
        let (version, saved) = {
            let st = self.state.lock().unwrap();
            if !force && *last == Some(st.version) {
                return;
            }
            (st.version, persist::snapshot(&st))
        };
        match persist::write(&self.state_path, &saved) {
            Ok(()) => *last = Some(version),
            Err(e) => eprintln!("galgod: no pude guardar el estado: {e:#}"),
        }
    }

    async fn handle(self: &Arc<Self>, req: Request) -> Result<Value> {
        match req {
            Request::Spawn { name, cmd, cwd, manager, inject, rows, cols, workspace } => {
                // git puede tardar (red, disco lento): la consulta va fuera del hilo de peticiones y con tiempo límite.
                let workspace = match workspace.filter(|w| !w.trim().is_empty()) {
                    Some(w) => Some(w),
                    None => {
                        let dir = cwd.clone();
                        tokio::task::spawn_blocking(move || workspace_for(dir.as_deref())).await?
                    }
                };
                let (id, inherited) = self.spawn_pane(name, cmd, cwd, manager, inject, rows, cols, workspace)?;
                let st = self.state.lock().unwrap();
                let mut v = serde_json::to_value(st.info(&st.panes[&id], self.orphan_secs))?;
                v["inherited"] = json!(inherited);
                Ok(v)
            }
            Request::List => {
                let st = self.state.lock().unwrap();
                let v: Vec<PaneInfo> =
                    st.panes.values().map(|p| st.info(p, self.orphan_secs)).collect();
                Ok(serde_json::to_value(v)?)
            }
            Request::Send { pane, text, enter } => {
                let mut st = self.state.lock().unwrap();
                let id = st.resolve(&pane)?;
                let p = st.panes.get_mut(&id).unwrap();
                let mut chunk = text.into_bytes();
                if enter {
                    chunk.push(b'\r');
                }
                if p.ghost {
                    return Err(no_proceso(&p.name));
                }
                p.input.try_send(chunk).map_err(input_error)?;
                p.touched_since_report = true;
                if enter {
                    // Una tarea nueva: el turno vuelve a estar abierto y el estado final se reinicia.
                    p.reported = false;
                    if matches!(p.status, Status::Done | Status::Failed) {
                        p.status = Status::Working;
                    }
                }
                Ok(json!({}))
            }
            Request::Read { pane } => {
                let screen = {
                    let st = self.state.lock().unwrap();
                    let p = &st.panes[&st.resolve(&pane)?];
                    if p.ghost {
                        return Err(no_proceso(&p.name));
                    }
                    Arc::clone(&p.screen)
                };
                let text = screen.lock().unwrap().screen().contents();
                Ok(json!({ "text": text.trim_end() }))
            }
            Request::Snapshot { pane, offset, since } => {
                let (screen, gen) = {
                    let st = self.state.lock().unwrap();
                    let p = &st.panes[&st.resolve(&pane)?];
                    if p.ghost {
                        return Err(no_proceso(&p.name));
                    }
                    (Arc::clone(&p.screen), Arc::clone(&p.gen))
                };
                let mut parser = screen.lock().unwrap();
                let current = token(gen.load(Ordering::SeqCst), offset);
                // Sin cambios desde la generación que pidió el cliente: no se serializa nada.
                if since == Some(current) {
                    return Ok(json!({ "unchanged": true, "gen": current }));
                }
                // Se mira hacia atrás solo para construir la respuesta; luego se vuelve a la pantalla viva.
                parser.screen_mut().set_scrollback(offset as usize);
                let mut snap = snapshot(parser.screen());
                snap.gen = current;
                snap.scrollback = parser.screen().scrollback() as u16;
                snap.scrollback_max = parser.screen().scrollback_len() as u16;
                snap.bracketed_paste = parser.screen().bracketed_paste();
                parser.screen_mut().set_scrollback(0);
                Ok(serde_json::to_value(snap)?)
            }
            Request::Resize { pane, rows, cols } => {
                check_size(rows, cols)?;
                let (screen, screen_gen) = {
                    let st = self.state.lock().unwrap();
                    let p = &st.panes[&st.resolve(&pane)?];
                    if p.ghost {
                        return Err(no_proceso(&p.name));
                    }
                    (Arc::clone(&p.screen), Arc::clone(&p.gen))
                };
                // Orden fijo: primero la pantalla, luego el estado. Ningún camino toma el candado de la
                // pantalla con el del estado ya tomado, así que el orden no puede bloquearse. Mientras
                // se mantienen ambos, el PTY y la pantalla cambian de tamaño juntos.
                let mut parser = screen.lock().unwrap();
                {
                    let st = self.state.lock().unwrap();
                    let id = st.resolve(&pane)?;
                    st.panes[&id]
                        ._master
                        .as_ref()
                        .context("el panel no tiene PTY")?
                        .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
                        .context("no pude cambiar el tamaño del PTY")?;
                }
                let nr = std::num::NonZeroU16::new(rows).context("rows debe ser mayor que 0")?;
                let nc = std::num::NonZeroU16::new(cols).context("cols debe ser mayor que 0")?;
                parser.screen_mut().set_size(nr, nc);
                screen_gen.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            }
            Request::SetStatus { pane, status } => {
                let id = {
                    let mut st = self.state.lock().unwrap();
                    let id = st.resolve(&pane)?;
                    st.panes.get_mut(&id).unwrap().status = status;
                    id
                };
                self.notify.notify_waiters();
                self.try_inject(id);
                Ok(json!({}))
            }
            Request::Done { pane, summary, failed } => {
                let manager = {
                    let mut st = self.state.lock().unwrap();
                    let id = st.resolve(&pane)?;
                    // M4: un manager que ya no existe no debe tragarse el resultado en silencio.
                    if let Some(m) = st.panes[&id].manager {
                        if !st.panes.contains_key(&m) {
                            bail!("el manager ya no existe: el resultado no se entregó");
                        }
                    }
                    let kind = if failed { "failed" } else { "done" };
                    let p = st.panes.get_mut(&id).unwrap();
                    p.status = if failed { Status::Failed } else { Status::Done };
                    p.reported = true;
                    p.touched_since_report = false;
                    p.last_summary = Some(summary.clone());
                    let manager = p.manager;
                    if let Some(m) = manager {
                        st.push(m, id, kind, summary);
                    }
                    manager
                };
                // M2: el resultado queda en disco antes de responder "ok"; un SIGTERM después no lo pierde.
                let d = Arc::clone(self);
                tokio::task::spawn_blocking(move || d.persist_now()).await?;
                self.notify.notify_waiters();
                if let Some(m) = manager {
                    self.try_inject(m);
                }
                Ok(json!({ "delivered_to": manager }))
            }
            Request::Hook { pane, event, summary, source } => {
                let urgent = matches!(event, HookEvent::TurnEnded | HookEvent::Failed | HookEvent::Blocked);
                let target = {
                    let mut st = self.state.lock().unwrap();
                    let id = st.resolve(&pane)?;
                    let (manager, reported) = {
                        let p = &st.panes[&id];
                        (p.manager, p.reported)
                    };
                    match event {
                        HookEvent::PromptSubmitted => {
                            let p = st.panes.get_mut(&id).unwrap();
                            p.reported = false;
                            p.last_blocked = None;
                            p.status = Status::Working;
                        }
                        HookEvent::Working => {
                            // Una herramienta que termina no cambia un resultado ya entregado.
                            let p = st.panes.get_mut(&id).unwrap();
                            if !matches!(p.status, Status::Done | Status::Failed) {
                                p.status = Status::Working;
                            }
                        }
                        HookEvent::Idle => {
                            if !reported {
                                st.panes.get_mut(&id).unwrap().status = Status::Idle;
                            }
                        }
                        HookEvent::Blocked => {
                            let p = st.panes.get_mut(&id).unwrap();
                            let prev = std::mem::replace(&mut p.status, Status::Blocked);
                            let new_reason = summary.is_some() && summary != p.last_blocked;
                            p.last_blocked = summary.clone();
                            // El manager no debe esperar para siempre a alguien que espera a una persona.
                            if prev != Status::Blocked || new_reason {
                                if let Some(m) = manager {
                                    let why = summary.unwrap_or_else(|| "pide una decisión".into());
                                    st.push(m, id, "blocked", why);
                                }
                            }
                        }
                        HookEvent::TurnEnded | HookEvent::Failed => {
                            // Si ya hubo un `galgo done` explícito en este turno, no se duplica.
                            if !reported {
                                let failed = event == HookEvent::Failed;
                                match manager {
                                    Some(m) => {
                                        let text = summary.unwrap_or_else(|| {
                                            if failed { "terminó con un error" } else { "terminó su turno" }.into()
                                        });
                                        let p = st.panes.get_mut(&id).unwrap();
                                        p.status = if failed { Status::Failed } else { Status::Done };
                                        p.reported = true;
                                        p.touched_since_report = false;
                                        p.last_summary = Some(text.clone());
                                        st.push(m, id, if failed { "failed" } else { "done" }, text);
                                    }
                                    None => {
                                        st.panes.get_mut(&id).unwrap().status =
                                            if failed { Status::Failed } else { Status::Idle };
                                    }
                                }
                            }
                        }
                    }
                    manager
                };
                // M2: un resultado o un bloqueo entregado se guarda antes de responder.
                if urgent && target.is_some() {
                    let d = Arc::clone(self);
                    tokio::task::spawn_blocking(move || d.persist_now()).await?;
                }
                self.notify.notify_waiters();
                if let Some(m) = target {
                    self.try_inject(m);
                }
                // Se resuelve en una sentencia aparte: un `if let` mantendría el candado tomado
                // dentro del cuerpo y `try_inject` lo pediría de nuevo (interbloqueo).
                let own = self.state.lock().unwrap().resolve(&pane).ok();
                if let Some(id) = own {
                    self.try_inject(id);
                    // Un hook con fuente: se sigue el transcript de este panel desde ahora.
                    if let Some(src) = &source {
                        self.track_hook(id, src);
                    }
                }
                Ok(json!({}))
            }
            Request::UsageReport { pane, agent, model, context_tokens, context_window, cost_usd, limits } => {
                self.usage_report(&pane, agent, model, context_tokens, context_window, cost_usd, &limits)?;
                Ok(json!({}))
            }
            Request::Usage { group_by } => self.usage_view(group_by.as_deref()),
            Request::Wait { .. } => unreachable!("se atiende en dispatch"),
            Request::Inbox { pane } => {
                let st = self.state.lock().unwrap();
                let p = &st.panes[&st.resolve(&pane)?];
                let v: Vec<Message> = p
                    .inbox
                    .iter()
                    .filter(|m| !m.acked)
                    .map(|m| Message {
                        id: m.id,
                        from: m.from,
                        from_name: m.from_name.clone(),
                        kind: m.kind.into(),
                        summary: m.summary.clone(),
                        age_secs: m.created.elapsed().as_secs(),
                        acked: m.acked,
                    })
                    .collect();
                Ok(serde_json::to_value(v)?)
            }
            Request::Ack { pane, msg } => {
                {
                    let mut st = self.state.lock().unwrap();
                    let epoch = st.epoch;
                    let id = st.resolve(&pane)?;
                    let p = st.panes.get_mut(&id).unwrap();
                    let m = p.inbox.iter_mut().find(|m| m.id == msg).ok_or_else(|| {
                        if msg >> 32 != epoch {
                            anyhow!("el mensaje {msg} es de otra ejecución del daemon y ya no existe; no se confirmó nada")
                        } else {
                            anyhow!("mensaje {msg} no encontrado")
                        }
                    })?;
                    m.acked = true;
                    st.touch();
                }
                self.notify.notify_waiters();
                Ok(json!({}))
            }
            Request::Kill { pane } => {
                if let Some(out) = self.forget_ghost(&pane)? {
                    return Ok(out);
                }
                let (id, name, sid, exited, orphans) = {
                    let st = self.state.lock().unwrap();
                    let id = st.resolve(&pane)?;
                    let p = &st.panes[&id];
                    (id, p.name.clone(), p.pid, p.exited, p.orphans.clone())
                };
                let Some(sid) = sid else { bail!("el panel {name} ya terminó") };
                let sid = sid as i32;
                // Miembros de la sesión antes de cualquier señal: solo estos se barren (#13).
                // Con el líder ya salido no se vuelve a listar: su pid pudo reutilizarse.
                let before = if exited {
                    // Solo los capturados al salir el líder que siguen en la sesión.
                    orphans.iter().copied().filter(|&p| unsafe { libc::getsid(p) } == sid).collect()
                } else {
                    tokio::task::spawn_blocking(move || session_pids(sid)).await?
                };
                let mut exited = exited;
                if !exited {
                    let mut st = self.state.lock().unwrap();
                    if let Some(p) = st.panes.get_mut(&id) {
                        if p.exited {
                            exited = true;
                        } else if let Err(e) = p.killer.kill() {
                            // ESRCH: el líder terminó justo ahora.
                            if e.raw_os_error() == Some(ESRCH) {
                                exited = true;
                            } else {
                                return Err(e.into());
                            }
                        }
                    }
                }
                if exited {
                    // El líder ya salió: solo se barre si quedan procesos de su sesión (#12).
                    if before.is_empty() {
                        bail!("el panel {name} ya terminó");
                    }
                    tokio::task::spawn_blocking(move || kill_members(sid, &before)).await?;
                    return Ok(json!({}));
                }
                // SIGHUP no basta para quien lo ignora, ni para los trabajos que una shell puso en
                // otro grupo de la misma sesión. Tras una espera breve, SIGKILL a los miembros.
                tokio::time::sleep(Duration::from_secs(1)).await;
                tokio::task::spawn_blocking(move || kill_members(sid, &before)).await?;
                Ok(json!({}))
            }
            Request::Shutdown => {
                let socket = self.socket.clone();
                let me = Arc::clone(self);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    // Lo pendiente del hilo de guardado se escribe antes de salir.
                    me.persist_now();
                    let _ = std::fs::remove_file(socket);
                    std::process::exit(0);
                });
                Ok(json!({}))
            }
        }
    }

    /// Quita un panel restaurado (sin proceso). Su inbox sin confirmar pasa a su manager vivo. Si tiene
    /// mensajes pendientes y no hay manager al que pasarlos, se rechaza sin tocar nada: nunca se descarta
    /// un resultado en silencio.
    fn forget_ghost(&self, pane: &str) -> Result<Option<Value>> {
        let mut st = self.state.lock().unwrap();
        let id = st.resolve(pane)?;
        if !st.panes[&id].ghost {
            return Ok(None);
        }
        let manager = st.panes[&id].manager.filter(|m| st.panes.contains_key(m));
        let pending = st.panes[&id].inbox.iter().filter(|m| !m.acked).count();
        if pending > 0 && manager.is_none() {
            bail!(
                "el panel {pane} tiene {pending} mensaje(s) sin confirmar y no tiene manager al que pasarlos; confírmalos con galgo ack o reasigna un manager"
            );
        }
        let ghost = st.panes.remove(&id).unwrap();
        let mut moved = 0usize;
        for m in ghost.inbox.into_iter().filter(|m| !m.acked) {
            st.panes.get_mut(&manager.unwrap()).unwrap().inbox.push(m);
            moved += 1;
        }
        // Lo que apuntaba a este fantasma pasa a su manager (o queda sin manager): ningún resultado se queda colgado.
        for p in st.panes.values_mut() {
            if p.manager == Some(id) {
                p.manager = manager;
            }
        }
        st.touch();
        let out = json!({ "removed": ghost.name, "moved": moved });
        drop(st);
        self.untrack_usage(id);
        Ok(Some(out))
    }

    /// Reintenta cada cierto tiempo los mensajes que nadie ha confirmado.
    fn tick(&self) {
        let ids: Vec<u32> = self.state.lock().unwrap().panes.keys().copied().collect();
        for id in ids {
            self.try_inject(id);
        }
    }
}

/// Pids de la sesión `sid`. El PTY hace líder de sesión al hijo, así que su sid es su pid.
fn session_pids(sid: i32) -> Vec<i32> {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid="]).output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<i32>().ok())
        .filter(|&pid| unsafe { libc::getsid(pid) } == sid)
        .collect()
}

/// SIGKILL a los pids de `pids` que siguen perteneciendo a la sesión `sid`, salvo el propio daemon.
/// Se comprueba `getsid` justo antes de cada señal: un pid que salió y se reutilizó en otra sesión se omite.
fn kill_members(sid: i32, pids: &[i32]) {
    let me = unsafe { libc::getpid() };
    for &pid in pids {
        if pid != me && unsafe { libc::getsid(pid) } == sid {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

async fn serve(d: Arc<Daemon>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    // Líneas que llegan mientras se atiende una espera; se procesan en orden.
    let mut queued: VecDeque<String> = VecDeque::new();
    loop {
        let line = match queued.pop_front() {
            Some(l) => l,
            None => match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => break,
            },
        };
        let mut client_ack = false;
        let (resp, ack) = match serde_json::from_str::<Request>(&line) {
            Ok(req) if matches!(req, Request::Wait { .. }) => {
                client_ack = matches!(req, Request::Wait { client_ack: true, .. });
                let mut fut = std::pin::pin!(d.dispatch(req));
                let mut read_open = true;
                let (res, ack) = loop {
                    tokio::select! {
                        out = &mut fut => break out,
                        next = lines.next_line(), if read_open => match next {
                            Ok(Some(l)) => queued.push_back(l),
                            // Cerrar la lectura no es irse: el cliente puede seguir esperando su
                            // respuesta (shutdown(SHUT_WR)). Una desconexión total se nota al escribir.
                            _ => read_open = false,
                        },
                    }
                };
                (to_response(res), ack)
            }
            Ok(req) => {
                // Cualquier otra solicitud se ejecuta siempre, aunque el cliente cierre su escritura.
                let (res, ack) = d.dispatch(req).await;
                (to_response(res), ack)
            }
            Err(e) => (Response::err(format!("solicitud inválida: {e}")), None),
        };
        let mut out = serde_json::to_string(&resp).unwrap();
        out.push('\n');
        // Un cliente que no lee no puede bloquear la entrega para siempre.
        let written = matches!(
            tokio::time::timeout(d.write_timeout, w.write_all(out.as_bytes())).await,
            Ok(Ok(()))
        );
        if let Some((pane, msg)) = ack {
            // Con `client_ack` solo el cliente confirma, tras entregar: escribir en el socket
            // no prueba que alguien lo leyera.
            if written && client_ack {
                d.hold(pane, msg, d.ack_secs);
            } else if written {
                d.confirm(pane, msg);
                d.mark_dirty();
            } else {
                d.release(pane, msg);
            }
        }
        if !written {
            break;
        }
    }
}

/// Espera una señal de cierre, escribe el estado pendiente y sale.
async fn on_signal(d: Arc<Daemon>, kind: tokio::signal::unix::SignalKind) {
    let Ok(mut sig) = tokio::signal::unix::signal(kind) else { return };
    sig.recv().await;
    let socket = d.socket.clone();
    let _ = tokio::task::spawn_blocking(move || d.persist_now()).await;
    let _ = std::fs::remove_file(socket);
    std::process::exit(0);
}

#[tokio::main]
async fn main() -> Result<()> {
    let socket = galgo_proto::socket_path();
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    if socket.exists() {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            bail!("ya hay un daemon escuchando en {}", socket.display());
        }
        std::fs::remove_file(&socket)?;
    }
    // Estado del arranque anterior, antes de aceptar conexiones: inbox sin confirmar y paneles (que vuelven
    // sin proceso). Un archivo que no sirve se aparta con aviso. El epoch no depende del archivo.
    let state_path = persist::state_path(&socket);
    let legacy = persist::legacy_path(&socket);
    let (saved, migrated) = persist::load_with_migration(&state_path, legacy.as_deref());
    let mut state = State { epoch: persist::epoch_for(saved.as_ref()), ..State::default() };
    if let Some(s) = saved {
        persist::restore(&mut state, s);
    }
    let (persist_tx, persist_rx) = std::sync::mpsc::channel::<()>();
    let (usage_tx, usage_rx) = std::sync::mpsc::channel::<()>();
    let migration = migrated.map(|old| (old, state_path.clone()));
    let listener = UnixListener::bind(&socket)?;
    // Quien alcance el socket puede escribir en todos los paneles: solo el dueño.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let daemon = Arc::new(Daemon {
        state: Mutex::new(state),
        state_path,
        persist_lock: Mutex::new(()),
        persist_tx,
        usage: usage::UsageLive::new(usage_tx),
        notify: Notify::new(),
        socket: socket.clone(),
        orphan_secs: env_u64("GALGO_ORPHAN_SECS", 30),
        ack_secs: env_u64("GALGO_ACK_SECS", 5),
        retry: Duration::from_secs(env_u64("GALGO_RETRY_SECS", 60)),
        write_timeout: Duration::from_secs(env_u64("GALGO_WRITE_TIMEOUT_SECS", 10)),
    });
    // El epoch nuevo queda guardado antes de aceptar la primera conexión; luego el hilo de guardado
    // escribe lo que cambie, agrupado y fuera del hilo de las peticiones.
    daemon.persist_now();
    if let Some((old, new)) = migration {
        match persist::finish_migration(&old, &new) {
            Ok(Some(to)) => eprintln!("galgod: estado de la versión anterior migrado; el original queda en {}", to.display()),
            Ok(None) => {}
            Err(e) => eprintln!("galgod: {e:#}"),
        }
    }
    {
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || persist_loop(d, persist_rx));
    }
    // M2: cierre de sesión, `kill` o Ctrl-C: lo pendiente se escribe antes de salir.
    for kind in [
        tokio::signal::unix::SignalKind::terminate(),
        tokio::signal::unix::SignalKind::interrupt(),
        tokio::signal::unix::SignalKind::hangup(),
    ] {
        tokio::spawn(on_signal(Arc::clone(&daemon), kind));
    }
    {
        let d = Arc::clone(&daemon);
        std::thread::spawn(move || usage::poll_loop(d, usage_rx));
    }
    eprintln!("galgod escuchando en {}", socket.display());

    let d = Arc::clone(&daemon);
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(2));
        loop {
            t.tick().await;
            d.tick();
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve(Arc::clone(&daemon), stream));
    }
}

fn conv_color(c: vt100::Color) -> galgo_proto::Color {
    match c {
        vt100::Color::Default => galgo_proto::Color::Default,
        vt100::Color::Idx(i) => galgo_proto::Color::Idx(i),
        vt100::Color::Rgb(r, g, b) => galgo_proto::Color::Rgb(r, g, b),
    }
}

fn snapshot(screen: &vt100::Screen) -> galgo_proto::Snapshot {
    use galgo_proto::Run;
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let mut lines = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut runs: Vec<Run> = Vec::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let t = if cell.has_contents() { cell.contents().to_string() } else { " ".to_string() };
            let (fg, bg) = (conv_color(cell.fgcolor()), conv_color(cell.bgcolor()));
            let (bold, italic, underline, inverse) =
                (cell.bold(), cell.italic(), cell.underline(), cell.inverse());
            match runs.last_mut() {
                Some(l)
                    if l.fg == fg
                        && l.bg == bg
                        && l.bold == bold
                        && l.italic == italic
                        && l.underline == underline
                        && l.inverse == inverse =>
                {
                    l.t.push_str(&t)
                }
                _ => runs.push(Run { t, fg, bg, bold, italic, underline, inverse }),
            }
        }
        lines.push(runs);
    }
    galgo_proto::Snapshot {
        gen: 0,
        rows,
        cols,
        cursor: screen.cursor_position(),
        cursor_hidden: screen.hide_cursor(),
        lines,
        scrollback: 0,
        scrollback_max: 0,
        bracketed_paste: false,
        wrapped: (0..rows).map(|r| screen.row_wrapped(r)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tamanos_validos_y_limites() {
        assert!(check_size(24, 80).is_ok());
        assert!(check_size(MAX_ROWS, MAX_COLS).is_ok());
        assert!(check_size(0, 80).is_err());
        assert!(check_size(24, 0).is_err());
        assert!(check_size(MAX_ROWS + 1, 80).is_err());
        assert!(check_size(24, MAX_COLS + 1).is_err());
    }
}

#[cfg(test)]
mod tests_sesion {
    use super::*;

    /// M2/M3: solo se quitan marcadores de sesión; la configuración del usuario se conserva.
    #[test]
    fn marcadores_de_sesion_si_configuracion_no() {
        for k in ["CLAUDECODE", "CLAUDE_PID", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_MESSAGING_TOKEN", "HERDR_PANE_ID", "HERDR_ENV"] {
            assert!(is_session_marker(k), "{k} debe quitarse");
        }
        for k in ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_MAX_OUTPUT_TOKENS", "CLAUDE_EFFORT", "HOME", "PATH", "GALGO_DEBUG"] {
            assert!(!is_session_marker(k), "{k} debe conservarse");
        }
    }

    /// B3: el token une generación y offset; el mismo gen con otro offset es otra vista.
    #[test]
    fn token_distingue_offset() {
        assert_ne!(token(7, 0), token(7, 30));
        assert_eq!(token(7, 30) >> 16, 7);
        assert_eq!(token(7, 30) & 0xFFFF, 30);
    }

    /// B1: una generación nueva no empieza en un número pequeño; cabe en el token.
    #[test]
    fn generacion_inicial_no_es_pequena_y_cabe_en_el_token() {
        let g = fresh_gen();
        assert!(g > 1_000_000, "la generación inicial viene de la hora de arranque");
        assert!(g < (1 << 40));
    }
}

#[cfg(test)]
mod tests_workspace {
    use super::*;

    /// "<repo>/<rama>": el repo es el último componente de la raíz; el HEAD desligado deja solo el repo.
    #[test]
    fn nombre_de_workspace_desde_repo_y_rama() {
        assert_eq!(workspace_name("/Users/ana/proyectos/galgo", "feat/panel"), "galgo/feat/panel");
        assert_eq!(workspace_name("/Users/ana/proyectos/galgo", "main"), "galgo/main");
        assert_eq!(workspace_name("/Users/ana/proyectos/galgo", "HEAD"), "galgo");
        assert_eq!(workspace_name("/Users/ana/proyectos/galgo", ""), "galgo");
    }

    /// Sin cwd, o fuera de un repo, no hay workspace deducido ni error.
    #[test]
    fn sin_repo_no_hay_workspace() {
        assert_eq!(workspace_for(None), None);
        let dir = std::env::temp_dir().join(format!("galgo-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Un directorio temporal fuera de cualquier repo: git falla y la deducción no hace nada.
        let d = dir.to_string_lossy().to_string();
        let _ = workspace_for(Some(&d));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Un repo real: la deducción sale de git, con la rama actual.
    #[test]
    fn deduce_repo_y_rama_de_git() {
        let dir = std::env::temp_dir().join(format!("galgo-ws-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
        run(&["init", "-q", "-b", "feat-x"]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "x"]);
        let d = dir.to_string_lossy().to_string();
        let ws = workspace_for(Some(&d));
        std::fs::remove_dir_all(&dir).unwrap();
        // Los directorios temporales de macOS pueden estar bajo un enlace: solo se comprueba la rama y el nombre.
        let ws = ws.expect("git debe deducir el workspace");
        assert!(ws.ends_with("/feat-x"), "{ws}");
        assert!(ws.starts_with("galgo-ws-git-"), "{ws}");
    }
}

#[cfg(test)]
mod tests_fantasmas {
    use super::*;

    fn daemon_con(state: State) -> Arc<Daemon> {
        daemon_con_en(state, std::env::temp_dir().join("galgo-no-se-usa.json"))
    }

    fn daemon_con_en(state: State, state_path: PathBuf) -> Arc<Daemon> {
        let (tx, _rx) = std::sync::mpsc::channel();
        Arc::new(Daemon {
            state: Mutex::new(state),
            state_path,
            persist_lock: Mutex::new(()),
            persist_tx: tx,
            usage: usage::UsageLive::new(std::sync::mpsc::channel().0),
            notify: Notify::new(),
            socket: PathBuf::from("/dev/null"),
            orphan_secs: 30,
            ack_secs: 5,
            retry: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
        })
    }

    fn fantasma(id: u32, name: &str, manager: Option<u32>) -> Pane {
        Pane::ghost(id, name.into(), vec!["bash".into()], manager, vec![], None, None)
    }

    fn msg(id: u64, from: u32, summary: &str) -> InboxMsg {
        InboxMsg {
            id,
            from,
            from_name: "w".into(),
            kind: "done",
            summary: summary.into(),
            created: Instant::now(),
            acked: false,
            claimed: false,
            hold_until: None,
            last_inject: None,
            inherited: false,
        }
    }

    /// M1: quitar un fantasma pasa su inbox sin confirmar a su manager; el nombre queda libre.
    #[test]
    fn quitar_un_fantasma_pasa_su_inbox_al_manager() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "mgr", None));
        let mut w = fantasma(2, "w", Some(1));
        w.inbox.push(msg(10, 2, "resultado"));
        st.panes.insert(2, w);
        let d = daemon_con(st);
        let out = d.forget_ghost("w").unwrap().expect("era un fantasma");
        assert_eq!(out["moved"], 1);
        let st = d.state.lock().unwrap();
        assert!(!st.panes.contains_key(&2), "el fantasma desaparece");
        assert_eq!(st.panes[&1].inbox[0].summary, "resultado", "el resultado llega al manager");
    }

    /// M1: un panel con proceso no se trata como fantasma.
    #[test]
    fn un_panel_vivo_no_se_quita_como_fantasma() {
        let mut st = State { epoch: 3, ..State::default() };
        let mut vivo = fantasma(5, "vivo", None);
        vivo.ghost = false;
        st.panes.insert(5, vivo);
        let d = daemon_con(st);
        assert!(d.forget_ghost("vivo").unwrap().is_none());
    }

    /// M1: sin manager vivo, un fantasma con mensajes pendientes no se quita (no se pierde ningún resultado).
    #[test]
    fn no_quitar_un_fantasma_con_pendientes_sin_manager() {
        let mut st = State { epoch: 3, ..State::default() };
        let mut w = fantasma(2, "w", None);
        w.inbox.push(msg(10, 2, "importante"));
        st.panes.insert(2, w);
        let d = daemon_con(st);
        assert!(d.forget_ghost("w").is_err());
        let st = d.state.lock().unwrap();
        assert!(st.panes.contains_key(&2), "el fantasma sigue");
        assert_eq!(st.panes[&2].inbox[0].summary, "importante");
    }

    /// Id máximo con el epoch más alto y el contador a punto de desbordar: nunca llega a 2^53.
    #[test]
    fn ids_de_mensaje_caben_en_javascript() {
        let mut st = State { epoch: persist::EPOCH_MAX, next_msg: 0xFFFF_FFFE, ..State::default() };
        let a = st.next_msg_id();
        let b = st.next_msg_id();
        assert!(a < persist::ID_LIMIT && b < persist::ID_LIMIT, "{a} {b}");
        assert_eq!(a >> 32, persist::EPOCH_MAX, "el epoch va en la parte alta");
        assert_eq!(a as f64 as u64, a, "el id es exacto como número de JavaScript");
    }

    /// G1: el id se escribe como texto JSON y se lee como double (JavaScript): vuelve igual.
    #[test]
    fn id_maximo_sobrevive_a_leerlo_como_double() {
        let mut st = State { epoch: persist::EPOCH_MAX, next_msg: 0xFFFF_FFFE, ..State::default() };
        for _ in 0..2 {
            let id = st.next_msg_id();
            let json = serde_json::to_string(&json!({ "msg": id })).unwrap();
            let leido = serde_json::from_str::<serde_json::Value>(&json).unwrap();
            let as_double = leido["msg"].as_f64().unwrap();
            assert_eq!(as_double as u64, id, "{json}");
            assert_eq!(id.to_string().parse::<f64>().unwrap() as u64, id);
        }
    }

    /// M1: confirmar un mensaje (wait o ack) cambia el estado persistente.
    #[test]
    fn confirmar_cambia_el_estado_persistente() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "mgr", None));
        st.panes.get_mut(&1).unwrap().inbox.push(msg(10, 2, "x"));
        let d = daemon_con(st);
        let antes = d.state.lock().unwrap().version;
        d.confirm(1, 10);
        assert!(d.state.lock().unwrap().version > antes);
    }

    #[test]
    fn ack_cambia_el_estado_persistente() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "mgr", None));
        st.panes.get_mut(&1).unwrap().inbox.push(msg(10, 2, "x"));
        let d = daemon_con(st);
        let antes = d.state.lock().unwrap().version;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(d.handle(Request::Ack { pane: "1".into(), msg: 10 })).unwrap();
        assert!(d.state.lock().unwrap().version > antes);
    }

    /// M2: un resultado respondido con éxito ya está en disco cuando llega la respuesta.
    #[test]
    fn done_queda_en_disco_antes_de_responder() {
        let dir = std::env::temp_dir().join(format!("galgo-m2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let path = dir.join("g.state.json");
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "mgr", None));
        let mut w = fantasma(2, "w", Some(1));
        w.ghost = false;
        st.panes.insert(2, w);
        let d = daemon_con_en(st, path.clone());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(d.handle(Request::Done { pane: "w".into(), summary: "resultadoB".into(), failed: false })).unwrap();
        // Sin esperar al hilo de guardado: el archivo ya lo tiene.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("resultadoB"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M3: una consulta a git que no responde se corta al límite; no retiene el spawn.
    #[test]
    fn git_colgado_se_corta_al_limite() {
        let dir = std::env::temp_dir().join(format!("galgo-m3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("git-colgado");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let t = Instant::now();
        assert_eq!(git_program(script.to_str().unwrap(), "/", &["rev-parse"]), None);
        assert!(t.elapsed() < Duration::from_secs(5), "tardó {:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B1: un GIT_DIR heredado no llega a git: el comando ve su propio repo, no el del entorno.
    #[test]
    fn git_no_hereda_git_dir() {
        let dir = std::env::temp_dir().join(format!("galgo-b1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("git-eco");
        std::fs::write(&script, "#!/bin/sh\nif [ -z \"${GIT_DIR+x}\" ]; then echo limpio; else echo heredado; fi\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // Se pasa al hijo sin tocar el entorno del proceso de pruebas (las demás pruebas usan git en paralelo).
        let out = git_program_with(script.to_str().unwrap(), "/", &["x"], &[("GIT_DIR", "/ruta/que/no/existe")]);
        assert_eq!(out.as_deref(), Some("limpio"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2: nombres y workspaces sin caracteres de control y con longitud acotada; lo que deduce git se limpia.
    #[test]
    fn nombres_y_workspaces_sin_caracteres_de_control() {
        assert!(check_text("el nombre", "ok", MAX_LABEL).is_ok());
        assert!(check_text("el nombre", "a\u{1b}[31mb", MAX_LABEL).is_err());
        assert!(check_text("el nombre", "a\nb", MAX_LABEL).is_err());
        assert!(check_text("el workspace", &"x".repeat(MAX_LABEL + 1), MAX_LABEL).is_err());
        assert_eq!(clean_label("ws\u{1b}[31m\n── falso"), "ws[31m── falso");
    }

    /// B3: el mismo manager hereda el inbox marcado, y ese mensaje no se inyecta en el terminal.
    #[test]
    fn mismo_manager_hereda_marcado_y_no_se_inyecta() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "mgr", None));
        let mut g = fantasma(2, "w", Some(1));
        g.inbox.push(msg(10, 1, "viejo"));
        st.panes.insert(2, g);
        assert_eq!(st.ghost_plan("w", Some(1)).unwrap(), Some((2, true)));
        let mut nuevo = fantasma(3, "w", Some(1));
        nuevo.ghost = false;
        assert_eq!(st.absorb_ghost(2, 3, true, &mut nuevo), 1);
        assert!(nuevo.inbox[0].inherited);
        assert!(!is_injectable(&nuevo.inbox[0]), "un heredado no se escribe en el terminal");
        assert!(!st.panes.contains_key(&2));
    }

    /// B3: con otro manager, los pendientes del fantasma van a su manager; el panel nuevo no hereda nada.
    #[test]
    fn otro_manager_recibe_los_pendientes_del_fantasma() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "viejo-mgr", None));
        st.panes.insert(4, fantasma(4, "nuevo-mgr", None));
        let mut g = fantasma(2, "w", Some(1));
        g.inbox.push(msg(10, 2, "para-el-viejo"));
        st.panes.insert(2, g);
        assert_eq!(st.ghost_plan("w", Some(4)).unwrap(), Some((2, false)));
        let mut nuevo = fantasma(3, "w", Some(4));
        nuevo.ghost = false;
        assert_eq!(st.absorb_ghost(2, 3, false, &mut nuevo), 0);
        assert!(nuevo.inbox.is_empty());
        assert_eq!(st.panes[&1].inbox[0].summary, "para-el-viejo");
    }

    /// B3: sin manager vivo para sus pendientes, el fantasma no se sustituye: se rechaza sin tocar nada.
    #[test]
    fn sin_manager_vivo_no_se_sustituye_con_pendientes() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(4, fantasma(4, "nuevo-mgr", None));
        let mut g = fantasma(2, "w", None);
        g.inbox.push(msg(10, 2, "importante"));
        st.panes.insert(2, g);
        assert!(st.ghost_plan("w", Some(4)).is_err());
        assert!(st.panes.contains_key(&2));
    }

    /// M4: quien apuntaba al fantasma pasa a apuntar al panel que lo sustituye; su resultado sigue llegando.
    #[test]
    fn al_sustituir_un_fantasma_sus_workers_apuntan_al_nuevo() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(1, fantasma(1, "mgr", None));
        st.panes.insert(5, fantasma(5, "worker", Some(1)));
        let (gid, inherit) = st.ghost_plan("mgr", None).unwrap().expect("es un fantasma");
        let mut nuevo = fantasma(3, "mgr", None);
        nuevo.ghost = false;
        st.absorb_ghost(gid, 3, inherit, &mut nuevo);
        assert_eq!(st.panes[&5].manager, Some(3));
    }

    /// M4: un panel no puede ser su propio manager, ni siquiera tras reutilizar el nombre de un fantasma.
    #[test]
    fn un_panel_no_es_su_propio_manager() {
        let mut st = State { epoch: 3, ..State::default() };
        st.panes.insert(2, fantasma(2, "m", None));
        assert!(st.ghost_plan("m", Some(2)).is_err());
    }
}
