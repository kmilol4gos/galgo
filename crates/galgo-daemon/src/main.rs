//! galgod: daemon que posee los PTYs, el estado de pantalla y la entrega de mensajes entre agentes.

mod cwd;
mod peer;
mod persist;
mod usage;

use anyhow::{anyhow, bail, Context, Result};
use galgo_proto::{HookEvent, Message, PaneInfo, Request, Response, Status, TaskInfo, TaskStatus, WorktreeAction};
use galgo_usage::PaneUsage;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
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
    /// Directorio de trabajo declarado al crear el panel. El vivo sale de `cwd_vivo`.
    cwd: Option<String>,
    /// Último directorio anunciado por el shell con OSC 7 (lo escribe el hilo lector).
    osc_cwd: Arc<Mutex<Option<String>>>,
    /// Profundidad de la última tarea que recibió este panel (0 si ninguna). No se reinicia al cerrarla (M2).
    profundidad_recibida: u8,
    /// Panel de un agente en modo orquestador (A3): no actúa sobre otros paneles. Se guarda con el estado.
    orquestador: bool,
    /// Último título que el programa anunció por OSC 0/2 (lo escribe el hilo lector).
    osc_titulo: Arc<Mutex<Option<String>>>,
    /// Uso del transcript (lo rellena el hilo de sondeo) y último informe del agente. Solo en memoria.
    usage: Option<PaneUsage>,
    usage_report: Option<usage::ReportData>,
    inbox: Vec<InboxMsg>,
    last_summary: Option<String>,
    /// Título de la tarea que declara el agente (solo en memoria).
    title: Option<String>,
    /// Se avisa cada vez que cambia la pantalla (la lee el hilo de salida) y al cerrar el panel: lo usan las suscripciones.
    changed: Arc<Notify>,
    /// Ha enviado eventos de hook de agente (un agente real, aunque no lo lanzara galgo). Solo en memoria.
    hooked: bool,
    /// Último motivo de bloqueo avisado al manager; un motivo nuevo se avisa aunque ya estuviera blocked.
    last_blocked: Option<String>,
    /// La línea del cursor es un prompt libre. Lo mantiene el hilo lector (y el redimensionado), que ya tiene la
    /// pantalla. El estado lo lee sin tomar el candado de la pantalla: así no hay interbloqueo (ver `ORDEN DE CANDADOS`).
    prompt_libre: Arc<AtomicBool>,
    /// Proceso en primer plano cuando el agente terminó su turno: si cambia, la marca de listo ya no vale (M2).
    listo_fg: Option<i32>,
    /// Último proceso en primer plano visto por `info`: si cambia, el título OSC del anterior se olvida (B4).
    fg_visto: Mutex<Option<i32>>,
    /// Desde cuándo el agente terminó su turno (Stop). Solo entonces se le entregan tareas (A2). Solo en memoria.
    listo_desde: Option<Instant>,
    /// Última entrada del usuario o del CLI en el pty. No se entrega tras teclear hace poco (A2). Solo en memoria.
    ultima_entrada: Option<Instant>,
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
    /// El agente de este panel tiene el terminal: el proceso en primer plano es un agente (por su ruta), o es el propio
    /// proceso del panel y el panel se lanzó como agente. Un shell libre, o un programa ajeno, no lo es (A1). Un panel
    /// sin pty (solo en pruebas) no tiene nada que comprobar.
    fn agente_en_primer_plano(&self) -> bool {
        if self.ghost || self.exited {
            return false;
        }
        let Some(master) = self._master.as_ref() else { return true };
        let Some(fg) = master.process_group_leader() else { return false };
        if let Some(ruta) = cwd::ruta_de_pid(fg) {
            if es_agente_ruta(&ruta) {
                return true;
            }
            // B1: una instalación por npm corre como `node …/claude/cli.js`: ahí manda la línea de órdenes.
            let nombre = std::path::Path::new(&ruta).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if matches!(nombre.as_str(), "node" | "bun" | "deno") && args_de_pid(fg).is_some_and(|a| es_agente_args(&a)) {
                return true;
            }
        }
        self.pid.is_some_and(|p| p as i32 == fg) && is_agent_cmd(&self.cmd)
    }

    /// Proceso en primer plano del pty, o None sin pty.
    fn fg_pid(&self) -> Option<i32> {
        self._master.as_ref().and_then(|m| m.process_group_leader())
    }

    /// El turno terminó (Stop o Idle): el agente puede recibir su tarea tras la gracia, si sigue en el mismo proceso.
    fn marcar_listo(&mut self) {
        self.listo_desde = Some(Instant::now());
        self.listo_fg = self.fg_pid();
    }

    /// El título OSC que sigue vigente: el del proceso en primer plano actual. Si el proceso cambió, el título del
    /// anterior no se hereda (B4: un `htop` no sale con el título de `vim`).
    fn titulo_vigente(&self) -> Option<String> {
        let fg = self.fg_pid();
        let mut visto = self.fg_visto.lock().unwrap();
        if *visto != fg {
            *visto = fg;
            *self.osc_titulo.lock().unwrap() = None;
        }
        self.osc_titulo.lock().unwrap().clone()
    }

    /// La línea del cursor es un prompt libre (lo mantiene el hilo lector). Sin tomar el candado de la pantalla.
    fn linea_de_prompt_libre(&self) -> bool {
        self.prompt_libre.load(Ordering::SeqCst)
    }

    /// Nombre del programa en primer plano (`vim`, `claude`, o el shell si está libre).
    fn programa(&self) -> Option<String> {
        if self.ghost || self.exited {
            return None;
        }
        let en_primer_plano = self._master.as_ref().and_then(|m| m.process_group_leader());
        en_primer_plano.and_then(cwd::nombre_de_pid).or_else(|| self.pid.and_then(|p| cwd::nombre_de_pid(p as i32)))
    }

    /// Directorio de trabajo vivo: el del proceso en primer plano del pty (su grupo de procesos), luego el del
    /// líder de sesión (el shell), luego lo último que anunció OSC 7 y, por último, el declarado al crear el panel.
    fn cwd_vivo(&self) -> Option<String> {
        if !self.ghost && !self.exited {
            let en_primer_plano = self._master.as_ref().and_then(|m| m.process_group_leader());
            if let Some(c) = en_primer_plano.and_then(cwd::cwd_de_pid) {
                return Some(c);
            }
            if let Some(c) = self.pid.and_then(|p| cwd::cwd_de_pid(p as i32)) {
                return Some(c);
            }
        }
        self.osc_cwd.lock().ok().and_then(|g| g.clone()).or_else(|| self.cwd.clone())
    }

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
            osc_cwd: Arc::new(Mutex::new(None)),
            osc_titulo: Arc::new(Mutex::new(None)),
            usage: None,
            usage_report: None,
            inbox,
            last_summary: None,
            title: None,
            last_blocked: None,
            hooked: false,
            profundidad_recibida: 0,
            orquestador: false,
            listo_desde: None,
            listo_fg: None,
            fg_visto: Mutex::new(None),
            ultima_entrada: None,
            prompt_libre: Arc::new(AtomicBool::new(false)),
            changed: Arc::new(Notify::new()),
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
    /// Tareas delegadas entre agentes (modo orquestador) y el último id usado.
    tasks: Vec<Task>,
    next_task: u64,
    /// Delegaciones recientes (remitente, momento unix) para el tope por hora (M2). Solo en memoria.
    delegaciones: Vec<(u32, u64)>,
}

// ORDEN DE CANDADOS (revisión 17, A1). Solo estos caminos toman dos a la vez, siempre en este orden:
//   1. pantalla (`Pane::screen`) → estado (`State`), en `Resize`.
// El estado nunca toma la pantalla: la línea del cursor llega como `Pane::prompt_libre`, un indicador atómico que
// mantienen el hilo lector y el redimensionado, que ya tienen la pantalla. Así `tick` y `deliver` (bajo el estado)
// no esperan a nadie que espere al estado. Un camino nuevo que tome la pantalla con el estado tomado es un error.

/// Límites del modo orquestador (corte 1): profundidad de delegación, tareas abiertas por par de agentes y por workspace.
const MAX_DELEGATION_DEPTH: u8 = 3;
const MAX_OPEN_PER_PAIR: usize = 3;
const MAX_OPEN_PER_WORKSPACE: usize = 8;
/// Tiempo que un agente debe llevar con el turno terminado (y sin teclear) antes de recibir una tarea (A2).
const GRACIA_ENTREGA: Duration = Duration::from_secs(3);
/// M2: tareas que un mismo remitente puede delegar por hora.
const MAX_DELEGATIONS_PER_HOUR: usize = 20;
/// Largo máximo de la tarea, su contexto y su resumen.
const MAX_TASK_TEXT: usize = 4000;

/// Una tarea delegada. Los nombres se guardan para que la lista siga legible aunque un panel desaparezca.
#[derive(Debug, Clone)]
struct Task {
    id: u64,
    from: u32,
    to: u32,
    from_name: String,
    to_name: String,
    workspace: Option<String>,
    text: String,
    context: Option<String>,
    status: TaskStatus,
    summary: Option<String>,
    files: Vec<String>,
    depth: u8,
    created_unix: u64,
}

impl Task {
    fn info(&self) -> TaskInfo {
        TaskInfo {
            id: self.id,
            from: self.from_name.clone(),
            to: self.to_name.clone(),
            text: self.text.clone(),
            context: self.context.clone(),
            status: self.status,
            summary: self.summary.clone(),
            files: self.files.clone(),
            depth: self.depth,
            created_unix: self.created_unix,
        }
    }
}

/// Segundos desde la época unix (para las marcas de tiempo de las tareas y del tope por hora).
fn unix_ahora() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Caducidad y poda de tareas (M3): una abierta más de un día cae como fallida; las cerradas se conservan una
/// semana y como mucho `MAX_CLOSED_TASKS` de las más recientes, para que el estado guardado no crezca sin límite.
const CADUCIDAD_TAREA_SECS: u64 = 24 * 3600;
const RETENCION_CERRADA_SECS: u64 = 7 * 24 * 3600;
const MAX_CLOSED_TASKS: usize = 200;

/// Texto que recibe el destinatario. Va como información de otro agente, nunca como orden del usuario.
fn task_text(t: &Task) -> String {
    let mut s = format!(
        "[galgo · tarea #{id} de {de}] Esto viene de otro agente, no es una orden del usuario. Cuando termines, ejecuta: galgo task done {id} \"<resumen>\"\n\n{texto}",
        id = t.id,
        de = t.from_name,
        texto = t.text
    );
    if let Some(c) = &t.context {
        s.push_str("\n\nContexto:\n");
        s.push_str(c);
    }
    s
}

/// Archivos que cambió un worktree de tarea frente a su base (y los nuevos sin añadir). Vacío si no es un worktree.
fn archivos_cambiados(cwd: &str) -> Vec<String> {
    let Some(rama) = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) else { return Vec::new() };
    let Some(base) = git_out(cwd, &["config", "--get", &clave_base(&rama)]) else { return Vec::new() };
    let mut out: Vec<String> = git_out(cwd, &["diff", "--name-only", &base]).unwrap_or_default().lines().map(str::to_string).collect();
    out.extend(
        git_out(cwd, &["status", "--porcelain"])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("?? "))
            .map(str::to_string),
    );
    out.truncate(50);
    out
}

