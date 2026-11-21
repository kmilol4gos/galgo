//! galgod: daemon que posee los PTYs, el estado de pantalla y la entrega de mensajes entre agentes.

use anyhow::{anyhow, bail, Context, Result};
use galgo_proto::{HookEvent, Message, PaneInfo, Request, Response, Status};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
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
    _master: Box<dyn MasterPty + Send>,
    inbox: Vec<InboxMsg>,
    last_summary: Option<String>,
    /// Último motivo de bloqueo avisado al manager; un motivo nuevo se avisa aunque ya estuviera blocked.
    last_blocked: Option<String>,
    waiting_on: Option<(u32, Instant)>,
    /// Ya entregó su resultado en este turno (`galgo done` o el evento de fin de turno).
    /// Solo se limpia con un prompt nuevo o con un envío con Enter.
    reported: bool,
}

#[derive(Default)]
struct State {
    next_pane: u32,
    next_msg: u64,
    panes: BTreeMap<u32, Pane>,
    /// Nombres que un spawn en curso ya reservó y aún no tienen panel.
    reserved: HashSet<String>,
}

impl State {
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

    fn push(&mut self, to: u32, from: u32, kind: &'static str, summary: String) {
        self.next_msg += 1;
        let id = self.next_msg;
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
        }
    }
}

struct Daemon {
    state: Mutex<State>,
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
    ) -> Result<u32> {
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
        let (id, manager_id, mut reservation) = {
            let mut st = self.state.lock().unwrap();
            if st.panes.values().any(|p| p.name == name) || st.reserved.contains(&name) {
                bail!("ya existe un panel llamado {name}");
            }
            let m = match manager {
                Some(r) => Some(st.resolve(&r)?),
                None => None,
            };
            st.next_pane += 1;
            st.reserved.insert(name.clone());
            let reservation = NameReservation { d: Arc::clone(self), name: Some(name.clone()) };
            (st.next_pane, m, reservation)
        };

        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("no pude abrir el PTY")?;
        let mut builder = CommandBuilder::new(&cmd[0]);
        builder.args(&cmd[1..]);
        if let Some(dir) = cwd {
            builder.cwd(dir);
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
            input,
            killer,
            pid,
            exited: false,
            touched_since_report: false,
            _master: pair.master,
            inbox: Vec::new(),
            last_summary: None,
            last_blocked: None,
            waiting_on: None,
            reported: false,
        };
        {
            let mut st = self.state.lock().unwrap();
            st.reserved.remove(&name);
            st.panes.insert(id, pane);
        }
        reservation.name = None;

        let d = Arc::clone(self);
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => screen.lock().unwrap().process(&buf[..n]),
                }
            }
            // El hijo se recolecta aquí, sin candados: no queda <defunct> y sabemos cómo terminó.
            // Antes de recolectar al hijo, el panel queda marcado como terminado: un kill que llegue
            // ahora ya no le envía señales, así que nunca actúa sobre un pid recolectado.
            d.mark_closed(id);
            let code = child.wait().ok().map(|s| s.exit_code());
            d.on_exit(id, code);
        });
        Ok(id)
    }

    fn mark_closed(&self, id: u32) {
        if let Some(p) = self.state.lock().unwrap().panes.get_mut(&id) {
            p.exited = true;
        }
    }

    /// El proceso del panel terminó. Siempre queda `exited` (conservando el último resumen).
    /// Avisa al manager si no hubo resultado en este turno o si hubo actividad después.
    fn on_exit(&self, id: u32, code: Option<u32>) {
        let manager = {
            let mut st = self.state.lock().unwrap();
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
        for m in p.inbox.iter_mut().filter(|m| !m.acked && !m.is_held()) {
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
        match req {
            Request::Wait { waiter, target, timeout_secs, .. } => {
                self.wait_for(waiter, target, timeout_secs).await
            }
            other => (self.handle(other).await, None),
        }
    }

    async fn handle(self: &Arc<Self>, req: Request) -> Result<Value> {
        match req {
            Request::Spawn { name, cmd, cwd, manager, inject, rows, cols } => {
                let id = self.spawn_pane(name, cmd, cwd, manager, inject, rows, cols)?;
                let st = self.state.lock().unwrap();
                Ok(serde_json::to_value(st.info(&st.panes[&id], self.orphan_secs))?)
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
                    Arc::clone(&st.panes[&st.resolve(&pane)?].screen)
                };
                let text = screen.lock().unwrap().screen().contents();
                Ok(json!({ "text": text.trim_end() }))
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
                self.notify.notify_waiters();
                if let Some(m) = manager {
                    self.try_inject(m);
                }
                Ok(json!({ "delivered_to": manager }))
            }
            Request::Hook { pane, event, summary } => {
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
                            p.status = Status::Working;
                        }
                        HookEvent::Working => {
                            // Una herramienta que termina no cambia un resultado ya entregado.
                            let p = st.panes.get_mut(&id).unwrap();
                            p.last_blocked = None;
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
                self.notify.notify_waiters();
                if let Some(m) = target {
                    self.try_inject(m);
                }
                // Se resuelve en una sentencia aparte: un `if let` mantendría el candado tomado
                // dentro del cuerpo y `try_inject` lo pediría de nuevo (interbloqueo).
                let own = self.state.lock().unwrap().resolve(&pane).ok();
                if let Some(id) = own {
                    self.try_inject(id);
                }
                Ok(json!({}))
            }
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
                    let id = st.resolve(&pane)?;
                    let p = st.panes.get_mut(&id).unwrap();
                    let m = p
                        .inbox
                        .iter_mut()
                        .find(|m| m.id == msg)
                        .ok_or_else(|| anyhow!("mensaje {msg} no encontrado"))?;
                    m.acked = true;
                }
                self.notify.notify_waiters();
                Ok(json!({}))
            }
            Request::Kill { pane } => {
                let (id, name, sid, exited) = {
                    let st = self.state.lock().unwrap();
                    let id = st.resolve(&pane)?;
                    let p = &st.panes[&id];
                    (id, p.name.clone(), p.pid, p.exited)
                };
                let Some(sid) = sid else { bail!("el panel {name} ya terminó") };
                let sid = sid as i32;
                // Miembros de la sesión antes de cualquier señal: solo estos se barren (#13).
                let before = tokio::task::spawn_blocking(move || session_pids(sid)).await?;
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
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = std::fs::remove_file(socket);
                    std::process::exit(0);
                });
                Ok(json!({}))
            }
        }
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
            } else {
                d.release(pane, msg);
            }
        }
        if !written {
            break;
        }
    }
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
    let listener = UnixListener::bind(&socket)?;
    // Quien alcance el socket puede escribir en todos los paneles: solo el dueño.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let daemon = Arc::new(Daemon {
        state: Mutex::new(State::default()),
        notify: Notify::new(),
        socket: socket.clone(),
        orphan_secs: env_u64("GALGO_ORPHAN_SECS", 30),
        ack_secs: env_u64("GALGO_ACK_SECS", 5),
        retry: Duration::from_secs(env_u64("GALGO_RETRY_SECS", 60)),
        write_timeout: Duration::from_secs(env_u64("GALGO_WRITE_TIMEOUT_SECS", 10)),
    });
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