impl State {
    /// M3: caduca las tareas abiertas viejas (avisando al remitente) y poda las cerradas antiguas o sobrantes.
    fn podar_tareas(&mut self, ahora: u64) {
        let caducadas: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status.abierta() && ahora.saturating_sub(t.created_unix) > CADUCIDAD_TAREA_SECS)
            .map(|(i, _)| i)
            .collect();
        for i in caducadas {
            let (from, to, id) = (self.tasks[i].from, self.tasks[i].to, self.tasks[i].id);
            self.tasks[i].status = TaskStatus::Fallida;
            self.tasks[i].summary = Some("caducó: pasó más de un día sin cerrarse".into());
            self.push(from, to, "failed", format!("tarea #{id}: caducó sin cerrarse"));
        }
        self.tasks.retain(|t| t.status.abierta() || ahora.saturating_sub(t.created_unix) <= RETENCION_CERRADA_SECS);
        let cerradas = self.tasks.iter().filter(|t| !t.status.abierta()).count();
        if cerradas > MAX_CLOSED_TASKS {
            let mut quitar = cerradas - MAX_CLOSED_TASKS;
            // Las más antiguas primero (la lista va en orden de creación).
            self.tasks.retain(|t| {
                if !t.status.abierta() && quitar > 0 {
                    quitar -= 1;
                    false
                } else {
                    true
                }
            });
        }
        self.touch();
    }

    /// M3: tras reiniciar el daemon, ninguna tarea abierta llega a su destinatario (todos los paneles son fantasmas).
    /// Cada una falla y el remitente se entera, en lugar de bloquear el cupo para siempre.
    fn fallar_tareas_sin_destino(&mut self) {
        let abiertas: Vec<usize> = self.tasks.iter().enumerate().filter(|(_, t)| t.status.abierta()).map(|(i, _)| i).collect();
        for i in abiertas {
            let (from, to, id) = (self.tasks[i].from, self.tasks[i].to, self.tasks[i].id);
            self.tasks[i].status = TaskStatus::Fallida;
            self.tasks[i].summary = Some("el daemon se reinició antes de que terminara".into());
            self.push(from, to, "failed", format!("tarea #{id}: el daemon se reinició y el agente ya no está; la tarea se da por fallida"));
        }
    }

    /// Marca un cambio persistente.
    fn touch(&mut self) {
        self.version += 1;
    }

    /// Profundidad de una delegación de `from` a `to`, o el motivo por el que no se permite (frenos del modo orquestador).
    fn check_delegation(&self, from: u32, to: u32, workspace: Option<&str>) -> Result<u8> {
        if from == to {
            bail!("un agente no se delega una tarea a sí mismo");
        }
        // M4: no se devuelve una tarea a quien te la delegó (ciclo): el manager nunca queda con profundidad 3.
        if self.tasks.iter().any(|t| t.status.abierta() && t.to == from && t.from == to) {
            bail!("no se le devuelve una tarea a quien te la delegó");
        }
        // M2: la profundidad es la de la última tarea que recibió el remitente, y no se reinicia al cerrarla: un
        // agente que trabaja en una tarea de profundidad d delega a d+1. Un manager (sin tareas recibidas) delega a 1.
        let profundidad = self.panes.get(&from).map_or(1, |p| p.profundidad_recibida + 1);
        if profundidad > MAX_DELEGATION_DEPTH {
            bail!("profundidad máxima de delegación ({MAX_DELEGATION_DEPTH}) alcanzada");
        }
        // M2: tope por hora por remitente: cerrar y reabrir tareas no es gratis, aunque el par y el workspace estén a salvo.
        let ahora = unix_ahora();
        let en_la_hora = self.delegaciones.iter().filter(|(f, at)| *f == from && ahora.saturating_sub(*at) < 3600).count();
        if en_la_hora >= MAX_DELEGATIONS_PER_HOUR {
            bail!("el panel ya delegó {en_la_hora} tareas en la última hora (máximo {MAX_DELEGATIONS_PER_HOUR})");
        }
        let par = self.tasks.iter().filter(|t| t.status.abierta() && t.from == from && t.to == to).count();
        if par >= MAX_OPEN_PER_PAIR {
            bail!("ya hay {par} tareas abiertas entre esos dos agentes: espera a que terminen");
        }
        // M2: los paneles sin workspace comparten un cupo propio, en lugar de no tener límite.
        let clave = workspace.unwrap_or("");
        let en_ws = self.tasks.iter().filter(|t| t.status.abierta() && t.workspace.as_deref().unwrap_or("") == clave).count();
        if en_ws >= MAX_OPEN_PER_WORKSPACE {
            bail!("el workspace ya tiene {en_ws} tareas abiertas (máximo {MAX_OPEN_PER_WORKSPACE})");
        }
        Ok(profundidad)
    }

    /// Entrega con las comprobaciones del panel real: antes de entregar, se mira si el agente sigue en primer plano (A1).
    fn entregar(&mut self, pane: u32) {
        let fuera = self.panes.get(&pane).is_some_and(|p| p.hooked && !p.agente_en_primer_plano());
        if fuera {
            // Ya no es el agente (salió, o hay un shell delante): no se le entrega nada, y hasta su próximo hook no es
            // agente. M2: lo que sabíamos de su turno (listo, resultado, estado) se olvida con él.
            // Solo se olvida lo que decide la entrega (el turno terminado). El estado y el resultado se conservan: un hook
            // de un shell no debe borrar el resultado que ya reportó el panel.
            if let Some(p) = self.panes.get_mut(&pane) {
                p.hooked = false;
                p.listo_desde = None;
                p.listo_fg = None;
            }
        }
        self.deliver(pane);
    }

    /// Entrega al destinatario su tarea pendiente más antigua, si está libre: con hooks, sin trabajar, sin estar bloqueado
    /// ni en un diálogo. Va pegada (bracketed paste) y enviada; nunca si no ha emitido ningún hook.
    fn deliver(&mut self, pane: u32) {
        let ahora = Instant::now();
        let libre = self.panes.get(&pane).is_some_and(|p| {
            p.hooked
                && !p.ghost
                && !p.exited
                && matches!(p.status, Status::Idle | Status::Done | Status::Failed)
                // A2: solo tras el fin de un turno, con un rato de gracia, sin teclear hace poco y con el cursor en un prompt vacío.
                && p.listo_desde.is_some_and(|t| ahora.duration_since(t) >= GRACIA_ENTREGA)
                && p.listo_fg == p.fg_pid()
                && p.ultima_entrada.is_none_or(|t| ahora.duration_since(t) >= GRACIA_ENTREGA)
                && p.linea_de_prompt_libre()
        });
        if !libre {
            return;
        }
        let Some(idx) = self.tasks.iter().position(|t| t.to == pane && t.status == TaskStatus::Pendiente) else { return };
        let texto = task_text(&self.tasks[idx]);
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(texto.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~\r");
        let Some(p) = self.panes.get_mut(&pane) else { return };
        if p.input.try_send(bytes).is_err() {
            return;
        }
        p.status = Status::Working;
        p.reported = false;
        p.touched_since_report = true;
        // M4: la profundidad sube con las entregas; solo una orden humana la devuelve a cero (ver `Send`).
        p.profundidad_recibida = p.profundidad_recibida.max(self.tasks[idx].depth);
        p.listo_desde = None;
        p.listo_fg = None;
        self.tasks[idx].status = TaskStatus::Entregada;
        self.touch();
    }

    /// Tras un hook del destinatario, su tarea abierta sigue su estado (en curso, bloqueada) y se intenta entregar la siguiente.
    fn sync_tasks(&mut self, pane: u32) {
        let estado = self.panes.get(&pane).map(|p| p.status);
        if let Some(estado) = estado {
            for t in self.tasks.iter_mut().filter(|t| t.to == pane && t.status.abierta()) {
                t.status = match (estado, t.status) {
                    (Status::Blocked, _) => TaskStatus::Bloqueada,
                    (Status::Working, TaskStatus::Entregada | TaskStatus::Bloqueada) => TaskStatus::EnCurso,
                    (_, s) => s,
                };
            }
        }
        self.entregar(pane);
    }

    /// El destinatario terminó su proceso: sus tareas abiertas fallan y el delegador lo sabe.
    fn fail_tasks_of(&mut self, pane: u32) {
        let abiertas: Vec<usize> = self.tasks.iter().enumerate().filter(|(_, t)| t.to == pane && t.status.abierta()).map(|(i, _)| i).collect();
        for i in abiertas {
            let (from, id) = (self.tasks[i].from, self.tasks[i].id);
            self.tasks[i].status = TaskStatus::Fallida;
            self.tasks[i].summary = Some("el agente terminó antes de cerrar la tarea".into());
            self.push(from, pane, "failed", format!("tarea #{id}: el agente terminó antes de cerrarla"));
        }
    }

    /// Id nuevo de mensaje: el epoch en la parte alta, así un ack de otra ejecución no lo confunde.
    fn next_msg_id(&mut self) -> u64 {
        self.next_msg += 1;
        (self.epoch << 32) | (self.next_msg & 0xFFFF_FFFF)
    }

    /// El panel vivo cuyo proceso (su líder de pty) está entre los ancestros de `pid`: el remitente de esa llamada.
    /// Sin panel en la cadena, no hay remitente (A2, revisión 17).
    fn panel_de_ascendencia(&self, pid: i32) -> Option<u32> {
        let cadena = peer::ascendencia(pid);
        cadena.iter().find_map(|&a| self.panes.values().find(|p| !p.ghost && !p.exited && p.pid == Some(a as u32)).map(|p| p.id))
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
            cwd: p.cwd_vivo(),
            programa: p.programa(),
            titulo_osc: p.titulo_vigente(),
            usage: usage::merged(p.usage.as_ref(), p.usage_report.as_ref()),
            title: p.title.clone(),
            agent: p.hooked || is_agent_cmd(&p.cmd) || p.usage.is_some() || p.usage_report.is_some(),
            hooked: p.hooked,
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

/// Caracteres de formato invisibles o de dirección (Cf): pueden invertir o esconder un texto en la pantalla (B1).
fn formato_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// M4: caracteres que no se ven pero el modelo lee: etiquetas Unicode, uso privado y el resto de formato invisible
/// (bidi, ancho cero, marcas de dirección). Un texto que los trae puede llevar instrucciones que el humano no ve.
fn invisible_para_el_modelo(c: char) -> bool {
    formato_invisible(c)
        || matches!(
            c,
            '\u{0600}'..='\u{0605}' | '\u{06DD}' | '\u{070F}' | '\u{08E2}' | '\u{110BD}' | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E007F}'
                | '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}'
        )
}

/// Texto de una tarea, su contexto o su resumen: además de los controles, nada invisible para el modelo (M4).
fn check_texto_de_tarea(what: &str, text: &str, max: usize) -> Result<()> {
    if text.chars().any(invisible_para_el_modelo) {
        bail!("{what} trae caracteres invisibles (formato, uso privado o etiquetas Unicode)");
    }
    check_text(what, text, max)
}

fn check_text(what: &str, text: &str, max: usize) -> Result<()> {
    if text.chars().any(|c| char::is_control(c) || formato_invisible(c)) {
        bail!("{what} no puede tener caracteres de control o de formato invisible");
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

/// Clave de config donde se guarda el commit base de un worktree de tarea.
fn clave_base(rama: &str) -> String {
    format!("branch.{rama}.galgo-base")
}

/// Cambios de un worktree de tarea frente a su commit base: estadística (recortada a `max` líneas si se pide)
/// y si hay archivos sin confirmar. `None` si el directorio no es un worktree de tarea.
fn resumen_de_cambios(cwd: &str, max: Option<usize>) -> Option<String> {
    let rama = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let base = git_out(cwd, &["config", "--get", &clave_base(&rama)])?;
    let stat = git_out(cwd, &["diff", "--stat", &base]);
    let sucio = git_out(cwd, &["status", "--porcelain"]).is_some();
    let mut lineas = vec![format!("Cambios en el worktree de la tarea (rama {rama}):")];
    match stat {
        Some(s) => {
            let todas: Vec<&str> = s.lines().collect();
            let (mostradas, resto) = match max {
                Some(m) if todas.len() > m => (&todas[..m], todas.len() - m),
                _ => (&todas[..], 0),
            };
            lineas.extend(mostradas.iter().map(|l| l.to_string()));
            if resto > 0 {
                lineas.push(format!("… y {resto} líneas más (galgo worktree diff)"));
            }
        }
        None => lineas.push("sin cambios respecto a la base".into()),
    }
    if sucio {
        lineas.push("hay archivos sin confirmar en el worktree".into());
    }
    Some(lineas.join("\n"))
}

/// Datos de un worktree de tarea desde un directorio: rama, commit base, repo principal y raíz del worktree.
struct TareaWt {
    rama: String,
    principal: String,
    top: String,
}

fn tarea_de(cwd: &str) -> std::result::Result<TareaWt, String> {
    let sin = || "este panel no tiene un worktree de tarea".to_string();
    let top = git_run(cwd, &["rev-parse", "--show-toplevel"]).map_err(|_| sin())?;
    let rama = git_run(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).map_err(|_| sin())?;
    // Sin el commit base en la config, no es un worktree de tarea: no se toca nada.
    git_run(cwd, &["config", "--get", &clave_base(&rama)]).map_err(|_| sin())?;
    let listado = git_run(cwd, &["worktree", "list", "--porcelain"])?;
    // La primera entrada del listado es siempre el repo principal.
    let principal = listado.lines().find_map(|l| l.strip_prefix("worktree ")).map(str::to_string).ok_or_else(sin)?;
    if principal == top {
        return Err(sin());
    }
    Ok(TareaWt { rama, principal, top })
}

/// Ejecuta la acción sobre el worktree de tarea. `ocupado`: el agente sigue trabajando (no se toca nada).
fn worktree_accion(cwd: &str, action: WorktreeAction, confirm: bool, ocupado: bool) -> std::result::Result<Value, String> {
    let t = tarea_de(cwd)?;
    match action {
        WorktreeAction::Diff => {
            let texto = resumen_de_cambios(&t.top, None).unwrap_or_else(|| "sin cambios".into());
            Ok(json!({ "texto": texto }))
        }
        WorktreeAction::Accept | WorktreeAction::Discard => {
            let accion = if action == WorktreeAction::Accept { "fusionar" } else { "descartar" };
            if ocupado {
                return Err("el panel sigue trabajando: espera a que termine".into());
            }
            if !confirm {
                return Err(format!("para {accion} la rama {} añade --yes", t.rama));
            }
            if action == WorktreeAction::Accept {
                if git_out(&t.top, &["status", "--porcelain"]).is_some() {
                    return Err("hay cambios sin confirmar en el worktree: haz commit antes".into());
                }
                if git_out(&t.principal, &["status", "--porcelain"]).is_some() {
                    return Err("el repo principal tiene cambios sin confirmar: no se fusiona nada".into());
                }
                let mensaje = format!("Fusiona {} (tarea de galgo)", t.rama);
                if let Err(e) = git_run(&t.principal, &["merge", "--no-ff", "-m", &mensaje, &t.rama]) {
                    let _ = git_run(&t.principal, &["merge", "--abort"]);
                    return Err(format!("no se fusionó (se deshizo el intento): {e}"));
                }
                git_run(&t.principal, &["worktree", "remove", &t.top])?;
                git_run(&t.principal, &["branch", "-d", &t.rama])?;
                let _ = git_run(&t.principal, &["config", "--unset", &clave_base(&t.rama)]);
                Ok(json!({ "mensaje": format!("fusionada {} en el repo principal; worktree y rama borrados", t.rama) }))
            } else {
                git_run(&t.principal, &["worktree", "remove", "--force", &t.top])?;
                git_run(&t.principal, &["branch", "-D", &t.rama])?;
                let _ = git_run(&t.principal, &["config", "--unset", &clave_base(&t.rama)]);
                Ok(json!({ "mensaje": format!("descartada {} sin fusionar; worktree y rama borrados", t.rama) }))
            }
        }
    }
}

/// Carpeta de los worktrees de tareas: `~/.galgo/worktrees/<repo>/<rama>`.
fn worktrees_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".galgo").join("worktrees")
}

/// Ejecuta git sin tiempo límite (crear un worktree puede tardar con un repo grande) y devuelve su salida o su error.
fn git_run(dir: &str, args: &[&str]) -> std::result::Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // B3: el repo del agente no ejecuta sus propios programas (fsmonitor, hooks): git no lee esa configuración.
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for var in GIT_ENV_REMOVE {
        cmd.env_remove(var);
    }
    let out = cmd.output().map_err(|e| format!("no pude ejecutar git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Nombre de rama que git acepta y que no puede parecer una opción (no empieza por `-`) ni llevar espacios.
fn rama_valida(dir: &str, rama: &str) -> std::result::Result<(), String> {
    let mal = || format!("nombre de rama no válido: {rama:?}");
    if rama.starts_with('-') || rama.contains("@{") || rama.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(mal());
    }
    git_run(dir, &["check-ref-format", "--branch", rama]).map(|_| ()).map_err(|_| mal())
}

/// Crea el worktree de una tarea: rama nueva desde HEAD en `<base>/<repo>/<rama>`. Devuelve su ruta y la raíz
/// del repo original (de ahí sale el workspace). No borra nada: si la rama o la carpeta ya existen, falla.
fn crear_worktree(dir: &str, rama: &str, base: &Path) -> std::result::Result<(PathBuf, String), String> {
    let top = git_run(dir, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{dir} no está en un repo git"))?;
    rama_valida(&top, rama)?;
    let repo = Path::new(&top).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let carpeta: String = rama.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let path = base.join(repo).join(carpeta);
    if path.exists() {
        return Err(format!("ya existe {}", path.display()));
    }
    if let Some(padre) = path.parent() {
        std::fs::create_dir_all(padre).map_err(|e| format!("no pude crear {}: {e}", padre.display()))?;
    }
    let base = git_run(&top, &["rev-parse", "HEAD"])?;
    let destino = path.to_string_lossy().into_owned();
    git_run(&top, &["worktree", "add", "-b", rama, &destino])?;
    // El commit base queda en la config del repo: el daemon compara contra él sin guardar estado propio.
    git_run(&top, &["config", &clave_base(rama), &base])?;
    Ok((path, top))
}

/// Deduce el workspace de un directorio dentro de un repo git: "<repo>/<rama>". Sin git, o con el
/// HEAD desligado, devuelve `None` o solo el nombre del repo. Nunca falla: git ausente no es un error.
/// Comando de un agente conocido (por nombre de ejecutable).
/// Si la línea del cursor de `s` es un prompt libre: el agente espera una orden y no hay texto a medias. Lo que hay
/// tras el prompt, sin contar los bordes de caja de la TUI, tiene que ser nada.
fn prompt_libre_de(s: &vt100::Screen) -> bool {
    // A2: un diálogo (confirmación, selección de inicio de sesión) oculta el cursor: no es un prompt. Y una línea
    // vacía tampoco basta: el prompt tiene que estar a la vista y terminar en su carácter.
    if s.hide_cursor() {
        return false;
    }
    let (fila, _) = s.cursor_position();
    let (_, cols) = s.size();
    let texto = s.contents_between(fila, 0, fila, cols.get());
    let limpio = texto.trim_end().trim_end_matches(|c: char| matches!(c, '│' | '║' | '|')).trim_end();
    !limpio.is_empty() && limpio.ends_with(['>', '❯', '›', '$', '%', '#'])
}

/// Una ruta de ejecutable es la de un agente conocido (`…/claude/versions/2.0.50`, `…/bin/codex`).
fn es_agente_ruta(ruta: &str) -> bool {
    ruta.split('/').any(|c| c == "claude" || c == "codex")
}

/// La línea de órdenes de un proceso tiene un componente claude o codex (B1: `node …/@openai/codex/bin/codex.js`).
fn es_agente_args(args: &str) -> bool {
    args.split_whitespace().any(|t| t.split('/').any(|c| c == "claude" || c == "codex"))
}

/// Argumentos de un proceso (`ps -o args=`), o None si no existe.
fn args_de_pid(pid: i32) -> Option<String> {
    let out = Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
    let texto = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!texto.is_empty()).then_some(texto)
}

fn is_agent_cmd(cmd: &[String]) -> bool {
    cmd.first()
        .and_then(|c| std::path::Path::new(c).file_name())
        .map(|n| matches!(n.to_string_lossy().as_ref(), "claude" | "codex" | "pi" | "opencode"))
        .unwrap_or(false)
}

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
        // B3: el repo del agente no ejecuta sus propios programas (fsmonitor, hooks): git no lee esa configuración.
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
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
    // Variables que dejan la sesión de Claude Code, el terminal o el multiplexor que lanzó el daemon: un panel que las
    // hereda se cree otro terminal o otra sesión. TERM_PROGRAM y su versión se fijan después, para galgo.
    const EXACT: &[&str] = &[
        "CLAUDECODE",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERM_SESSION_ID",
        "TERMINFO",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "WT_SESSION",
        "VTE_VERSION",
        "ITERM_SESSION_ID",
        "ITERM_PROFILE",
        "KITTY_WINDOW_ID",
        "KITTY_PID",
        "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET",
        "GHOSTTY_RESOURCES_DIR",
        "ALACRITTY_SOCKET",
        "ALACRITTY_WINDOW_ID",
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
    const PREFIJOS: [&str; 6] = ["HERDR_", "ITERM_", "KITTY_", "WEZTERM_", "GHOSTTY_", "ALACRITTY_"];
    EXACT.contains(&key) || PREFIJOS.iter().any(|p| key.starts_with(p))
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
        // galgo es el terminal de estos paneles: los programas que miran TERM_PROGRAM lo ven a él, no a quien lo lanzó.
        builder.env("TERM_PROGRAM", "galgo");
        builder.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
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
        let changed = Arc::new(Notify::new());
        let osc_cwd: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let osc_hilo = Arc::clone(&osc_cwd);
        let osc_titulo: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let titulo_hilo = Arc::clone(&osc_titulo);
        let prompt = Arc::new(AtomicBool::new(false));
        let prompt_hilo = Arc::clone(&prompt);

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
            osc_cwd,
            osc_titulo,
            prompt_libre: prompt,
            usage: None,
            usage_report: None,
            inbox: Vec::new(),
            last_summary: None,
            title: None,
            last_blocked: None,
            hooked: false,
            profundidad_recibida: 0,
            orquestador: false,
            listo_desde: None,
            listo_fg: None,
            fg_visto: Mutex::new(None),
            ultima_entrada: None,
            changed: Arc::clone(&changed),
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
            let mut osc = cwd::Osc::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let anuncios = osc.feed(&buf[..n]);
                        if let Some(dir) = anuncios.cwd.filter(|d| cwd::ruta_segura(d)) {
                            *osc_hilo.lock().unwrap() = Some(dir);
                        }
                        if let Some(titulo) = anuncios.titulo {
                            *titulo_hilo.lock().unwrap() = Some(titulo);
                        }
                        let mut parser = screen.lock().unwrap();
                        parser.process(&buf[..n]);
                        prompt_hilo.store(prompt_libre_de(parser.screen()), Ordering::SeqCst);
                        gen.fetch_add(1, Ordering::SeqCst);
                        // Las suscripciones se enteran sin sondear: un aviso por trozo de salida.
                        changed.notify_waiters();
                    }
                }
            }
            changed.notify_waiters();
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
            st.fail_tasks_of(id);
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
    async fn dispatch(self: &Arc<Self>, req: Request, sender: Option<u32>) -> (Result<Value>, Option<(u32, u64)>) {
        // Solo lo que puede cambiar el inbox o la lista de paneles avisa al guardado. Un `Send` no cambia
        // nada persistente (el texto no se guarda), y las lecturas tampoco: no se escribe nada por ellos.
        let mutating = matches!(
            req,
            Request::Spawn { .. } | Request::Done { .. } | Request::Hook { .. }
                | Request::Ack { .. } | Request::Kill { .. } | Request::Wait { .. }
                | Request::Delegate { .. } | Request::TaskDone { .. }
        );
        let out = match req {
            Request::Wait { waiter, target, timeout_secs, .. } => {
                self.wait_for(waiter, target, timeout_secs).await
            }
            other => (self.handle(sender, other).await, None),
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

    /// A3 (revisión 17): lo que un panel puede pedir al daemon. Sin panel (la CLI del usuario, las pruebas) no hay
    /// restricción. Un panel solo actúa sobre sí mismo en lo suyo (su resultado, su buzón, su estado, sus hooks); sobre
    /// otros, solo dentro de su workspace, y un agente orquestado nunca. Ningún panel detiene el daemon.
    fn autorizar(&self, sender: Option<u32>, req: &Request) -> Result<()> {
        let Some(s) = sender else { return Ok(()) };
        let st = self.state.lock().unwrap();
        let (orquestado, ws_propio) = st.panes.get(&s).map_or((false, None), |p| (p.orquestador, p.workspace.clone()));
        let propio = |pane: &str| -> Result<bool> { Ok(st.resolve(pane)? == s) };
        let alcance = |pane: &str| -> Result<()> {
            let id = st.resolve(pane)?;
            if id == s {
                return Ok(());
            }
            if orquestado {
                bail!("un agente orquestado solo actúa sobre su propio panel");
            }
            if st.panes[&id].workspace != ws_propio {
                bail!("{pane} está en otro workspace: un panel solo actúa dentro del suyo");
            }
            Ok(())
        };
        match req {
            Request::Done { pane, .. }
            | Request::Inbox { pane }
            | Request::Ack { pane, .. }
            | Request::SetStatus { pane, .. }
            | Request::Hook { pane, .. }
            | Request::UsageReport { pane, .. }
            | Request::SetTitle { pane, .. } => {
                if !propio(pane)? {
                    bail!("un panel solo actúa sobre sí mismo ({pane} no es el suyo)");
                }
            }
            Request::Send { pane, .. }
            | Request::Read { pane, .. }
            | Request::Snapshot { pane, .. }
            | Request::Resize { pane, .. }
            | Request::Watch { pane }
            | Request::Kill { pane }
            | Request::Worktree { pane, .. } => alcance(pane)?,
            Request::Spawn { .. } if orquestado => bail!("un agente orquestado no lanza paneles"),
            Request::Shutdown => bail!("un panel no detiene el daemon"),
            _ => {}
        }
        Ok(())
    }

    async fn handle(self: &Arc<Self>, sender: Option<u32>, req: Request) -> Result<Value> {
        self.autorizar(sender, &req)?;
        match req {
            Request::Spawn { name, cmd, cwd, manager, inject, rows, cols, workspace, worktree, orquestador } => {
                let rama = worktree.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
                let (cwd, workspace) = match rama {
                    // Worktree propio: el panel arranca dentro, y el workspace sale del repo original, no del worktree.
                    Some(rama) => {
                        let dir = cwd.clone().ok_or_else(|| anyhow!("un worktree necesita un directorio de trabajo"))?;
                        let base = worktrees_base();
                        let r = rama.clone();
                        let (path, top) = tokio::task::spawn_blocking(move || crear_worktree(&dir, &r, &base))
                            .await?
                            .map_err(|e| anyhow!("{e}"))?;
                        let workspace = workspace
                            .filter(|w| !w.trim().is_empty())
                            .or_else(|| Some(clean_label(&workspace_name(&top, &rama))).filter(|w| !w.is_empty()));
                        (Some(path.to_string_lossy().into_owned()), workspace)
                    }
                    None => {
                        // git puede tardar (red, disco lento): la consulta va fuera del hilo de peticiones y con tiempo límite.
                        let workspace = match workspace.filter(|w| !w.trim().is_empty()) {
                            Some(w) => Some(w),
                            None => {
                                let dir = cwd.clone();
                                tokio::task::spawn_blocking(move || workspace_for(dir.as_deref())).await?
                            }
                        };
                        (cwd, workspace)
                    }
                };
                let (id, inherited) = self.spawn_pane(name, cmd, cwd, manager, inject, rows, cols, workspace)?;
                let mut st = self.state.lock().unwrap();
                if let Some(p) = st.panes.get_mut(&id) {
                    p.orquestador = orquestador;
                }
                st.touch();
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
                // M4: una orden humana (sin panel detrás) empieza un trabajo nuevo: la cadena de profundidad vuelve a cero.
                if sender.is_none() && enter {
                    p.profundidad_recibida = 0;
                }
                let mut chunk = text.into_bytes();
                if enter {
                    chunk.push(b'\r');
                }
                if p.ghost {
                    return Err(no_proceso(&p.name));
                }
                p.input.try_send(chunk).map_err(input_error)?;
                p.ultima_entrada = Some(Instant::now());
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
                // Pantalla tomada → estado: el único orden permitido (ver ORDEN DE CANDADOS).
                let prompt = {
                    let st = self.state.lock().unwrap();
                    st.resolve(&pane).ok().and_then(|id| st.panes.get(&id)).map(|p| Arc::clone(&p.prompt_libre))
                };
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
                if let Some(prompt) = prompt {
                    prompt.store(prompt_libre_de(parser.screen()), Ordering::SeqCst);
                }
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
                // Si el panel trabaja en un worktree de tarea, el resultado lleva los cambios para el manager.
                let cwd = {
                    let st = self.state.lock().unwrap();
                    st.resolve(&pane).ok().and_then(|id| st.panes[&id].cwd_vivo())
                };
                let cambios = tokio::task::spawn_blocking(move || cwd.as_deref().and_then(|c| resumen_de_cambios(c, Some(15))))
                    .await?;
                let summary = match cambios {
                    Some(c) => format!("{summary}\n{c}"),
                    None => summary,
                };
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
                    // Un panel no puede hablar por otro: si la llamada viene de un panel, el hook es de ese panel.
                    // A2: un hook lo envía el agente de su propio panel. Sin panel en la cadena de procesos, se rechaza.
                    if sender != Some(id) {
                        bail!("los hooks solo los envía el agente de su propio panel ({pane})");
                    }
                    // Un hook de agente marca el panel como agente real (lo muestra la GUI).
                    st.panes.get_mut(&id).unwrap().hooked = true;
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
                            p.listo_desde = None;
                        }
                        HookEvent::Working => {
                            // Una herramienta que termina no cambia un resultado ya entregado.
                            let p = st.panes.get_mut(&id).unwrap();
                            p.listo_desde = None;
                            if !matches!(p.status, Status::Done | Status::Failed) {
                                p.status = Status::Working;
                            }
                        }
                        HookEvent::Idle => {
                            // A2: un aviso de espera no saca de Blocked: puede ser el diálogo de un permiso.
                            let p = st.panes.get_mut(&id).unwrap();
                            if !reported && p.status != Status::Blocked {
                                p.status = Status::Idle;
                            }
                            // M1: un agente que espera la orden (idle_prompt, o el SessionStart de Codex al arrancar)
                            // está libre. La heurística del prompt (cursor visible) evita los diálogos de arranque.
                            if p.status == Status::Idle {
                                p.marcar_listo();
                            }
                        }
                        HookEvent::Blocked => {
                            let p = st.panes.get_mut(&id).unwrap();
                            p.listo_desde = None;
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
                            // A2: el turno terminó: solo ahora (y tras un rato) se le puede entregar una tarea.
                            st.panes.get_mut(&id).unwrap().marcar_listo();
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
                    // Modo orquestador: la tarea del destinatario sigue su estado y, si ya está libre, se le entrega la siguiente.
                    st.sync_tasks(id);
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
            Request::SetTitle { pane, title } => {
                check_text("el título", &title, MAX_LABEL)?;
                let mut st = self.state.lock().unwrap();
                let id = st.resolve(&pane)?;
                let title = title.trim().to_string();
                st.panes.get_mut(&id).unwrap().title = (!title.is_empty()).then_some(title);
                Ok(json!({}))
            }
            Request::Delegate { to, task, context } => {
                check_texto_de_tarea("la tarea", &task, MAX_TASK_TEXT)?;
                if let Some(c) = &context {
                    check_texto_de_tarea("el contexto", c, MAX_TASK_TEXT)?;
                }
                let info = {
                    let mut st = self.state.lock().unwrap();
                    let from_id = sender.ok_or_else(|| anyhow!("solo un panel de galgo puede delegar: esta llamada no viene de ninguno"))?;
                    let to_id = st.resolve(&to)?;
                    let (from_name, workspace) = {
                        let p = &st.panes[&from_id];
                        (p.name.clone(), p.workspace.clone())
                    };
                    let to_name = {
                        let p = &st.panes[&to_id];
                        // M1: solo se delega dentro del propio workspace.
                        if p.workspace != workspace {
                            bail!("{} está en otro workspace: solo se delega dentro del propio", p.name);
                        }
                        if p.exited && !p.ghost {
                            bail!("el destinatario {} ya terminó", p.name);
                        }
                        p.name.clone()
                    };
                    let depth = st.check_delegation(from_id, to_id, workspace.as_deref())?;
                    st.next_task += 1;
                    let id = st.next_task;
                    let created_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    st.tasks.push(Task {
                        id,
                        from: from_id,
                        to: to_id,
                        from_name,
                        to_name,
                        workspace,
                        text: task,
                        context,
                        status: TaskStatus::Pendiente,
                        summary: None,
                        files: Vec::new(),
                        depth,
                        created_unix,
                    });
                    st.touch();
                    let ahora = unix_ahora();
                    st.delegaciones.retain(|(_, at)| ahora.saturating_sub(*at) < 3600);
                    st.delegaciones.push((from_id, ahora));
                    st.podar_tareas(ahora);
                    st.entregar(to_id);
                    st.tasks.iter().find(|t| t.id == id).map(Task::info)
                };
                self.notify.notify_waiters();
                Ok(serde_json::to_value(info)?)
            }
            Request::TaskDone { id, summary, ok } => {
                check_texto_de_tarea("el resumen", &summary, MAX_TASK_TEXT)?;
                // Solo el destinatario de la tarea la cierra. El remitente sale de su panel, nunca de los argumentos.
                let (cwd, from_id) = {
                    let st = self.state.lock().unwrap();
                    let from_id = sender.ok_or_else(|| anyhow!("solo un panel de galgo puede cerrar una tarea: esta llamada no viene de ninguno"))?;
                    let t = st.tasks.iter().find(|t| t.id == id).ok_or_else(|| anyhow!("tarea {id} no encontrada"))?;
                    if t.to != from_id {
                        bail!("solo el agente al que se delegó la tarea {id} puede cerrarla");
                    }
                    if !t.status.abierta() {
                        bail!("la tarea {id} ya está cerrada");
                    }
                    (st.panes.get(&from_id).and_then(|p| p.cwd_vivo()), from_id)
                };
                let files = tokio::task::spawn_blocking(move || cwd.map(|c| archivos_cambiados(&c)).unwrap_or_default()).await?;
                let manager = {
                    let mut st = self.state.lock().unwrap();
                    let idx = st.tasks.iter().position(|t| t.id == id).ok_or_else(|| anyhow!("tarea {id} no encontrada"))?;
                    st.tasks[idx].status = if ok { TaskStatus::Lista } else { TaskStatus::Fallida };
                    st.tasks[idx].summary = Some(summary.clone());
                    st.tasks[idx].files = files.clone();
                    let manager = st.tasks[idx].from;
                    let quien = st.tasks[idx].to_name.clone();
                    let mut texto = format!("tarea #{id} {} por {quien}: {summary}", if ok { "lista" } else { "fallida" });
                    if !files.is_empty() {
                        texto.push_str(&format!("\narchivos cambiados: {}", files.join(", ")));
                    }
                    if st.panes.contains_key(&manager) {
                        st.push(manager, from_id, if ok { "done" } else { "failed" }, texto);
                    }
                    st.touch();
                    manager
                };
                self.try_inject(manager);
                self.notify.notify_waiters();
                Ok(json!({ "ok": true }))
            }
            Request::Tasks { pane } => {
                let st = self.state.lock().unwrap();
                let filtro = match pane {
                    Some(p) => Some(st.resolve(&p)?),
                    None => None,
                };
                // M1: un agente solo ve las tareas de su workspace (sin workspace: solo las suyas).
                let alcance = sender.map(|s| (st.panes.get(&s).and_then(|p| p.workspace.clone()), s));
                let v: Vec<TaskInfo> = st
                    .tasks
                    .iter()
                    .filter(|t| filtro.is_none_or(|f| t.from == f || t.to == f))
                    .filter(|t| match &alcance {
                        None => true,
                        Some((Some(ws), _)) => t.workspace.as_deref() == Some(ws.as_str()),
                        Some((None, s)) => t.from == *s || t.to == *s,
                    })
                    .map(Task::info)
                    .collect();
                Ok(serde_json::to_value(v)?)
            }
            Request::Worktree { pane, action, confirm } => {
                let (cwd, ocupado) = {
                    let st = self.state.lock().unwrap();
                    let p = &st.panes[&st.resolve(&pane)?];
                    (p.cwd_vivo(), matches!(p.status, Status::Working | Status::Blocked))
                };
                let cwd = cwd.ok_or_else(|| anyhow!("el panel no tiene directorio de trabajo"))?;
                let v = tokio::task::spawn_blocking(move || worktree_accion(&cwd, action, confirm, ocupado))
                    .await?
                    .map_err(|e| anyhow!("{e}"))?;
                Ok(v)
            }
            Request::Wait { .. } => unreachable!("se atiende en dispatch"),
            Request::Watch { .. } => Err(anyhow!("la suscripción se atiende en su conexión")),
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
        // A1 y B2: se revisa qué agentes siguen en primer plano y se reintenta entregar lo pendiente (un panel que
        // queda libre por otra vía, sin hook, también recibe su tarea).
        let mut st = self.state.lock().unwrap();
        let antes = st.version;
        let ids: Vec<u32> = st.panes.keys().copied().collect();
        for id in ids {
            st.entregar(id);
        }
        if st.version != antes {
            drop(st);
            self.mark_dirty();
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

/// Escribe una línea JSON; `false` si el cliente no la lee (o la conexión cayó).
async fn send_json<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, v: Value) -> bool {
    let mut out = v.to_string();
    out.push('\n');
    matches!(tokio::time::timeout(Duration::from_secs(5), w.write_all(out.as_bytes())).await, Ok(Ok(())))
}

impl Daemon {
    /// Suscripción a un panel: escribe `changed` cada vez que su pantalla cambia (sin sondeo: el hilo de salida avisa),
    /// y `closed` al cerrarse el panel. Termina si el cliente corta la conexión o escribe algo.
    async fn watch<R, W>(self: &Arc<Self>, pane: String, lines: &mut tokio::io::Lines<BufReader<R>>, w: &mut W)
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let found = {
            let st = self.state.lock().unwrap();
            st.resolve(&pane).ok().and_then(|id| st.panes.get(&id).map(|p| (id, Arc::clone(&p.changed), Arc::clone(&p.gen))))
        };
        let Some((id, changed, gen)) = found else {
            let _ = send_json(w, json!({ "ok": false, "error": format!("panel no encontrado: {pane}") })).await;
            return;
        };
        if !send_json(w, json!({ "ok": true, "event": "watching", "pane": id })).await {
            return;
        }
        let mut last = None;
        loop {
            // El aviso queda registrado antes de leer la generación: un cambio entre las dos cosas no se pierde
            // (sin `enable`, un aviso que llega antes de la primera espera se perdería hasta el respaldo de 1 s).
            let aviso = changed.notified();
            tokio::pin!(aviso);
            aviso.as_mut().enable();
            let g = gen.load(Ordering::SeqCst);
            if last != Some(g) {
                last = Some(g);
                if !send_json(w, json!({ "ok": true, "event": "changed", "gen": g })).await {
                    return;
                }
            }
            let cerrado = {
                let st = self.state.lock().unwrap();
                st.panes.get(&id).map_or(true, |p| p.exited)
            };
            if cerrado {
                let _ = send_json(w, json!({ "ok": true, "event": "closed", "pane": id })).await;
                return;
            }
            tokio::select! {
                _ = &mut aviso => {}
                // Un red de seguridad por si un aviso se pierde: revisa el estado del panel una vez por segundo.
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = lines.next_line() => return,
            }
        }
    }
}

async fn serve(d: Arc<Daemon>, stream: UnixStream) {
    use std::os::unix::io::AsRawFd;
    // El proceso que conectó: de él sale el remitente de las llamadas de agente (ver peer.rs).
    // El remitente se fija al conectar (B2): el pid del par no se vuelve a evaluar en cada solicitud.
    let sender = match peer::pid_del_par(stream.as_raw_fd()) {
        Some(pid) => d.state.lock().unwrap().panel_de_ascendencia(pid),
        None => None,
    };
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
            // La suscripción es dueña de la conexión: al terminar, se cierra.
            Ok(Request::Watch { pane }) => {
                d.watch(pane, &mut lines, &mut w).await;
                break;
            }
            Ok(req) if matches!(req, Request::Wait { .. }) => {
                client_ack = matches!(req, Request::Wait { client_ack: true, .. });
                let mut fut = std::pin::pin!(d.dispatch(req, sender));
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
                let (res, ack) = d.dispatch(req, sender).await;
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
            let (dim, strike, blink) = (cell.dim(), cell.strikethrough(), cell.blink());
            match runs.last_mut() {
                Some(l)
                    if l.fg == fg
                        && l.bg == bg
                        && l.bold == bold
                        && l.italic == italic
                        && l.underline == underline
                        && l.inverse == inverse
                        && l.dim == dim
                        && l.strike == strike
                        && l.blink == blink =>
                {
                    l.t.push_str(&t)
                }
                _ => runs.push(Run { t, fg, bg, bold, italic, underline, inverse, dim, strike, blink }),
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
mod tests_estilos {
    use super::*;

    /// P1.6: tachado (SGR 9), atenuado (SGR 2) y parpadeo (SGR 5) llegan al snapshot como atributos de cada tramo.
    /// vt100 sin el parche los descartaba (vendor/vt100/PATCH-GALGO.md).
    #[test]
    fn tachado_atenuado_y_parpadeo_llegan_al_snapshot() {
        use std::num::NonZeroU16;
        let mut p = vt100::Parser::new(NonZeroU16::new(2).unwrap(), NonZeroU16::new(20).unwrap(), 0);
        p.process(b"\x1b[9mA\x1b[29m\x1b[2mB\x1b[22m\x1b[5mC\x1b[25mD");
        let snap = snapshot(p.screen());
        let tramos = &snap.lines[0];
        let de = |t: &str| tramos.iter().find(|r| r.t.starts_with(t)).unwrap_or_else(|| panic!("falta el tramo {t}: {tramos:?}"));
        assert!(de("A").strike && !de("A").dim && !de("A").blink);
        assert!(de("B").dim && !de("B").strike);
        assert!(de("C").blink && !de("C").strike);
        assert!(!de("D").strike && !de("D").dim && !de("D").blink);
    }
}

#[cfg(test)]
mod tests_sesion {
    use super::*;

    /// M2/M3: solo se quitan marcadores de sesión; la configuración del usuario se conserva.
    #[test]
    fn marcadores_de_sesion_si_configuracion_no() {
        for k in ["CLAUDECODE", "CLAUDE_PID", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_MESSAGING_TOKEN", "HERDR_PANE_ID", "HERDR_ENV", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERMINFO", "TMUX", "ITERM_SESSION_ID", "KITTY_WINDOW_ID", "WEZTERM_PANE"] {
            assert!(is_session_marker(k), "{k} debe quitarse");
        }
        for k in ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_MAX_OUTPUT_TOKENS", "HOME", "PATH", "GALGO_DEBUG", "COLORTERM", "LANG", "CLAUDE_EFFORT", "TERMINFO_DIRS"] {
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

    pub(crate) fn daemon_con(state: State) -> Arc<Daemon> {
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

    pub(crate) fn fantasma(id: u32, name: &str, manager: Option<u32>) -> Pane {
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
        rt.block_on(d.handle(None, Request::Ack { pane: "1".into(), msg: 10 })).unwrap();
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
        rt.block_on(d.handle(None, Request::Done { pane: "w".into(), summary: "resultadoB".into(), failed: false })).unwrap();
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

    /// Paso 2 (rex): el título de la tarea llega a la lista de paneles; uno vacío lo borra; un título con caracteres
    /// de control se rechaza.
    #[test]
    fn titulo_de_la_tarea_llega_a_la_lista() {
        let mut st = State { epoch: 3, ..State::default() };
        let mut p = fantasma(1, "claude", None);
        p.ghost = false;
        st.panes.insert(1, p);
        let d = daemon_con(st);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(d.handle(None, Request::SetTitle { pane: "claude".into(), title: "  bug de ventas  ".into() })).unwrap();
        let v = {
            let st = d.state.lock().unwrap();
            st.info(&st.panes[&1], 30)
        };
        assert_eq!(v.title.as_deref(), Some("bug de ventas"));
        rt.block_on(d.handle(None, Request::SetTitle { pane: "1".into(), title: "  ".into() })).unwrap();
        let v = {
            let st = d.state.lock().unwrap();
            st.info(&st.panes[&1], 30)
        };
        assert!(v.title.is_none(), "un título vacío lo borra");
        assert!(rt.block_on(d.handle(None, Request::SetTitle { pane: "1".into(), title: "x\u{1b}[31m".into() })).is_err());
    }
}

#[cfg(test)]
mod r14_tests {
    use super::*;

    /// B1 (review-14): un carácter de dirección (RLO) o de ancho cero no entra en un nombre ni en un título.
    #[test]
    fn r14_formato_invisible_se_rechaza() {
        assert!(check_text("el título", "bug \u{202E}ventas", MAX_LABEL).is_err());
        assert!(check_text("el nombre", "a\u{200B}b", MAX_LABEL).is_err());
        assert!(check_text("el título", "bug de ventas", MAX_LABEL).is_ok());
    }
}

#[cfg(test)]
mod worktree_tests {
    use super::*;

    /// Repo git de prueba con un commit, en un directorio temporal propio del test.
    fn repo_de_prueba(nombre: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("galgo-wt-{nombre}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();
        git_run(&d, &["init", "-q"]).unwrap();
        // Identidad propia del repo de prueba: el merge y los commits de los worktrees la necesitan.
        git_run(&d, &["config", "user.name", "galgo"]).unwrap();
        git_run(&d, &["config", "user.email", "galgo@test"]).unwrap();
        git_run(&d, &["commit", "-q", "--allow-empty", "-m", "base"]).unwrap();
        dir
    }

    /// Escribe un archivo en el worktree (sin confirmar aún).
    fn escribir(dir: &Path, nombre: &str) {
        std::fs::write(dir.join(nombre), "contenido\n").unwrap();
    }

    /// Un panel de tarea arranca en su propio worktree, con una rama nueva y el workspace del repo original.
    #[test]
    fn crear_worktree_deja_rama_y_carpeta() {
        let repo = repo_de_prueba("crear");
        let base = repo.join("..").join(format!("galgo-wt-base-{}", std::process::id()));
        let d = repo.to_string_lossy().into_owned();
        let (path, top) = crear_worktree(&d, "tarea/uno", &base).unwrap();
        assert!(path.join(".git").exists(), "el worktree tiene su .git");
        assert_eq!(git_run(&path.to_string_lossy(), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "tarea/uno");
        assert_eq!(workspace_name(&top, "tarea/uno"), format!("{}/tarea/uno", repo.file_name().unwrap().to_string_lossy()));
        // La rama ya existe: falla sin tocar el worktree anterior.
        assert!(crear_worktree(&d, "tarea/uno", &base).is_err());
        let _ = git_run(&d, &["worktree", "remove", "--force", &path.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Un nombre que git no acepta, o que parece una opción, no crea nada.
    #[test]
    fn rama_peligrosa_se_rechaza() {
        let repo = repo_de_prueba("nombres");
        let base = repo.join("..").join(format!("galgo-wt-nombres-base-{}", std::process::id()));
        let d = repo.to_string_lossy().into_owned();
        for mala in ["-f", "a..b", "con espacio", "x@{1}", "rama.lock", ""] {
            assert!(crear_worktree(&d, mala, &base).is_err(), "{mala:?} debe rechazarse");
        }
        assert!(!base.exists(), "ningún nombre malo llega a crear carpetas");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Fuera de un repo git no hay worktree posible.
    #[test]
    fn fuera_de_repo_no_hay_worktree() {
        let dir = std::env::temp_dir().join(format!("galgo-wt-sin-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base");
        assert!(crear_worktree(&dir.to_string_lossy(), "tarea", &base).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ver cambios, fusionar sin --yes (no hace nada), fusionar con --yes: el archivo llega al repo principal y el
    /// worktree y la rama se borran.
    #[test]
    fn aceptar_fusiona_y_limpia() {
        let repo = repo_de_prueba("aceptar");
        let base = repo.join("..").join(format!("galgo-wt-aceptar-base-{}", std::process::id()));
        let d = repo.to_string_lossy().into_owned();
        let (path, _) = crear_worktree(&d, "tarea/dos", &base).unwrap();
        let p = path.to_string_lossy().into_owned();
        escribir(&path, "nuevo.txt");
        let sin_confirmar = resumen_de_cambios(&p, Some(15)).unwrap();
        assert!(sin_confirmar.contains("sin confirmar"), "{sin_confirmar}");
        git_run(&p, &["add", "-A"]).unwrap();
        git_run(&p, &["commit", "-q", "-m", "tarea"]).unwrap();
        let diff = worktree_accion(&p, WorktreeAction::Diff, false, false).unwrap();
        assert!(diff["texto"].as_str().unwrap().contains("nuevo.txt"), "{diff}");
        assert!(worktree_accion(&p, WorktreeAction::Accept, false, false).is_err(), "sin --yes no se fusiona");
        assert!(worktree_accion(&p, WorktreeAction::Accept, true, true).is_err(), "con el agente trabajando no se fusiona");
        worktree_accion(&p, WorktreeAction::Accept, true, false).unwrap();
        assert!(repo.join("nuevo.txt").exists(), "el archivo llega al repo principal");
        assert!(!path.exists(), "el worktree se borra");
        assert!(git_run(&d, &["branch", "--list", "tarea/dos"]).unwrap().is_empty(), "la rama se borra");
        assert!(tarea_de(&p).is_err(), "ya no es un worktree de tarea");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Descartar sin --yes no toca nada; con --yes borra worktree y rama aunque no estén fusionadas.
    #[test]
    fn descartar_exige_confirmacion() {
        let repo = repo_de_prueba("descartar");
        let base = repo.join("..").join(format!("galgo-wt-descartar-base-{}", std::process::id()));
        let d = repo.to_string_lossy().into_owned();
        let (path, _) = crear_worktree(&d, "tarea/tres", &base).unwrap();
        let p = path.to_string_lossy().into_owned();
        escribir(&path, "descartable.txt");
        assert!(worktree_accion(&p, WorktreeAction::Discard, false, false).is_err());
        assert!(path.exists(), "sin --yes el worktree sigue");
        worktree_accion(&p, WorktreeAction::Discard, true, false).unwrap();
        assert!(!path.exists() && !repo.join("descartable.txt").exists(), "descartado sin tocar el principal");
        assert!(git_run(&d, &["branch", "--list", "tarea/tres"]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// En el repo principal (no es un worktree de tarea) no hay acción posible.
    #[test]
    fn repo_principal_no_es_tarea() {
        let repo = repo_de_prueba("principal");
        let d = repo.to_string_lossy().into_owned();
        assert!(worktree_accion(&d, WorktreeAction::Diff, false, false).is_err());
        assert!(worktree_accion(&d, WorktreeAction::Discard, true, false).is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }
}

#[cfg(test)]
mod agente_tests {
    use super::*;
    use super::tests_fantasmas::{daemon_con, fantasma};

    /// Divulgación progresiva: un comando de agente conocido o un panel con hooks es un agente real; un shell no.
    #[test]
    fn agente_real_por_comando_o_por_hook() {
        assert!(is_agent_cmd(&["/usr/local/bin/claude".to_string(), "--model".into()]));
        assert!(!is_agent_cmd(&["/bin/zsh".to_string()]));
        assert!(!is_agent_cmd(&[]));
        let mut st = State { epoch: 3, ..State::default() };
        let mut p = fantasma(1, "shell", None);
        p.ghost = false;
        p.cmd = vec!["/bin/zsh".into()];
        st.panes.insert(1, p);
        let d = daemon_con(st);
        let antes = { let st = d.state.lock().unwrap(); st.info(&st.panes[&1], 30).agent };
        assert!(!antes, "un shell sin hooks no es agente");
        d.state.lock().unwrap().panes.get_mut(&1).unwrap().hooked = true;
        let despues = { let st = d.state.lock().unwrap(); st.info(&st.panes[&1], 30).agent };
        assert!(despues, "con hooks, el panel es un agente real");
    }
}

#[cfg(test)]
mod watch_tests {
    use super::*;
    use super::tests_fantasmas::{daemon_con, fantasma};
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncBufReadExt, BufReader};

    /// Siguiente línea de la suscripción, con tiempo límite (si no llega, la prueba falla en vez de colgarse).
    async fn siguiente(c: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>) -> String {
        tokio::time::timeout(Duration::from_secs(3), c.next_line()).await.unwrap().unwrap().unwrap()
    }

    /// La suscripción empuja: un aviso al abrir (estado actual), uno por cada cambio de pantalla y un `closed` al
    /// cerrarse el panel. El cliente no sondea: solo lee lo que llega.
    #[test]
    fn suscripcion_empuja_cambios_y_cierre() {
        let mut st = State { epoch: 3, ..State::default() };
        let mut p = fantasma(1, "shell", None);
        p.ghost = false;
        p.exited = false;
        st.panes.insert(1, p);
        let d = daemon_con(st);
        let (gen, changed) = {
            let st = d.state.lock().unwrap();
            (Arc::clone(&st.panes[&1].gen), Arc::clone(&st.panes[&1].changed))
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (cliente, servidor) = tokio::io::duplex(4096);
            let (r, w) = tokio::io::split(servidor);
            let d2 = Arc::clone(&d);
            let tarea = tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                let mut w = w;
                d2.watch("1".into(), &mut lines, &mut w).await;
            });
            let mut c = BufReader::new(cliente).lines();
            assert!(siguiente(&mut c).await.contains("watching"));
            assert!(siguiente(&mut c).await.contains("\"changed\""), "el estado actual se avisa al abrir");
            gen.fetch_add(1, Ordering::SeqCst);
            changed.notify_waiters();
            let linea = siguiente(&mut c).await;
            assert!(linea.contains("\"changed\""), "un cambio de pantalla se avisa: {linea}");
            d.state.lock().unwrap().panes.get_mut(&1).unwrap().exited = true;
            changed.notify_waiters();
            assert!(siguiente(&mut c).await.contains("closed"), "el cierre del panel se avisa");
            tokio::time::timeout(Duration::from_secs(3), tarea).await.unwrap().unwrap();
        });
    }
}

#[cfg(test)]
mod orquestador_tests {
    use super::tests_fantasmas::{daemon_con, fantasma};
    use super::*;

    /// Panel vivo con su entrada conectada: `rx` recibe lo que el daemon escribiría en el PTY.
    fn vivo(st: &mut State, id: u32, name: &str, hooked: bool, status: Status) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let mut p = fantasma(id, name, None);
        p.ghost = false;
        p.exited = false;
        p.hooked = hooked;
        p.status = status;
        // Un agente con hooks ya terminó su turno (A2): puede recibir tareas sin esperar la gracia.
        p.listo_desde = hooked.then(|| Instant::now() - Duration::from_secs(60));
        // Pantalla vacía: el prompt está libre (lo mantendría el hilo lector en un panel real).
        p.prompt_libre.store(true, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        p.input = tx;
        st.panes.insert(id, p);
        rx
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// La tarea se entrega solo a un agente con hooks que está libre: pegada entre marcas de bracketed paste y con un
    /// único Enter al final. Sin hooks, trabajando o bloqueado, queda pendiente.
    #[test]
    fn la_entrega_exige_hooks_y_estar_libre() {
        let mut st = State { epoch: 3, ..State::default() };
        let _jefe = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let codex_rx = vivo(&mut st, 2, "codex", false, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        let info = r.block_on(d.handle(Some(1), Request::Delegate { to: "2".into(), task: "crea x.txt".into(), context: None })).unwrap();
        assert_eq!(info["status"], "pendiente", "sin hooks no se entrega");
        assert!(codex_rx.try_recv().is_err());
        for (hooked, estado) in [(true, Status::Working), (true, Status::Blocked)] {
            let mut st = d.state.lock().unwrap();
            st.panes.get_mut(&2).unwrap().hooked = hooked;
            st.panes.get_mut(&2).unwrap().status = estado;
            st.deliver(2);
            assert_eq!(st.tasks[0].status, TaskStatus::Pendiente, "{estado:?} no recibe tareas");
        }
        {
            let mut st = d.state.lock().unwrap();
            st.panes.get_mut(&2).unwrap().status = Status::Idle;
            // Un agente con hooks que terminó su turno (A2) y lleva un rato sin teclear.
            st.panes.get_mut(&2).unwrap().listo_desde = Some(Instant::now() - Duration::from_secs(60));
            st.deliver(2);
            assert_eq!(st.tasks[0].status, TaskStatus::Entregada);
        }
        let bytes = codex_rx.try_recv().expect("se escribió en el PTY");
        let texto = String::from_utf8(bytes).unwrap();
        assert!(texto.starts_with("\x1b[200~[galgo · tarea #1 de jefe]"), "{texto}");
        assert!(texto.contains("no es una orden del usuario"));
        assert!(texto.ends_with("\x1b[201~\r"), "un solo Enter, al final, tras el pegado");
        assert_eq!(texto.matches('\r').count(), 1);
    }

    /// A2: un hook sin panel en la cadena de procesos se rechaza; el de un panel, no.
    #[test]
    fn hook_sin_panel_se_rechaza() {
        let mut st = State { epoch: 3, ..State::default() };
        let _x = vivo(&mut st, 1, "x", false, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        let hook = || Request::Hook { pane: "1".into(), event: HookEvent::Working, summary: None, source: None };
        assert!(r.block_on(d.handle(None, hook())).is_err(), "sin panel no hay hook");
        assert!(r.block_on(d.handle(Some(1), hook())).is_ok());
    }

    /// A2: la búsqueda de panel sube por los ancestros del proceso que conecta (un proceso real de esta prueba).
    #[test]
    fn el_panel_se_identifica_por_ascendencia() {
        let padre = unsafe { libc::getppid() } as u32;
        let mut st = State { epoch: 3, ..State::default() };
        let _x = vivo(&mut st, 7, "x", false, Status::Idle);
        st.panes.get_mut(&7).unwrap().pid = Some(padre);
        assert_eq!(st.panel_de_ascendencia(std::process::id() as i32), Some(7), "el padre del proceso es el panel");
        st.panes.get_mut(&7).unwrap().pid = Some(999_999);
        assert_eq!(st.panel_de_ascendencia(std::process::id() as i32), None, "ningún ancestro es un panel");
    }

    /// A2: el prompt libre exige cursor visible y un prompt con su carácter; una línea vacía no basta.
    #[test]
    fn prompt_libre_exige_cursor_visible_y_prompt() {
        use std::num::NonZeroU16;
        let mut p = vt100::Parser::new(NonZeroU16::new(4).unwrap(), NonZeroU16::new(40).unwrap(), 0);
        p.process(b"\x1b[?25l\x1b[1;1H> ");
        assert!(!prompt_libre_de(p.screen()), "cursor oculto: es un diálogo");
        p.process(b"\x1b[?25h");
        assert!(prompt_libre_de(p.screen()), "cursor visible tras '> '");
        p.process(b"\x1b[2K\x1b[1;1H");
        assert!(!prompt_libre_de(p.screen()), "línea vacía: no es prompt");
        p.process(b"\x1b[1;1H\xe2\x94\x82 > \xe2\x94\x82");
        assert!(prompt_libre_de(p.screen()), "prompt dentro de borde de caja");
        p.process(b"\x1b[2K\x1b[1;1Hescribiendo algo");
        assert!(!prompt_libre_de(p.screen()), "texto a medias");
    }

    /// A3 (revisión 17): lo que un panel puede pedir. Sin panel no hay restricción (la CLI del usuario); un panel actúa
    /// sobre sí mismo en lo suyo y sobre otros solo dentro de su workspace; un agente orquestado no actúa sobre otros;
    /// ningún panel lanza paneles orquestados ni detiene el daemon.
    #[test]
    fn autorizacion_de_paneles() {
        let mut st = State { epoch: 3, ..State::default() };
        let _p1 = vivo(&mut st, 1, "a1", true, Status::Idle);
        let _p2 = vivo(&mut st, 2, "a2", true, Status::Idle);
        let _p3 = vivo(&mut st, 3, "b1", true, Status::Idle);
        let _p4 = vivo(&mut st, 4, "orq", true, Status::Idle);
        for id in [1, 2, 4] {
            st.panes.get_mut(&id).unwrap().workspace = Some("uno".into());
        }
        st.panes.get_mut(&3).unwrap().workspace = Some("otro".into());
        st.panes.get_mut(&4).unwrap().orquestador = true;
        let d = daemon_con(st);
        let r = rt();
        let envia = |pane: &str| Request::Send { pane: pane.into(), text: "hola".into(), enter: false };
        assert!(r.block_on(d.handle(Some(1), Request::Inbox { pane: "2".into() })).is_err(), "el buzón de otro no se lee");
        assert!(r.block_on(d.handle(Some(1), Request::Inbox { pane: "1".into() })).is_ok(), "el propio sí");
        assert!(r.block_on(d.handle(Some(1), envia("3"))).is_err(), "otro workspace: no se escribe");
        assert!(r.block_on(d.handle(Some(1), envia("2"))).is_ok(), "mismo workspace: sí");
        assert!(r.block_on(d.handle(Some(4), envia("2"))).is_err(), "un agente orquestado no escribe en otros");
        assert!(r.block_on(d.handle(Some(4), envia("4"))).is_ok(), "ni a sí mismo está prohibido");
        let lanza = Request::Spawn {
            name: "nuevo".into(), cmd: vec!["bash".into()], cwd: None, manager: None, inject: false, rows: 24, cols: 80,
            workspace: None, worktree: None, orquestador: true,
        };
        assert!(r.block_on(d.handle(Some(4), lanza)).is_err(), "un agente orquestado no lanza paneles");
        assert!(r.block_on(d.handle(Some(1), Request::Shutdown)).is_err(), "un panel no detiene el daemon");
        assert!(r.block_on(d.handle(None, envia("3"))).is_ok(), "sin panel (el usuario) no hay restricción");
    }

    /// M1: un agente que avisa `idle` (su espera de orden, o el arranque de Codex) queda listo para su primera tarea
    /// tras la gracia. Blocked no cuenta: ahí hay un diálogo.
    #[test]
    fn idle_deja_listo_para_la_primera_tarea() {
        let mut st = State { epoch: 3, ..State::default() };
        let _x = vivo(&mut st, 1, "x", false, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        r.block_on(d.handle(Some(1), Request::Hook { pane: "1".into(), event: HookEvent::Idle, summary: None, source: None })).unwrap();
        assert!(d.state.lock().unwrap().panes[&1].listo_desde.is_some(), "idle es fin de turno");
        let mut st = d.state.lock().unwrap();
        st.panes.get_mut(&1).unwrap().status = Status::Blocked;
        st.panes.get_mut(&1).unwrap().listo_desde = None;
        drop(st);
        r.block_on(d.handle(Some(1), Request::Hook { pane: "1".into(), event: HookEvent::Idle, summary: None, source: None })).unwrap();
        assert!(d.state.lock().unwrap().panes[&1].listo_desde.is_none(), "un bloqueo no se da por listo");
    }

    /// M4: no se devuelve una tarea a quien te la delegó; y una orden humana devuelve la profundidad a cero.
    #[test]
    fn ciclos_y_profundidad_con_orden_humana() {
        let mut st = State { epoch: 3, ..State::default() };
        let _j = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let _w = vivo(&mut st, 2, "worker", true, Status::Idle);
        st.tasks.push(tarea_abierta(1, 1, 2, None, unix_ahora()));
        assert!(st.check_delegation(2, 1, None).is_err(), "el worker no devuelve la tarea a su jefe");
        st.panes.get_mut(&2).unwrap().profundidad_recibida = 2;
        let d = daemon_con(st);
        let r = rt();
        r.block_on(d.handle(None, Request::Send { pane: "2".into(), text: "hola".into(), enter: true })).unwrap();
        assert_eq!(d.state.lock().unwrap().panes[&2].profundidad_recibida, 0, "la orden humana empieza un trabajo nuevo");
    }

    /// B1: una instalación por npm se reconoce por su línea de órdenes; un node cualquiera no.
    #[test]
    fn el_agente_de_npm_se_reconoce_por_sus_argumentos() {
        assert!(es_agente_args("node /usr/local/lib/node_modules/@openai/codex/bin/codex.js --model x"));
        assert!(es_agente_args("node /opt/claude/cli.js"));
        assert!(!es_agente_args("node server.js --port 3000"));
    }

    /// Revisión 17, A1: `Resize` (pantalla → estado) y `tick` (que entrega) no deben interbloquearse. Con una tarea
    /// pendiente y un agente listo, el `tick` llega a mirar el prompt; antes tomaba la pantalla con el estado tomado.
    #[test]
    fn resize_y_tick_no_se_bloquean() {
        let mut st = State { epoch: 3, ..State::default() };
        let _a = vivo(&mut st, 1, "a", true, Status::Idle);
        let _b = vivo(&mut st, 2, "b", true, Status::Idle);
        st.tasks.push(tarea_abierta(1, 1, 2, None, unix_ahora()));
        st.tasks[0].status = TaskStatus::Pendiente;
        let d = std::sync::Arc::new(daemon_con(st));
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..3 {
            let (d, tx) = (std::sync::Arc::clone(&d), tx.clone());
            std::thread::spawn(move || {
                let r = rt();
                for i in 0..5000u16 {
                    let _ = r.block_on(d.handle(None, Request::Resize { pane: "2".into(), rows: 20 + i % 5, cols: 80 }));
                }
                let _ = tx.send(());
            });
        }
        let (d2, tx2) = (std::sync::Arc::clone(&d), tx.clone());
        std::thread::spawn(move || {
            for _ in 0..5000 {
                // Cada vuelta deja al agente libre y con una tarea nueva: así el tick llega siempre a mirar el prompt.
                {
                    let mut st = d2.state.lock().unwrap();
                    let ahora = unix_ahora();
                    if let Some(p) = st.panes.get_mut(&2) {
                        p.status = Status::Idle;
                        p.listo_desde = Some(Instant::now() - Duration::from_secs(60));
                    }
                    st.tasks.push(tarea_abierta(9, 1, 2, None, ahora));
                    st.tasks.last_mut().unwrap().status = TaskStatus::Pendiente;
                }
                d2.tick();
            }
            let _ = tx2.send(());
        });
        for _ in 0..4 {
            rx.recv_timeout(std::time::Duration::from_secs(20)).expect("un hilo se quedó esperando un candado: interbloqueo");
        }
    }

    /// A1: solo son agentes los procesos de ruta de claude o codex, o el propio proceso de un panel lanzado como agente.
    #[test]
    fn el_agente_se_reconoce_por_su_ruta() {
        assert!(es_agente_ruta("/Users/x/.local/share/claude/versions/2.0.50"));
        assert!(es_agente_ruta("/opt/homebrew/bin/codex"));
        assert!(!es_agente_ruta("/bin/zsh"));
        assert!(!es_agente_ruta("/Users/x/claude-notes/zsh"), "un componente que solo empieza por claude no cuenta");
    }

    /// A2: un aviso de espera (idle_prompt) no saca a un agente de Blocked; el diálogo pendiente sigue siendo suyo.
    #[test]
    fn idle_no_saca_de_blocked() {
        let mut st = State { epoch: 3, ..State::default() };
        let _x = vivo(&mut st, 1, "x", true, Status::Blocked);
        let d = daemon_con(st);
        let r = rt();
        r.block_on(d.handle(Some(1), Request::Hook { pane: "1".into(), event: HookEvent::Idle, summary: None, source: None })).unwrap();
        assert_eq!(d.state.lock().unwrap().panes[&1].status, Status::Blocked);
    }

    /// A2: no se entrega justo al terminar el turno (gracia), ni con texto a medias en el prompt.
    #[test]
    fn la_entrega_espera_la_gracia_y_el_prompt_vacio() {
        let mut st = State { epoch: 3, ..State::default() };
        let _jefe = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let rx = vivo(&mut st, 2, "codex", true, Status::Idle);
        st.tasks.push(Task {
            id: 1, from: 1, to: 2, from_name: "jefe".into(), to_name: "codex".into(), workspace: None, text: "t".into(), context: None,
            status: TaskStatus::Pendiente, summary: None, files: Vec::new(), depth: 1, created_unix: 0,
        });
        st.panes.get_mut(&2).unwrap().listo_desde = Some(Instant::now());
        st.deliver(2);
        assert_eq!(st.tasks[0].status, TaskStatus::Pendiente, "gracia: acaba de terminar");
        st.panes.get_mut(&2).unwrap().listo_desde = Some(Instant::now() - Duration::from_secs(60));
        st.panes.get_mut(&2).unwrap().ultima_entrada = Some(Instant::now());
        st.deliver(2);
        assert_eq!(st.tasks[0].status, TaskStatus::Pendiente, "tecleó hace poco");
        st.panes.get_mut(&2).unwrap().ultima_entrada = None;
        st.panes.get_mut(&2).unwrap().screen.lock().unwrap().process(b"\x1b[1;1Hborrador a medias");
        { let p = st.panes.get(&2).unwrap(); let sc = p.screen.lock().unwrap(); p.prompt_libre.store(prompt_libre_de(sc.screen()), Ordering::SeqCst); }
        st.deliver(2);
        assert_eq!(st.tasks[0].status, TaskStatus::Pendiente, "hay texto a medias en el prompt");
        assert!(rx.try_recv().is_err());
        st.panes.get_mut(&2).unwrap().screen.lock().unwrap().process(b"\x1b[2K\x1b[1;1H> ");
        { let p = st.panes.get(&2).unwrap(); let sc = p.screen.lock().unwrap(); p.prompt_libre.store(prompt_libre_de(sc.screen()), Ordering::SeqCst); }
        st.deliver(2);
        assert_eq!(st.tasks[0].status, TaskStatus::Entregada);
    }

    fn tarea_abierta(id: u64, from: u32, to: u32, ws: Option<&str>, created_unix: u64) -> Task {
        Task {
            id, from, to, from_name: format!("p{from}"), to_name: format!("p{to}"), workspace: ws.map(String::from), text: "t".into(),
            context: None, status: TaskStatus::Entregada, summary: None, files: Vec::new(), depth: 1, created_unix,
        }
    }

    /// M2: la profundidad es la de la última tarea recibida; cerrarla no la reinicia, y a profundidad 3 no se delega más.
    #[test]
    fn la_profundidad_no_se_reinicia_al_cerrar() {
        let mut st = State { epoch: 3, ..State::default() };
        for id in 1..=4 {
            let _ = vivo(&mut st, id, &format!("p{id}"), true, Status::Idle);
        }
        assert_eq!(st.check_delegation(1, 2, None).unwrap(), 1, "un manager delega a 1");
        st.panes.get_mut(&2).unwrap().profundidad_recibida = 1;
        assert_eq!(st.check_delegation(2, 3, None).unwrap(), 2);
        st.tasks.push(Task { status: TaskStatus::Lista, ..tarea_abierta(1, 2, 3, None, 0) });
        assert_eq!(st.check_delegation(2, 3, None).unwrap(), 2, "cerrar la tarea no baja la profundidad");
        st.panes.get_mut(&3).unwrap().profundidad_recibida = 3;
        assert!(st.check_delegation(3, 4, None).is_err(), "a profundidad 3 no se delega más");
    }

    /// M2: los paneles sin workspace comparten un cupo de tareas abiertas en lugar de no tener límite.
    #[test]
    fn sin_workspace_tambien_hay_cupo() {
        let mut st = State { epoch: 3, ..State::default() };
        for id in 1..=12 {
            let _ = vivo(&mut st, id, &format!("p{id}"), true, Status::Idle);
        }
        for i in 0..MAX_OPEN_PER_WORKSPACE as u64 {
            let from = (i % 4) as u32 + 1;
            st.tasks.push(tarea_abierta(i + 1, from, 10, None, 0));
        }
        assert!(st.check_delegation(5, 11, None).is_err(), "el cupo de los paneles sin workspace se llena");
    }

    /// M2: tope por hora por remitente, aunque cada par y cada workspace estén dentro de sus cupos.
    #[test]
    fn tope_de_delegaciones_por_hora() {
        let mut st = State { epoch: 3, ..State::default() };
        for id in 1..=2 {
            let _ = vivo(&mut st, id, &format!("p{id}"), true, Status::Idle);
        }
        let ahora = unix_ahora();
        st.delegaciones = (0..MAX_DELEGATIONS_PER_HOUR).map(|_| (1, ahora)).collect();
        assert!(st.check_delegation(1, 2, None).is_err(), "ya delegó el máximo esta hora");
        st.delegaciones = (0..MAX_DELEGATIONS_PER_HOUR).map(|_| (1, ahora - 7200)).collect();
        assert!(st.check_delegation(1, 2, None).is_ok(), "las de hace más de una hora no cuentan");
    }

    /// M3: tras reiniciar, una tarea abierta no tiene destino vivo: falla, y el remitente recibe el aviso.
    #[test]
    fn tras_reiniciar_las_tareas_abiertas_fallan_y_avisan() {
        let mut st = State { epoch: 3, ..State::default() };
        let _j = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let _w = vivo(&mut st, 2, "worker", true, Status::Idle);
        st.tasks.push(tarea_abierta(1, 1, 2, None, unix_ahora()));
        st.fallar_tareas_sin_destino();
        assert_eq!(st.tasks[0].status, TaskStatus::Fallida);
        assert!(st.panes[&1].inbox.iter().any(|m| m.kind == "failed"), "el remitente lo sabe");
    }

    /// M3: las abiertas de más de un día caducan, y las cerradas antiguas se podan.
    #[test]
    fn caducidad_y_poda_de_tareas() {
        let mut st = State { epoch: 3, ..State::default() };
        let _j = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let _w = vivo(&mut st, 2, "worker", true, Status::Idle);
        let ahora = unix_ahora();
        st.tasks.push(tarea_abierta(1, 1, 2, None, ahora - 2 * CADUCIDAD_TAREA_SECS));
        st.tasks.push(Task { status: TaskStatus::Lista, ..tarea_abierta(2, 1, 2, None, ahora - 30 * 24 * 3600) });
        st.tasks.push(Task { status: TaskStatus::Lista, ..tarea_abierta(3, 1, 2, None, ahora) });
        st.podar_tareas(ahora);
        assert_eq!(st.tasks.iter().find(|t| t.id == 1).unwrap().status, TaskStatus::Fallida, "caducada");
        assert!(st.tasks.iter().all(|t| t.id != 2), "la cerrada de hace un mes se poda");
        assert!(st.tasks.iter().any(|t| t.id == 3), "la reciente se conserva");
    }

    /// M4: etiquetas Unicode, uso privado y formato invisible no entran en tareas, contexto ni resumen.
    #[test]
    fn texto_invisible_se_rechaza() {
        assert!(check_texto_de_tarea("la tarea", "crea ñandú.txt", MAX_TASK_TEXT).is_ok());
        for c in ['\u{E0001}', '\u{E0041}', '\u{E000}', '\u{202E}', '\u{200B}', '\u{0605}'] {
            assert!(check_texto_de_tarea("la tarea", &format!("ok{c}"), MAX_TASK_TEXT).is_err(), "U+{:04X}", c as u32);
        }
    }

    /// Un shell normal no vuelve como fantasma tras un reinicio: el guardado lo lleva (sigue vivo), pero la restauración
    /// solo crea fantasmas de paneles con trabajo pendiente. Su manager, si lo tiene, recibe el aviso.
    #[test]
    fn el_shell_sin_trabajo_no_reaparece_tras_reiniciar() {
        let mut st = State { epoch: 3, ..State::default() };
        let _shell = vivo(&mut st, 1, "shell", false, Status::Idle);
        let _j = vivo(&mut st, 2, "jefe", true, Status::Idle);
        st.tasks.push(tarea_abierta(1, 2, 3, None, unix_ahora()));
        let guardado = crate::persist::snapshot(&st);
        let mut nuevo = State { epoch: 4, ..State::default() };
        crate::persist::restore(&mut nuevo, guardado);
        let nombres: Vec<&str> = nuevo.panes.values().map(|p| p.name.as_str()).collect();
        assert!(!nombres.contains(&"shell"), "un shell sin trabajo no reaparece: {nombres:?}");
        assert!(nombres.contains(&"jefe"), "quien tiene una tarea abierta sí vuelve: {nombres:?}");
    }

    /// A3: el remitente lo pone el socket, no la solicitud. Sin panel no se delega ni se cierra nada; a otro
    /// workspace no se delega; y un panel no emite hooks por otro.
    #[test]
    fn el_remitente_sale_del_socket_y_se_acota_al_workspace() {
        let mut st = State { epoch: 3, ..State::default() };
        let _a = vivo(&mut st, 1, "a", true, Status::Idle);
        let _b = vivo(&mut st, 2, "b", true, Status::Idle);
        let _c = vivo(&mut st, 3, "c", true, Status::Idle);
        st.panes.get_mut(&1).unwrap().workspace = Some("uno".into());
        st.panes.get_mut(&2).unwrap().workspace = Some("uno".into());
        st.panes.get_mut(&3).unwrap().workspace = Some("otro".into());
        let d = daemon_con(st);
        let r = rt();
        let sin_panel = r.block_on(d.handle(None, Request::Delegate { to: "2".into(), task: "t".into(), context: None }));
        assert!(sin_panel.is_err(), "una llamada que no viene de un panel no delega");
        assert!(r.block_on(d.handle(Some(1), Request::Delegate { to: "3".into(), task: "t".into(), context: None })).is_err(), "otro workspace");
        assert!(r.block_on(d.handle(Some(1), Request::Delegate { to: "2".into(), task: "t".into(), context: None })).is_ok());
        assert!(r.block_on(d.handle(Some(3), Request::Tasks { pane: None })).unwrap().as_array().unwrap().is_empty(), "el workspace otro no ve las tareas de uno");
        assert_eq!(r.block_on(d.handle(Some(2), Request::Tasks { pane: None })).unwrap().as_array().unwrap().len(), 1);
        let hook = Request::Hook { pane: "2".into(), event: HookEvent::Working, summary: None, source: None };
        assert!(r.block_on(d.handle(Some(1), hook)).is_err(), "un panel no emite hooks por otro");
    }

    /// Frenos: como mucho 3 tareas abiertas por par, profundidad máxima 3, y 8 abiertas por workspace.
    #[test]
    fn frenos_de_delegacion() {
        let mut st = State { epoch: 3, ..State::default() };
        let _a = vivo(&mut st, 1, "a", true, Status::Idle);
        let _b = vivo(&mut st, 2, "b", true, Status::Idle);
        let _c = vivo(&mut st, 3, "c", true, Status::Idle);
        let _e = vivo(&mut st, 4, "e", true, Status::Idle);
        let _f = vivo(&mut st, 5, "f", true, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        let mut ok = 0;
        for _ in 0..4 {
            if r.block_on(d.handle(Some(1), Request::Delegate { to: "2".into(), task: "t".into(), context: None })).is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 3, "el cuarto par entre los mismos dos agentes se rechaza");
        // Cadena 1→2→3→4 (profundidad 3) y el siguiente salto, 4→5, pasa de la profundidad máxima.
        assert!(r.block_on(d.handle(Some(2), Request::Delegate { to: "3".into(), task: "t".into(), context: None })).is_ok());
        assert!(r.block_on(d.handle(Some(3), Request::Delegate { to: "4".into(), task: "t".into(), context: None })).is_ok());
        let err = r.block_on(d.handle(Some(4), Request::Delegate { to: "5".into(), task: "t".into(), context: None })).unwrap_err();
        assert!(err.to_string().contains("profundidad máxima"), "{err}");
        // Workspace: 8 tareas abiertas y ya no entra ninguna más.
        let mut st = d.state.lock().unwrap();
        st.tasks.clear();
        for i in 0..8u64 {
            st.tasks.push(Task { id: i + 1, from: 1, to: 2, from_name: "a".into(), to_name: "b".into(), workspace: Some("ws".into()), text: "t".into(), context: None, status: TaskStatus::Pendiente, summary: None, files: vec![], depth: 1, created_unix: 0 });
        }
        let e = st.check_delegation(3, 4, Some("ws")).unwrap_err();
        assert!(e.to_string().contains("workspace"), "{e}");
        assert!(st.check_delegation(3, 4, Some("otro")).is_ok(), "otro workspace tiene su propio cupo");
    }

    /// El resultado vuelve al remitente como mensaje de su buzón; solo el destinatario puede cerrar la tarea.
    #[test]
    fn el_resultado_vuelve_al_remitente() {
        let mut st = State { epoch: 3, ..State::default() };
        let _jefe = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let _codex = vivo(&mut st, 2, "codex", true, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        let info = r.block_on(d.handle(Some(1), Request::Delegate { to: "2".into(), task: "crea x.txt".into(), context: None })).unwrap();
        let id = info["id"].as_u64().unwrap();
        assert!(r.block_on(d.handle(Some(1), Request::TaskDone { id, summary: "no soy yo".into(), ok: true })).is_err(), "el remitente no cierra la tarea");
        r.block_on(d.handle(Some(2), Request::TaskDone { id, summary: "creado".into(), ok: true })).unwrap();
        let st = d.state.lock().unwrap();
        assert_eq!(st.tasks[0].status, TaskStatus::Lista);
        let buzon = &st.panes[&1].inbox;
        assert!(buzon.iter().any(|m| m.kind == "done" && m.summary.contains("creado")), "el jefe recibe el resultado");
    }

    /// Si el destinatario termina antes de cerrar la tarea, la tarea falla y el delegador se entera.
    #[test]
    fn destinatario_que_termina_falla_la_tarea() {
        let mut st = State { epoch: 3, ..State::default() };
        let _jefe = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let _codex = vivo(&mut st, 2, "codex", true, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        r.block_on(d.handle(Some(1), Request::Delegate { to: "2".into(), task: "t".into(), context: None })).unwrap();
        let mut st = d.state.lock().unwrap();
        st.fail_tasks_of(2);
        assert_eq!(st.tasks[0].status, TaskStatus::Fallida);
        assert!(st.panes[&1].inbox.iter().any(|m| m.kind == "failed"), "el jefe se entera");
    }

    /// Las tareas sobreviven al reinicio con su estado y su id.
    #[test]
    fn tareas_sobreviven_al_reinicio() {
        let mut st = State { epoch: 3, ..State::default() };
        let _jefe = vivo(&mut st, 1, "jefe", true, Status::Idle);
        let _codex = vivo(&mut st, 2, "codex", true, Status::Idle);
        let d = daemon_con(st);
        let r = rt();
        r.block_on(d.handle(Some(1), Request::Delegate { to: "2".into(), task: "crea x.txt".into(), context: Some("en la raíz".into()) })).unwrap();
        let guardado = persist::snapshot(&d.state.lock().unwrap());
        let mut nuevo = State { epoch: 4, ..State::default() };
        persist::restore(&mut nuevo, guardado);
        assert_eq!(nuevo.tasks.len(), 1);
        // M3: tras el reinicio no hay agente vivo al que llegue la tarea abierta: falla, y el remitente se entera.
        assert_eq!(nuevo.tasks[0].status, TaskStatus::Fallida);
        assert!(nuevo.panes[&1].inbox.iter().any(|m| m.kind == "failed"), "el jefe recibe el aviso");
        assert_eq!(nuevo.tasks[0].context.as_deref(), Some("en la raíz"));
        assert_eq!(nuevo.next_task, 1);
    }
}
