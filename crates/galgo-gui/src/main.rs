use galgo_proto::{client, PaneInfo, Request, Status};
use iced::{widget::{column, container, row}, Color, Element, Font, Length, Task};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;
use std::time::Duration;

use theme::{BG, MUTED, SIDEBAR, TEXT};

mod attention;
mod chrome;
mod launch;
mod layout;
mod newpane;
mod palette;
mod recents;
mod sidebar;
mod term;
mod theme;
mod usage;

use attention::{Alert, Attention, Badge};
use galgo_proto::{Run, Snapshot};
use galgo_usage::Limits;
use iced::keyboard::{self, key::Named, Key};
use iced::mouse;
use iced::widget::text::Span;
use iced::widget::{rich_text, span};
use iced::{event, font, Event, Point, Subscription};
use newpane::{NewPane, Preset};
use std::sync::{mpsc, OnceLock};

const FONT_SIZE: f32 = 14.0;
const LINE_H: f32 = 18.0;
const CELL_W: f32 = 8.43; // Menlo: 0,602 em
const SIDEBAR_W: f32 = 248.0;

/// Paso de zoom de la fuente del terminal (Cmd+= / Cmd+- / Cmd+0): 0 es el tamaño base. Global porque la geometría
/// (celdas, filas, ratón) se calcula en funciones puras que no reciben la app.
static ZOOM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
const ZOOM_MIN: i32 = -5;
const ZOOM_MAX: i32 = 12;

/// (tamaño de letra, alto de línea, ancho de celda) para un paso de zoom: proporcionales a los de base.
fn metrics_for(zoom: i32) -> (f32, f32, f32) {
    let size = FONT_SIZE + zoom as f32;
    (size, (size * LINE_H / FONT_SIZE).round(), size * CELL_W / FONT_SIZE)
}

fn zoom() -> i32 {
    ZOOM.load(std::sync::atomic::Ordering::Relaxed)
}
fn font_size() -> f32 {
    metrics_for(zoom()).0
}
fn line_h() -> f32 {
    metrics_for(zoom()).1
}
fn cell_w() -> f32 {
    metrics_for(zoom()).2
}
/// Líneas de historial por cada paso de la rueda.
const WHEEL_LINES: i32 = 3;
/// Altura de la barra de pestañas (arriba del área de terminales, con su divisor de 1 px).
const TABS_H: f32 = 36.0;
/// Altura de la barra de estado (abajo, con su divisor de 1 px).
const STATUS_H: f32 = 26.0;
/// Margen interior de cada panel de terminal.
const LEAF_PAD: f32 = 8.0;
/// Origen de la cuadrícula de un panel único (referencia de las pruebas); un split usa el de su rectángulo.
const TERM_ORIGIN: (f32, f32) = (SIDEBAR_W + LEAF_PAD, TABS_H + LEAF_PAD);
/// Primera opción del desplegable de manager del formulario: ningún manager.
const NO_MANAGER: &str = "(ninguno)";

/// (fila, columna) dentro de la pantalla mostrada.
type Cell = (u16, u16);

#[derive(Debug, Clone, Copy, PartialEq, Hash)]
struct Selection {
    anchor: Cell,
    head: Cell,
}

fn terminal_font() -> Font {
    Font::with_name("Menlo")
}

/// Último error de envío que el hilo de `outbox` no pudo entregar. La GUI lo muestra.
fn outbox_error() -> &'static Mutex<Option<String>> {
    static ERR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    ERR.get_or_init(|| Mutex::new(None))
}

/// Fusiona los `Send` consecutivos del mismo panel mientras el anterior no lleve Enter. El orden se
/// conserva: un Enter cierra la fusión y queda como último tramo.
fn coalesce(batch: Vec<Request>) -> Vec<Request> {
    let mut out: Vec<Request> = Vec::with_capacity(batch.len());
    for req in batch {
        let merged = match (out.last_mut(), &req) {
            (
                Some(Request::Send { pane: p0, text: t0, enter: e0 }),
                Request::Send { pane: p1, text: t1, enter: e1 },
            ) if !*e0 && *p0 == *p1 => {
                t0.push_str(t1);
                *e0 = *e1;
                true
            }
            _ => false,
        };
        if !merged {
            out.push(req);
        }
    }
    out
}

/// Todas las peticiones de teclado, pegado y resize pasan por un solo hilo: el orden importa.
/// Lo que esperaba en la cola se envía junto, fusionado cuando es posible.
fn outbox() -> &'static mpsc::Sender<Request> {
    static TX: OnceLock<mpsc::Sender<Request>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Request>();
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }
                for req in coalesce(batch) {
                    if let Err(e) = request(req) {
                        *outbox_error().lock().unwrap() = Some(e);
                    }
                }
            }
        });
        tx
    })
}

/// Texto que se envía al pegar. Con bracketed paste activo se envuelve en `ESC[200~ … ESC[201~` y se
/// quita del texto cualquier `ESC[201~` (cerraría el pegado antes de tiempo). Sin él, se quitan los ESC.
fn paste_bytes(text: &str, bracketed: bool) -> String {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""))
    } else {
        text.replace('\x1b', "")
    }
}

/// Decide si la entrada se envía ya o se guarda tras un pegado pendiente, para no adelantarse a él.
fn route_input(app: &mut App, bytes: String) -> Option<String> {
    if app.paste_pending {
        app.key_queue.push(bytes);
        None
    } else {
        Some(bytes)
    }
}

#[derive(Default)]
struct App {
    panes: Vec<PaneInfo>,
    selected: Option<u32>,
    snap: Option<Snapshot>,
    /// Generación y desplazamiento de `snap`, para pedir solo lo que cambió.
    snap_gen: Option<u64>,
    snap_offset: u16,
    /// Copia de la pantalla tomada al empezar una selección: lo que se resalta y se copia.
    /// Mientras existe, la vista no se actualiza, así la selección no se desplaza.
    sel_snap: Option<Snapshot>,
    error: Option<String>,
    loading: bool,
    listing: bool,
    window: (f32, f32),
    /// Último tamaño (filas, columnas) enviado o confirmado por panel (M3: cada panel del layout tiene el suyo).
    sizes: std::collections::HashMap<u32, (u16, u16)>,
    /// Líneas hacia atrás que se piden al daemon (0 = en vivo).
    scroll: u16,
    /// Fracción de líneas de la rueda que aún no suma un paso completo.
    wheel_acc: f32,
    sel: Option<Selection>,
    dragging: bool,
    pointer: Option<Point>,
    /// Hay un pegado pedido al portapapeles que aún no ha llegado.
    paste_pending: bool,
    /// Teclas que llegaron mientras tanto; se envían después del pegado.
    key_queue: Vec<String>,
    /// Tamaño con el que se pidió el último `Spawn`: es el tamaño del panel que nace.
    spawn_size: Option<(u16, u16)>,
    /// Grupos de workspace plegados.
    collapsed: std::collections::BTreeSet<String>,
    attention: Attention,
    /// Formulario de "Nuevo panel", abierto mientras existe.
    form: Option<NewPane>,
    form_error: Option<String>,
    /// Pide confirmación antes de cerrar el panel seleccionado.
    confirm_close: bool,
    /// Cuotas por agente del daemon, y el último error si no se pudieron leer.
    limits: Vec<Limits>,
    limits_error: Option<String>,
    /// La ventana tiene el foco: si no, las alertas van a notificación de macOS.
    focused: bool,
    /// «Nuevo workspace» abierto, con el texto de búsqueda en recientes.
    new_ws: Option<String>,
    /// Carpetas abiertas recientemente (la más reciente primero) y dónde se guardan.
    recents: Vec<String>,
    recents_path: Option<std::path::PathBuf>,
    /// Lanzador de agentes abierto.
    launcher: bool,
    /// Paleta de comandos abierta: texto de búsqueda y resultado resaltado.
    palette: Option<String>,
    palette_cursor: usize,
    /// Pestañas y splits de cada workspace (solo en la GUI).
    layouts: layout::Layouts,
    /// Split pedido y aún sin panel: (panel con foco, lado a lado).
    pending_split: std::collections::HashMap<String, (u32, bool)>,
    /// Nombres de paneles pedidos y aún sin respuesta (B2): un lanzamiento nuevo no los repite.
    inflight: Vec<String>,
    /// Últimas pantallas de los paneles visibles que no tienen el foco, y su generación.
    others: std::collections::HashMap<u32, Snapshot>,
    others_gen: std::collections::HashMap<u32, u64>,
    others_loading: std::collections::HashSet<u32>,
    others_cursor: usize,
    /// Foco que toma el hermano o la pestaña vecina al cerrar el panel enfocado (M1).
    pending_focus: Option<u32>,
    /// Panel maximizado dentro de su split (⌘⇧Enter): ocupa toda el área y los demás se ocultan.
    maximized: Option<u32>,
    /// Divisor de split que se está arrastrando, y el que está bajo el puntero (camino y orientación).
    drag_divider: Option<layout::Divider>,
    hover_divider: Option<(layout::Path, bool)>,
}

#[derive(Debug, Clone)]
enum Message {
    TickScreen,
    /// Zoom de la letra del terminal: +1 / -1 pasos, o 0 para volver al tamaño base.
    Zoom(i32),
    /// Maximiza el panel enfocado dentro de su split, o lo devuelve al split.
    ToggleMaximize,
    TickList,
    Panes(Result<Vec<PaneInfo>, String>),
    Limits(Result<Vec<Limits>, String>),
    /// Respuesta de una petición de pantalla: id, offset pedido, `since` enviado y el resultado.
    Screen(u32, u16, Option<u64>, Result<ScreenReply, String>),
    Select(u32),
    Event(Event),
    Paste(Option<String>),
    /// Pliega o despliega un grupo de workspace (la clave es el nombre; vacío = sin workspace).
    ToggleGroup(String),
    FormWorkspace(String),
    OpenForm,
    CancelForm,
    FormName(String),
    FormPreset(String),
    FormCustom(String),
    FormCwd(String),
    FormManager(String),
    SubmitForm,
    Spawned(Result<serde_json::Value, String>),
    AskClose,
    CancelClose,
    ConfirmClose,
    Killed(Result<serde_json::Value, String>),
    OpenNewWorkspace,
    CloseNewWorkspace,
    NewWsQuery(String),
    /// Abre la primera carpeta reciente que coincide con la búsqueda (Enter).
    NewWsSubmit,
    PickFolder,
    FolderPicked(Option<String>),
    /// La carpeta elegida tiene una ruta que no es UTF-8: no se abre (B10).
    FolderRejected,
    /// Crea un workspace en esa carpeta: un shell, sin formulario.
    OpenFolder(String),
    WorkspaceOpened(String, Result<serde_json::Value, String>),
    /// Atajos (B): pestaña nueva en el workspace actual, workspace por número, siguiente que necesita atención,
    /// cerrar el panel enfocado, lanzador de agentes.
    NewTab,
    SelectWorkspaceAt(usize),
    NextAttention,
    CloseFocused,
    OpenLauncher,
    CloseLauncher,
    Launch(launch::Agent),
    /// Un panel creado desde un atajo o el lanzador: queda seleccionado.
    PaneLaunched(Result<serde_json::Value, String>),
    /// Un lanzamiento que falló: su nombre queda libre y su split pendiente se descarta (B1, B2).
    LaunchFailed(String, String),
    /// Pestañas y splits (E): cambiar de pestaña, elegir una, dividir el panel enfocado, mover el foco por direcciones.
    NextTab(isize),
    SelectTab(usize),
    SplitFocused(bool),
    FocusDir(layout::Dir),
    /// Pantalla de un panel visible sin el foco.
    OtherScreen(u32, Result<ScreenReply, String>),
    /// Paleta de comandos (C): abrir/cerrar, buscar, mover el resaltado, ejecutar (por índice de resultado).
    OpenPalette,
    ClosePalette,
    PaletteQuery(String),
    PaletteMove(isize),
    PaletteRun(usize),
}

/// Un resultado de la paleta: lo que muestra, lo que busca y lo que hace al elegirlo.
struct PaletteItem {
    label: String,
    detail: String,
    message: Message,
}

/// Todos los resultados posibles: workspaces, paneles y comandos. Se filtran con la búsqueda.
fn palette_items(app: &App) -> Vec<PaletteItem> {
    let mut items = Vec::new();
    for (i, ws) in sidebar::workspaces(&app.panes).into_iter().enumerate() {
        let label = ws.label.clone().unwrap_or_else(|| "sin workspace".into());
        items.push(PaletteItem { label: format!("Workspace · {label}"), detail: format!("{} panel(es)", ws.panes.len()), message: Message::SelectWorkspaceAt(i) });
    }
    for p in &app.panes {
        let detail = format!("{} · {}", p.status.as_str(), p.workspace.as_deref().unwrap_or("sin workspace"));
        items.push(PaletteItem { label: format!("Panel · {}", p.name), detail, message: Message::Select(p.id) });
    }
    let commands: [(&str, &str, Message); 20] = [
        ("Nuevo workspace…", "Cmd+N", Message::OpenNewWorkspace),
        ("Abrir carpeta…", "Cmd+O", Message::PickFolder),
        ("Nuevo terminal en este workspace", "Cmd+T", Message::NewTab),
        ("Lanzar agente…", "Cmd+Shift+L", Message::OpenLauncher),
        ("Cerrar panel", "Cmd+W", Message::CloseFocused),
        ("Siguiente que necesita atención", "Cmd+Shift+U", Message::NextAttention),
        ("Lanzar Claude aquí", "", Message::Launch(launch::Agent::Claude)),
        ("Lanzar Codex aquí", "", Message::Launch(launch::Agent::Codex)),
        ("Lanzar PI aquí", "", Message::Launch(launch::Agent::Pi)),
        ("Nuevo panel con formulario", "", Message::OpenForm),
        ("Dividir a la derecha", "Cmd+D", Message::SplitFocused(true)),
        ("Dividir hacia abajo", "Cmd+Shift+D", Message::SplitFocused(false)),
        ("Pestaña siguiente", "Cmd+]", Message::NextTab(1)),
        ("Pestaña anterior", "Cmd+[", Message::NextTab(-1)),
        ("Lanzar opencode aquí", "", Message::Launch(launch::Agent::OpenCode)),
        ("Lanzar shell aquí", "", Message::Launch(launch::Agent::Shell)),
        ("Aumentar la letra", "Cmd+=", Message::Zoom(1)),
        ("Reducir la letra", "Cmd+-", Message::Zoom(-1)),
        ("Letra de tamaño normal", "Cmd+0", Message::Zoom(0)),
        ("Maximizar o restaurar el panel", "Cmd+Shift+Enter", Message::ToggleMaximize),
    ];
    for (label, keys, message) in commands {
        items.push(PaletteItem { label: label.into(), detail: keys.into(), message });
    }
    items
}

/// Resultados que casan con la búsqueda actual, del mejor al peor.
fn palette_results(app: &App) -> Vec<PaletteItem> {
    let query = app.palette.as_deref().unwrap_or("");
    let items = palette_items(app);
    let texts: Vec<String> = items.iter().map(|i| format!("{} {}", i.label, i.detail)).collect();
    palette::rank(&texts, query).into_iter().map(|i| PaletteItem { label: items[i].label.clone(), detail: items[i].detail.clone(), message: items[i].message.clone() }).collect()
}

/// Atajos de la ventana con Cmd. Lo que no es atajo llega al panel seleccionado.
fn shortcut(key: &Key, modifiers: keyboard::Modifiers) -> Option<Message> {
    let Key::Character(c) = key else { return None };
    let lower = c.as_str().to_lowercase();
    match (lower.as_str(), modifiers.shift()) {
        ("k", false) | ("p", true) => Some(Message::OpenPalette),
        ("n", false) => Some(Message::OpenNewWorkspace),
        ("o", false) => Some(Message::PickFolder),
        ("t", false) => Some(Message::NewTab),
        ("w", false) => Some(Message::CloseFocused),
        ("[", false) => Some(Message::NextTab(-1)),
        ("]", false) => Some(Message::NextTab(1)),
        ("d", false) => Some(Message::SplitFocused(true)),
        ("d", true) => Some(Message::SplitFocused(false)),
        ("l", true) => Some(Message::OpenLauncher),
        ("u", true) => Some(Message::NextAttention),
        ("=", _) | ("+", _) => Some(Message::Zoom(1)),
        ("-", false) => Some(Message::Zoom(-1)),
        ("0", false) => Some(Message::Zoom(0)),
        (d, false) => match d.parse::<usize>() {
            Ok(n @ 1..=9) => Some(Message::SelectWorkspaceAt(n - 1)),
            _ => None,
        },
        _ => None,
    }
}

/// M2: un panel que puede tener trabajo en curso (todo salvo los terminados o caídos) pide confirmación al cerrarse.
fn needs_confirm(p: &PaneInfo) -> bool {
    !matches!(p.status, Status::Done | Status::Failed | Status::Exited)
}

/// M2: con el formulario abierto, Cmd+W cierra el formulario (no el panel de detrás) y Cmd+N sigue valiendo;
/// el resto de atajos no actúa.
fn shortcut_in_state(app: &App, key: &Key, modifiers: keyboard::Modifiers) -> Option<Message> {
    let message = shortcut(key, modifiers)?;
    if app.form.is_none() {
        return Some(message);
    }
    match message {
        Message::CloseFocused => Some(Message::CancelForm),
        Message::OpenNewWorkspace => Some(message),
        _ => None,
    }
}

/// M1: el panel que toma el foco al cerrar `id`: su hermano en la misma pestaña, o si era la única hoja, el foco de
/// la pestaña vecina del mismo workspace. `None` si no hay ninguno.
fn successor(app: &App, id: u32) -> Option<u32> {
    let key = app.layouts.key_of(id)?;
    let ws = app.layouts.by_key.get(&key)?;
    let i = ws.tabs.iter().position(|t| t.root.ids().contains(&id))?;
    if let Some(other) = ws.tabs[i].root.ids().into_iter().find(|x| *x != id) {
        return Some(other);
    }
    let near = if i > 0 { i - 1 } else { i + 1 };
    ws.tabs.get(near).map(|t| t.focus).filter(|x| *x != id)
}

/// Workspace n-ésimo (0 = primero) en el orden de la barra lateral; se selecciona su primer panel.
/// M5: el panel que enfoca Cmd+n es el de la pestaña activa de ese workspace; sin layout, el primero.
fn workspace_at(panes: &[PaneInfo], layouts: &layout::Layouts, n: usize) -> Option<u32> {
    let ws = sidebar::workspaces(panes).into_iter().nth(n)?;
    let key = ws.label.clone().unwrap_or_default();
    let activo = layouts.active_tab(&key).map(|t| t.focus).filter(|f| ws.panes.iter().any(|p| p.id == *f));
    activo.or_else(|| ws.panes.first().map(|p| p.id))
}

/// Siguiente panel, en orden de la lista y dando la vuelta, que necesita atención (insignia visible).
fn next_attention(panes: &[PaneInfo], selected: Option<u32>, unread: impl Fn(u32) -> bool) -> Option<u32> {
    let flagged: Vec<u32> = panes.iter().filter(|p| attention::badge(p, unread(p.id)).is_some()).map(|p| p.id).collect();
    if flagged.is_empty() {
        return None;
    }
    let after = selected.and_then(|s| flagged.iter().position(|&id| id == s)).map_or(0, |i| i + 1);
    flagged.get(after % flagged.len()).copied()
}

/// Lo que heredan los paneles nuevos del panel enfocado: su workspace y su carpeta.
fn focus_context(app: &App) -> (Option<String>, Option<String>) {
    app.selected
        .and_then(|id| app.panes.iter().find(|p| p.id == id))
        .map_or((None, None), |p| (p.workspace.clone(), p.cwd.clone()))
}

/// Petición de un panel nuevo en el contexto del enfocado. Sin carpeta conocida, el directorio de inicio.
fn context_request(app: &App, agent: launch::Agent) -> galgo_proto::Request {
    let (rows, cols) = grid_for(app.window);
    let (workspace, cwd) = focus_context(app);
    let cwd = cwd.unwrap_or_else(home_dir);
    let name = launch::free_name(agent.label(), &names(app));
    launch::spawn_request(&name, agent.command(&login_shell()), &cwd, workspace, rows, cols)
}

/// Crea un panel con la petición dada; al llegar queda seleccionado.
fn spawn_task(app: &mut App, req: galgo_proto::Request) -> Task<Message> {
    let size = grid_for(app.window);
    spawn_task_sized(app, req, size)
}

/// Como `spawn_task`, con el tamaño del panel que nace (un split nace con la mitad del área).
fn spawn_task_sized(app: &mut App, req: galgo_proto::Request, (rows, cols): (u16, u16)) -> Task<Message> {
    app.spawn_size = Some((rows, cols));
    // B2: el nombre pedido se reserva hasta que llegue la respuesta, para que el siguiente lanzamiento no lo repita.
    let name = match &req {
        galgo_proto::Request::Spawn { name, .. } => name.clone(),
        _ => String::new(),
    };
    app.inflight.push(name.clone());
    Task::perform(
        async move { tokio::task::spawn_blocking(move || request(req)).await.map_err(|e| e.to_string()).and_then(|r| r) },
        move |result| match result {
            Ok(data) => Message::PaneLaunched(Ok(data)),
            Err(e) => Message::LaunchFailed(name.clone(), e),
        },
    )
}

/// Qué ocupa el cuerpo de la ventana: bienvenida, «Nuevo workspace», formulario o el terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    Welcome,
    NewWorkspace,
    Launcher,
    Palette,
    Form,
    Screen,
}

fn body_mode(app: &App) -> Body {
    if app.palette.is_some() {
        Body::Palette
    } else if app.new_ws.is_some() {
        Body::NewWorkspace
    } else if app.launcher {
        Body::Launcher
    } else if app.form.is_some() {
        Body::Form
    } else if app.panes.is_empty() {
        Body::Welcome
    } else {
        Body::Screen
    }
}

/// Nombres de todos los paneles (vivos y restaurados): un nombre nuevo no puede repetirlos.
fn names(app: &App) -> Vec<&str> {
    app.panes.iter().map(|p| p.name.as_str()).chain(app.inflight.iter().map(String::as_str)).collect()
}

/// Crea un workspace en `path`: un shell en esa carpeta, con nombre libre y sin formulario.
fn open_folder(app: &mut App, path: String) -> Task<Message> {
    let (rows, cols) = grid_for(app.window);
    let name = launch::free_name("shell", &names(app));
    app.spawn_size = Some((rows, cols));
    let req = launch::shell_request(&name, &login_shell(), &path, None, rows, cols);
    Task::perform(
        async move {
            let result = tokio::task::spawn_blocking(move || request(req)).await.map_err(|e| e.to_string()).and_then(|r| r);
            (path, result)
        },
        |(path, result)| Message::WorkspaceOpened(path, result),
    )
}

/// Primera carpeta reciente que coincide con la búsqueda, si la hay.
fn first_match(app: &App) -> Option<String> {
    let query = app.new_ws.as_deref().unwrap_or("");
    recents::matching(&app.recents, query).first().map(|p| p.to_string())
}

/// Siguiente panel visible sin el foco que hay que sondear, por turnos. `None` si no hay ninguno libre.
fn next_other(app: &mut App) -> Option<u32> {
    let key = focus_key(app)?;
    let ids = app.layouts.active_tab(&key)?.root.ids();
    let others: Vec<u32> = ids.into_iter().filter(|id| Some(*id) != app.selected && !app.others_loading.contains(id)).collect();
    if others.is_empty() {
        return None;
    }
    let id = others[app.others_cursor % others.len()];
    app.others_cursor = app.others_cursor.wrapping_add(1);
    Some(id)
}

/// Pide la pantalla de un panel visible sin el foco: desplazamiento 0, con «lo que cambió» si ya tenemos una.
fn load_other(id: u32, since: Option<u64>) -> Task<Message> {
    Task::perform(async move {
        tokio::task::spawn_blocking(move || snapshot_request(id, 0, since)).await.map_err(|e| e.to_string())?
    }, move |r| Message::OtherScreen(id, r))
}

/// Identificador del buscador de «Nuevo workspace» (B3: para darle el foco).
fn new_ws_input_id() -> iced::widget::text_input::Id {
    iced::widget::text_input::Id::new("nuevo-workspace")
}

/// Resultados de la paleta que se dibujan (y, por tanto, a los que puede llegar el resaltado).
const PALETTE_VISIBLE: usize = 12;

/// Identificador del campo de búsqueda de la paleta: para darle el foco al abrirla.
fn palette_input_id() -> iced::widget::text_input::Id {
    iced::widget::text_input::Id::new("paleta")
}


fn request(req: Request) -> Result<serde_json::Value, String> {
    let response = client::request(&req).map_err(|e| format!("{e:#}"))?;
    if response.ok { Ok(response.data) } else { Err(response.error.unwrap_or_else(|| "Error del daemon".into())) }
}

fn load_panes() -> Task<Message> {
    Task::perform(async {
        tokio::task::spawn_blocking(|| serde_json::from_value(request(Request::List)?).map_err(|e| e.to_string()))
            .await.map_err(|e| e.to_string())?
    }, Message::Panes)
}

/// Cuotas por agente (`Usage`). Va junto a la lista: es una lectura barata, sin tocar los PTY.
fn load_limits() -> Task<Message> {
    Task::perform(async {
        tokio::task::spawn_blocking(|| {
            let data = request(Request::Usage { group_by: None })?;
            serde_json::from_value::<Vec<Limits>>(data["limits"].clone()).map_err(|e| e.to_string())
        })
        .await.map_err(|e| e.to_string())?
    }, Message::Limits)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Respuesta de `snapshot` leída en una sola pasada: `data` queda crudo hasta saber qué es.
#[derive(Deserialize)]
struct SnapEnvelope<'a> {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(borrow)]
    data: &'a serde_json::value::RawValue,
}

/// `{"unchanged": true, "gen": N}`. Con `deny_unknown_fields`, un snapshot completo falla al primer campo
/// que no es suyo, sin recorrerlo entero.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unchanged {
    unchanged: bool,
    gen: u64,
}

#[derive(Debug, Clone)]
enum ScreenReply {
    /// La pantalla sigue en la generación pedida: no hay nada que redibujar.
    Unchanged(u64),
    Full(Snapshot),
}

/// Pide la pantalla de un panel. `since` solo se envía con el mismo `offset` que la última respuesta completa.
fn snapshot_request(id: u32, offset: u16, since: Option<u64>) -> Result<ScreenReply, String> {
    let req = Request::Snapshot { pane: id.to_string(), offset, since };
    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');
    let mut stream = client::connect().map_err(|e| format!("{e:#}"))?;
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).map_err(|e| e.to_string())?;
    if reply.is_empty() {
        return Err("el daemon cerró la conexión sin responder".into());
    }
    parse_snapshot_reply(&reply)
}

/// Interpreta una línea de respuesta de `snapshot` sin reparsear el snapshot: `data` se lee una vez.
fn parse_snapshot_reply(reply: &str) -> Result<ScreenReply, String> {
    let env: SnapEnvelope = serde_json::from_str(reply).map_err(|e| e.to_string())?;
    if !env.ok {
        return Err(env.error.unwrap_or_else(|| "Error del daemon".into()));
    }
    let raw = env.data.get();
    if let Ok(u) = serde_json::from_str::<Unchanged>(raw) {
        if u.unchanged {
            return Ok(ScreenReply::Unchanged(u.gen));
        }
    }
    serde_json::from_str::<Snapshot>(raw).map(ScreenReply::Full).map_err(|e| e.to_string())
}

fn load_screen(id: u32, offset: u16, since: Option<u64>) -> Task<Message> {
    Task::perform(async move {
        tokio::task::spawn_blocking(move || snapshot_request(id, offset, since)).await.map_err(|e| e.to_string())?
    }, move |r| Message::Screen(id, offset, since, r))
}

/// Envía una petición que no es de teclado (crear, cerrar) fuera del hilo de la interfaz.
fn send_request(req: Request, to: fn(Result<serde_json::Value, String>) -> Message) -> Task<Message> {
    Task::perform(async move {
        tokio::task::spawn_blocking(move || request(req)).await.map_err(|e| e.to_string())?
    }, to)
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn login_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
}

fn send_text(app: &App, text: String) {
    let Some(id) = app.selected else { return };
    let req = if text == "\r" {
        Request::Send { pane: id.to_string(), text: String::new(), enter: true }
    } else {
        Request::Send { pane: id.to_string(), text, enter: false }
    };
    let _ = outbox().send(req);
}

/// Quita la selección y su copia congelada.
fn clear_selection(app: &mut App) {
    app.sel = None;
    app.sel_snap = None;
    app.dragging = false;
}

/// Vuelve a la pantalla en vivo y descarta la selección: el contenido mostrado cambia.
fn back_to_live(app: &mut App) {
    app.scroll = 0;
    clear_selection(app);
}

/// Selecciona un panel. Mirarlo no cambia su tamaño y lo deja sin marca de "sin leer".
fn select(app: &mut App, id: u32) -> Task<Message> {
    // B6: la última pantalla del que pierde el foco pasa a `others`, y el que lo gana se muestra desde ahí (no hay
    // «Esperando salida…» durante un tick).
    if let (Some(prev), Some(snap)) = (app.selected, app.snap.take()) {
        if prev != id {
            app.others.insert(prev, snap);
        }
    }
    app.selected = Some(id);
    app.layouts.focus(id);
    app.snap = app.others.get(&id).cloned();
    app.snap_gen = app.snap.as_ref().map(|s| s.gen);
    app.snap_offset = 0;
    app.scroll = 0;
    app.confirm_close = false;
    clear_selection(app);
    app.attention.mark_read(id);
    if app.loading {
        Task::none()
    } else {
        app.loading = true;
        load_screen(id, 0, None)
    }
}

/// Área donde viven los paneles: a la derecha de la barra lateral, entre la barra de pestañas y la de estado.
/// `extra_top` y `extra_bottom` quedan para franjas futuras; hoy nada cambia la geometría (errores, uso y
/// confirmaciones van en la barra de estado), así el terminal no salta de tamaño.
fn terminal_area(window: (f32, f32), extra_top: f32, extra_bottom: f32) -> layout::Rect {
    let x = SIDEBAR_W;
    let y = TABS_H + extra_top;
    layout::Rect { x, y, w: (window.0 - x).max(0.0), h: (window.1 - y - STATUS_H - extra_bottom).max(0.0) }
}

/// Cuadrícula de texto que cabe en un rectángulo de panel (con su margen interior).
fn grid_in(rect: layout::Rect) -> (u16, u16) {
    (((rect.h - 2.0 * LEAF_PAD) / line_h()).floor().max(5.0) as u16, ((rect.w - 2.0 * LEAF_PAD) / cell_w()).floor().max(20.0) as u16)
}

/// Cuadrícula de la ventana entera (un panel nuevo sin split).
fn grid_for(window: (f32, f32)) -> (u16, u16) {
    grid_in(terminal_area(window, 0.0, 0.0))
}

/// Área de los paneles para la ventana actual.
fn terminal_area_for(app: &App) -> layout::Rect {
    terminal_area(app.window, 0.0, 0.0)
}

/// Panel maximizado de la pestaña visible (⌘⇧Enter), si lo hay y sigue en ella.
fn maximized(app: &App) -> Option<u32> {
    let id = app.maximized?;
    let key = focus_key(app)?;
    app.layouts.active_tab(&key).filter(|t| t.root.ids().len() > 1 && t.root.ids().contains(&id)).map(|_| id)
}

/// Rectángulos de los paneles que se ven en la pestaña activa del workspace `key`: todos los del split, o solo el
/// maximizado ocupando el área entera.
fn visible_rects(app: &App, key: &str) -> Vec<(u32, layout::Rect)> {
    let Some(tab) = app.layouts.active_tab(key) else { return Vec::new() };
    let area = terminal_area_for(app);
    match maximized(app).filter(|id| tab.root.ids().contains(id)) {
        Some(id) => vec![(id, area)],
        None => tab.root.rects(area),
    }
}

/// Rectángulo del panel `id` dentro de la pestaña visible de su workspace.
fn pane_rect(app: &App, id: u32) -> Option<layout::Rect> {
    let key = app.layouts.key_of(id)?;
    visible_rects(app, &key).into_iter().find(|(p, _)| *p == id).map(|(_, r)| r)
}

/// Origen de la cuadrícula de un rectángulo de panel (su margen de 10 px).
fn origin_in(rect: layout::Rect) -> (f32, f32) {
    (rect.x + LEAF_PAD, rect.y + LEAF_PAD)
}

/// Origen de la cuadrícula del panel enfocado; sin rectángulo conocido, el de referencia.
fn focus_origin(app: &App) -> (f32, f32) {
    app.selected.and_then(|id| pane_rect(app, id)).map_or(TERM_ORIGIN, origin_in)
}

/// Panel visible bajo el puntero, si hay alguno.
fn leaf_at(app: &App, p: Point) -> Option<u32> {
    if body_mode(app) != Body::Screen {
        return None;
    }
    let key = app.selected.and_then(|id| app.layouts.key_of(id))?;
    visible_rects(app, &key).into_iter().find(|(_, r)| p.x >= r.x && p.y >= r.y && p.x < r.x + r.w && p.y < r.y + r.h).map(|(id, _)| id)
}

/// Margen, en píxeles a cada lado de la línea, con el que se agarra un divisor.
const DIVIDER_SLOP: f32 = 4.0;

/// Divisor de la pestaña visible bajo el punto, si lo hay (no con un panel maximizado ni con capas abiertas).
fn divider_under(app: &App, p: Point) -> Option<layout::Divider> {
    if body_mode(app) != Body::Screen || maximized(app).is_some() {
        return None;
    }
    let key = focus_key(app)?;
    let tab = app.layouts.active_tab(&key)?;
    layout::divider_at(&tab.root, terminal_area_for(app), p.x, p.y, DIVIDER_SLOP)
}

/// Clave de workspace del panel enfocado.
fn focus_key(app: &App) -> Option<String> {
    app.selected.and_then(|id| app.layouts.key_of(id))
}

/// Pares (id, clave de workspace) de todos los paneles, para alinear el layout.
fn pane_keys(app: &App) -> Vec<(u32, String)> {
    app.panes.iter().map(|p| (p.id, p.workspace.clone().unwrap_or_default())).collect()
}

/// Pantalla que se muestra: la congelada si hay selección, si no la última recibida.
fn shown(app: &App) -> Option<&Snapshot> {
    app.sel_snap.as_ref().or(app.snap.as_ref())
}

/// Celda de la cuadrícula bajo el píxel (x, y), con el origen del terminal restado.
/// Fuera de la cuadrícula por la derecha o por abajo se recorta al borde; por la izquierda o
/// por arriba devuelve `None`.
fn pixel_to_cell(origin: (f32, f32), x: f32, y: f32, grid: (u16, u16)) -> Option<Cell> {
    let (rows, cols) = grid;
    if rows == 0 || cols == 0 {
        return None;
    }
    let (ox, oy) = origin;
    if x < ox || y < oy {
        return None;
    }
    let col = ((x - ox) / cell_w()).floor() as u16;
    let row = ((y - oy) / line_h()).floor() as u16;
    Some((row.min(rows - 1), col.min(cols - 1)))
}

/// Si el píxel cae dentro de la cuadrícula visible (no en el recorte de los bordes).
fn inside_grid(origin: (f32, f32), x: f32, y: f32, grid: (u16, u16)) -> bool {
    let (ox, oy) = origin;
    x >= ox && y >= oy && x < ox + grid.1 as f32 * cell_w() && y < oy + grid.0 as f32 * line_h()
}

/// M4: el puntero cuenta para el terminal solo si está dentro del rectángulo del panel enfocado (y de su cuadrícula),
/// y solo si no hay una vista superpuesta.
fn pointer_in_grid(app: &App) -> bool {
    if body_mode(app) != Body::Screen {
        return false;
    }
    let rect = app.selected.and_then(|id| pane_rect(app, id)).unwrap_or_else(|| terminal_area_for(app));
    match (app.pointer, shown(app)) {
        (Some(p), Some(s)) => {
            p.x >= rect.x && p.y >= rect.y && p.x < rect.x + rect.w && p.y < rect.y + rect.h
                && inside_grid(focus_origin(app), p.x, p.y, (s.rows, s.cols))
        }
        _ => false,
    }
}

fn grid_cell(app: &App, p: Point) -> Option<Cell> {
    let snap = shown(app)?;
    pixel_to_cell(focus_origin(app), p.x, p.y, (snap.rows, snap.cols))
}

/// Extremos de una selección en orden de lectura.
fn ordered(sel: Selection) -> (Cell, Cell) {
    if sel.anchor <= sel.head { (sel.anchor, sel.head) } else { (sel.head, sel.anchor) }
}

/// Columnas [inicio, fin] seleccionadas en `row`, o `None` si la fila no está en la selección.
fn selected_cols(sel: Selection, row: u16, cols: u16) -> Option<(u16, u16)> {
    let (s, e) = ordered(sel);
    if row < s.0 || row > e.0 || cols == 0 {
        return None;
    }
    let start = if row == s.0 { s.1 } else { 0 };
    let end = if row == e.0 { e.1 } else { cols - 1 };
    (start <= end).then_some((start, end))
}

/// Texto de la selección. Las filas que continúan en la siguiente (soft wrap) se unen sin salto de
/// línea; el resto termina en `\n` y sin espacios de relleno al final.
fn selection_text(snap: &Snapshot, sel: Selection) -> String {
    let (s, e) = ordered(sel);
    let mut out = String::new();
    for row in s.0..=e.0 {
        let Some(runs) = snap.lines.get(row as usize) else { break };
        let Some((c0, c1)) = selected_cols(sel, row, snap.cols) else { continue };
        let chars: Vec<char> = runs.iter().flat_map(|r| r.t.chars()).collect();
        let part: String = chars.iter().skip(c0 as usize).take((c1 - c0 + 1) as usize).collect();
        let continues = row < e.0 && snap.wrapped.get(row as usize).copied().unwrap_or(false);
        if continues {
            out.push_str(&part);
        } else {
            out.push_str(part.trim_end());
            if row < e.0 {
                out.push('\n');
            }
        }
    }
    out
}

/// Petición de `Resize` para el panel seleccionado, si la ventana cambió desde el último envío.
/// Solo se llama al escribir en un panel o al redimensionar la ventana: mirar un panel no cambia su tamaño.
/// M3: el tamaño de cada panel de la pestaña visible es el de su rectángulo. Devuelve el `Resize` de los que
/// cambiaron desde el último envío. Un fantasma (sin PTY) rechaza el `Resize` en el daemon y esa respuesta se descarta.
fn layout_resizes(app: &mut App) -> Vec<Request> {
    let Some(key) = focus_key(app) else { return Vec::new() };
    let rects = visible_rects(app, &key);
    let mut out = Vec::new();
    for (id, rect) in rects {
        let grid = grid_in(rect);
        if app.sizes.get(&id) != Some(&grid) {
            app.sizes.insert(id, grid);
            out.push(Request::Resize { pane: id.to_string(), rows: grid.0, cols: grid.1 });
        }
    }
    out
}

/// Envía los `Resize` pendientes del layout (llamado desde el tick de pantalla y al escribir o cambiar la ventana).
fn resize_if_needed(app: &mut App) {
    for req in layout_resizes(app) {
        let _ = outbox().send(req);
    }
}

/// Pide la pantalla del panel enfocado dentro de unos milisegundos (lo que tarda el PTY en hacer eco de una tecla),
/// sin esperar al siguiente tick.
fn echo_soon() -> Task<Message> {
    Task::perform(
        async { tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(ECHO_DELAY_MS))).await.ok() },
        |_| Message::TickScreen,
    )
}

/// Espera tras una tecla antes de pedir su eco.
const ECHO_DELAY_MS: u64 = 8;

/// Avisa de las alertas nuevas con una notificación de macOS, si la ventana no tiene el foco.
fn notify(alerts: &[Alert], focused: bool) {
    if focused {
        return;
    }
    for a in alerts {
        let script = attention::notification_script(&a.name, a.status);
        std::thread::spawn(move || {
            let _ = std::process::Command::new("osascript").args(["-e", &script]).status();
        });
    }
}

fn app_title(app: &App) -> String {
    attention::window_title(app.attention.count(&app.panes))
}

/// Punto de entrada de la GUI: aplica el mensaje y, después, garantiza la invariante de foco.
fn update(app: &mut App, message: Message) -> Task<Message> {
    let task = update_inner(app, message);
    keep_focus_visible(app);
    task
}

/// Invariante (G1): el panel seleccionado está en la pestaña activa de su workspace. Si no lo está, la pestaña
/// activa pasa a ser la suya. Así lo que se teclea siempre va a un panel que se ve.
fn keep_focus_visible(app: &mut App) {
    // Un panel maximizado solo se mantiene mientras tiene el foco: si el foco pasa a otro (barra lateral, cierre,
    // otra pestaña), el split vuelve a verse entero y el enfocado no queda oculto.
    if app.maximized.is_some() && app.maximized != app.selected {
        app.maximized = None;
    }
    if let Some(id) = app.selected {
        if let Some(key) = app.layouts.key_of(id) {
            let visible = app.layouts.active_tab(&key).is_some_and(|t| t.root.ids().contains(&id));
            if !visible {
                app.layouts.focus(id);
            }
        }
    }
}

fn update_inner(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::ToggleMaximize => {
            app.maximized = match (app.maximized, app.selected) {
                (Some(_), _) => None,
                (None, Some(id)) if app.layouts.key_of(id).and_then(|k| app.layouts.active_tab(&k).map(|t| t.root.ids().len())).unwrap_or(0) > 1 => Some(id),
                _ => None,
            };
            // Cambia el área de los paneles visibles: se redimensionan a la nueva.
            resize_if_needed(app);
            Task::none()
        }
        Message::Zoom(step) => {
            let next = if step == 0 { 0 } else { (zoom() + step).clamp(ZOOM_MIN, ZOOM_MAX) };
            ZOOM.store(next, std::sync::atomic::Ordering::Relaxed);
            // Con otra letra caben otras filas y columnas: todos los paneles visibles se redimensionan.
            app.sizes.clear();
            resize_if_needed(app);
            Task::none()
        }
        Message::TickScreen => {
            if let Some(e) = outbox_error().lock().unwrap().take() {
                app.error = Some(format!("no se envió: {e}"));
            }
            // M3: los paneles del layout se redimensionan a su rectángulo cuando cambia (split, cierre, ventana).
            resize_if_needed(app);
            let focus_task = match app.selected {
                Some(id) if !app.loading => {
                    app.loading = true;
                    // Solo se pide "lo que cambió" si la última respuesta completa es del mismo desplazamiento.
                    let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
                    load_screen(id, app.scroll, since)
                }
                _ => Task::none(),
            };
            // Un panel visible sin el foco se sondea por turnos: uno por tick, sin tocar su tamaño.
            let other_task = match next_other(app) {
                Some(id) => {
                    app.others_loading.insert(id);
                    load_other(id, app.others_gen.get(&id).copied())
                }
                None => Task::none(),
            };
            Task::batch([focus_task, other_task])
        }
        Message::OtherScreen(id, result) => {
            app.others_loading.remove(&id);
            if let Ok(ScreenReply::Full(snap)) = result {
                app.others_gen.insert(id, snap.gen);
                app.others.insert(id, snap);
            }
            Task::none()
        }
        Message::NextTab(delta) => {
            let Some(key) = focus_key(app) else { return Task::none() };
            app.layouts.cycle(&key, delta);
            match app.layouts.active_tab(&key).map(|t| t.focus) {
                Some(id) => select(app, id),
                None => Task::none(),
            }
        }
        Message::SelectTab(index) => {
            let Some(key) = focus_key(app) else { return Task::none() };
            if let Some(ws) = app.layouts.by_key.get_mut(&key) {
                if index < ws.tabs.len() {
                    ws.active = index;
                }
            }
            match app.layouts.active_tab(&key).map(|t| t.focus) {
                Some(id) => select(app, id),
                None => Task::none(),
            }
        }
        Message::SplitFocused(vertical) => {
            let Some(focus) = app.selected else { return Task::none() };
            let (workspace, cwd) = focus_context(app);
            // El nuevo panel nace con la mitad del rectángulo del enfocado.
            let area = pane_rect(app, focus).unwrap_or_else(|| terminal_area_for(app));
            let (rows, cols) = grid_in(layout::halves(area, vertical).1);
            let name = launch::free_name("shell", &names(app));
            let req = launch::shell_request(&name, &login_shell(), &cwd.unwrap_or_else(home_dir), workspace, rows, cols);
            app.pending_split.insert(name.clone(), (focus, vertical));
            spawn_task_sized(app, req, (rows, cols))
        }
        Message::FocusDir(dir) => {
            let (Some(from), Some(key)) = (app.selected, focus_key(app)) else { return Task::none() };
            let rects = match app.layouts.active_tab(&key) {
                Some(tab) => tab.root.rects(terminal_area_for(app)),
                None => return Task::none(),
            };
            match layout::neighbor(&rects, from, dir) {
                Some(id) => {
                    // Con un panel maximizado, moverse a otro lo maximiza en su lugar (como tmux con zoom).
                    if app.maximized.is_some() {
                        app.maximized = Some(id);
                    }
                    select(app, id)
                }
                None => Task::none(),
            }
        }
        Message::TickList if !app.listing => { app.listing = true; Task::batch([load_panes(), load_limits()]) }
        Message::TickList => Task::none(),
        Message::NewTab => {
            let (rows, cols) = grid_for(app.window);
            let (workspace, cwd) = focus_context(app);
            let name = launch::free_name("shell", &names(app));
            let cwd = cwd.unwrap_or_else(home_dir);
            let req = launch::shell_request(&name, &login_shell(), &cwd, workspace, rows, cols);
            spawn_task(app, req)
        }
        Message::PaneLaunched(Ok(data)) => {
            app.launcher = false;
            app.error = None;
            match serde_json::from_value::<PaneInfo>(data) {
                Ok(p) => {
                    let (rows, cols) = app.spawn_size.take().unwrap_or_else(|| grid_for(app.window));
                    app.sizes.insert(p.id, (rows, cols));
                    app.panes.push(p.clone());
                    // Un split coloca el panel junto al enfocado; si no, nace en su pestaña.
                    app.inflight.retain(|n| *n != p.name);
                    if let Some((focus, vertical)) = app.pending_split.remove(&p.name) {
                        app.layouts.split(focus, vertical, p.id);
                    }
                    app.layouts.sync(&pane_keys(app));
                    app.layouts.focus(p.id);
                    let task = select(app, p.id);
                    Task::batch([task, load_panes()])
                }
                Err(_) => load_panes(),
            }
        }
        Message::PaneLaunched(Err(e)) => {
            app.error = Some(e);
            Task::none()
        }
        Message::LaunchFailed(name, e) => {
            app.inflight.retain(|n| *n != name);
            app.pending_split.remove(&name);
            app.error = Some(e);
            Task::none()
        }
        Message::SelectWorkspaceAt(n) => match workspace_at(&app.panes, &app.layouts, n) {
            Some(id) => select(app, id),
            None => Task::none(),
        },
        Message::NextAttention => {
            // B7: en el orden de la barra lateral, como Cmd+1..9 y la paleta.
            let ordenados: Vec<PaneInfo> = sidebar::workspaces(&app.panes).into_iter().flat_map(|w| w.panes.into_iter().cloned()).collect();
            let next = next_attention(&ordenados, app.selected, |id| app.attention.is_unread(id));
            match next {
                Some(id) => select(app, id),
                None => Task::none(),
            }
        }
        Message::CloseFocused => {
            let vivo = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id)).is_some_and(needs_confirm);
            if vivo {
                app.confirm_close = true;
                Task::none()
            } else {
                update(app, Message::ConfirmClose)
            }
        }
        Message::OpenPalette => {
            if app.palette.is_some() {
                app.palette = None;
                return Task::none();
            }
            app.palette = Some(String::new());
            app.palette_cursor = 0;
            iced::widget::text_input::focus(palette_input_id())
        }
        Message::ClosePalette => {
            app.palette = None;
            Task::none()
        }
        Message::PaletteQuery(query) => {
            app.palette = Some(query);
            app.palette_cursor = 0;
            Task::none()
        }
        Message::PaletteMove(delta) => {
            // B4: el resaltado no pasa de los resultados que se dibujan (12): Enter no ejecuta algo invisible.
            let n = palette_results(app).len().min(PALETTE_VISIBLE);
            if n > 0 {
                app.palette_cursor = (app.palette_cursor as isize + delta).rem_euclid(n as isize) as usize;
            }
            Task::none()
        }
        Message::PaletteRun(index) => {
            let chosen = palette_results(app).into_iter().nth(index).map(|item| item.message);
            app.palette = None;
            match chosen {
                Some(message) => update(app, message),
                None => Task::none(),
            }
        }
        Message::OpenLauncher => {
            app.form = None;
            app.new_ws = None;
            app.launcher = true;
            Task::none()
        }
        Message::CloseLauncher => {
            app.launcher = false;
            Task::none()
        }
        Message::Launch(agent) => {
            let req = context_request(app, agent);
            spawn_task(app, req)
        }
        Message::OpenNewWorkspace => {
            app.form = None;
            app.new_ws = Some(String::new());
            // B3: el foco va al buscador: lo que se escribe, y Enter, llegan ahí.
            iced::widget::text_input::focus(new_ws_input_id())
        }
        Message::CloseNewWorkspace => {
            app.new_ws = None;
            Task::none()
        }
        Message::NewWsQuery(query) => {
            app.new_ws = Some(query);
            Task::none()
        }
        Message::NewWsSubmit => match first_match(app) {
            Some(path) => open_folder(app, path),
            None => Task::none(),
        },
        Message::PickFolder => Task::perform(
            async {
                rfd::AsyncFileDialog::new()
                    .set_title("Elegir la carpeta del workspace")
                    .pick_folder()
                    .await
                    .map(|folder| folder.path().to_path_buf())
            },
            // B10: una ruta que no es UTF-8 no se convierte en otra carpeta con caracteres de reemplazo.
            |folder| match folder {
                None => Message::FolderPicked(None),
                Some(path) => match path.to_str() {
                    Some(text) => Message::FolderPicked(Some(text.to_string())),
                    None => Message::FolderRejected,
                },
            },
        ),
        Message::FolderPicked(Some(path)) => open_folder(app, path),
        Message::FolderPicked(None) => Task::none(),
        Message::FolderRejected => {
            app.error = Some("la carpeta tiene un nombre que no es UTF-8; elige otra o renómbrala".into());
            Task::none()
        }
        Message::OpenFolder(path) => open_folder(app, path),
        Message::WorkspaceOpened(path, Ok(data)) => {
            app.new_ws = None;
            app.error = None;
            // B9: el inicio no es un workspace reciente; las rutas se guardan sin barra final.
            if path != home_dir() {
                recents::push(&mut app.recents, &path);
            }
            if let Some(file) = &app.recents_path {
                let _ = recents::save(file, &app.recents);
            }
            match serde_json::from_value::<PaneInfo>(data) {
                Ok(p) => {
                    let (rows, cols) = app.spawn_size.take().unwrap_or_else(|| grid_for(app.window));
                    app.sizes.insert(p.id, (rows, cols));
                    app.panes.push(p.clone());
                    let task = select(app, p.id);
                    Task::batch([task, load_panes()])
                }
                Err(_) => load_panes(),
            }
        }
        Message::WorkspaceOpened(_, Err(e)) => {
            app.error = Some(e);
            Task::none()
        }
        Message::Limits(Ok(limits)) => {
            app.limits = limits;
            app.limits_error = None;
            Task::none()
        }
        Message::Limits(Err(e)) => {
            app.limits_error = Some(e);
            Task::none()
        }
        Message::Panes(result) => {
            app.listing = false;
            match result {
                Ok(panes) => {
                    app.error = None;
                    app.panes = panes;
                    app.layouts.sync(&pane_keys(app));
                    app.others.retain(|id, _| app.panes.iter().any(|p| p.id == *id));
                    app.others_gen.retain(|id, _| app.panes.iter().any(|p| p.id == *id));
                    if app.selected.is_some_and(|id| !app.panes.iter().any(|p| p.id == id)) {
                        app.selected = None;
                        app.snap = None;
                        app.snap_gen = None;
                        app.scroll = 0;
                        app.confirm_close = false;
                        clear_selection(app);
                    }
                    // M1: si el cierre dejó sin foco, el hermano del split o la pestaña vecina; solo si no hay, el primero.
                    if app.selected.is_none() {
                        if let Some(id) = app.pending_focus.take().filter(|id| app.panes.iter().any(|p| p.id == *id)) {
                            app.selected = Some(id);
                        }
                    }
                    if app.selected.is_none() {
                        if let Some(id) = app.panes.first().map(|p| p.id) {
                            app.selected = Some(id);
                        }
                    }
                    let alerts = app.attention.observe(&app.panes, app.selected);
                    notify(&alerts, app.focused);
                }
                Err(e) => {
                    app.error = Some(e);
                    // Tras un fallo de conexión (quizá un daemon nuevo) no se confía en la generación guardada.
                    app.snap_gen = None;
                }
            }
            Task::none()
        }
        Message::Screen(id, offset, sent_since, result) => {
            app.loading = false;
            if app.selected == Some(id) {
                match result {
                    // Sin cambios. Solo vale si esta respuesta corresponde a la vista que se muestra
                    // ahora: una respuesta en vuelo de antes de seleccionar otro panel no la puede
                    // confirmar (dejaría la pantalla vacía para siempre).
                    Ok(ScreenReply::Unchanged(gen)) => {
                        app.error = None;
                        let same_view = app.snap.is_some() && sent_since.is_some() && sent_since == app.snap_gen && offset == app.snap_offset;
                        app.snap_gen = if same_view { Some(gen) } else { None };
                    }
                    Ok(ScreenReply::Full(snap)) => {
                        app.error = None;
                        // El daemon puede haber recortado el desplazamiento al historial disponible.
                        app.scroll = app.scroll.min(snap.scrollback_max);
                        app.snap_gen = Some(snap.gen);
                        app.snap_offset = offset;
                        app.snap = Some(snap);
                    }
                    Err(e) => {
                        app.error = Some(e);
                        // Sin una vista válida, la siguiente petición debe ser completa.
                        app.snap_gen = None;
                    }
                }
            }
            Task::none()
        }
        Message::Select(id) => select(app, id),
        Message::Event(Event::Window(iced::window::Event::Resized(size))) => {
            app.window = (size.width, size.height);
            resize_if_needed(app);
            Task::none()
        }
        Message::Event(Event::Window(iced::window::Event::Opened { size, .. })) => {
            app.window = (size.width, size.height);
            resize_if_needed(app);
            Task::none()
        }
        Message::Event(Event::Window(iced::window::Event::Focused)) => {
            app.focused = true;
            Task::none()
        }
        Message::Event(Event::Window(iced::window::Event::Unfocused)) => {
            app.focused = false;
            Task::none()
        }
        Message::Event(Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, text, .. })) => {
            // La paleta tiene su propio teclado: flechas, Enter y Esc; lo demás lo escribe su campo.
            if app.palette.is_some() {
                return match &key {
                    Key::Named(Named::Escape) => update(app, Message::ClosePalette),
                    Key::Named(Named::ArrowDown) => update(app, Message::PaletteMove(1)),
                    Key::Named(Named::ArrowUp) => update(app, Message::PaletteMove(-1)),
                    _ if modifiers.command() && matches!(&key, Key::Character(c) if c.as_str() == "k") => update(app, Message::ClosePalette),
                    _ => Task::none(),
                };
            }
            // Cmd+Opt+flechas: el foco se mueve al split vecino.
            if modifiers.command() && modifiers.alt() {
                let dir = match &key {
                    Key::Named(Named::ArrowLeft) => Some(layout::Dir::Left),
                    Key::Named(Named::ArrowRight) => Some(layout::Dir::Right),
                    Key::Named(Named::ArrowUp) => Some(layout::Dir::Up),
                    Key::Named(Named::ArrowDown) => Some(layout::Dir::Down),
                    _ => None,
                };
                if let Some(dir) = dir {
                    return update(app, Message::FocusDir(dir));
                }
            }
            // Cmd+Shift+Enter: maximizar o restaurar el panel enfocado de un split.
            if modifiers.command() && modifiers.shift() && matches!(&key, Key::Named(Named::Enter)) && body_mode(app) == Body::Screen {
                return update(app, Message::ToggleMaximize);
            }
            // Atajos de la ventana (Cmd+…), salvo mientras «Nuevo workspace» está abierto.
            if modifiers.command() && app.new_ws.is_none() && !app.launcher {
                if let Some(message) = shortcut_in_state(app, &key, modifiers) {
                    return update(app, message);
                }
            }
            // En el lanzador, 1-5 lanza el agente de esa fila.
            if app.launcher && !modifiers.command() {
                if let Key::Character(c) = &key {
                    if let Ok(n @ 1..=5) = c.as_str().parse::<usize>() {
                        return update(app, Message::Launch(launch::Agent::ALL[n - 1]));
                    }
                }
            }
            // Con «Nuevo workspace» o el lanzador abiertos, el teclado es suyo, no del panel.
            if app.new_ws.is_some() || app.launcher {
                if matches!(&key, Key::Named(Named::Escape)) {
                    app.new_ws = None;
                    app.launcher = false;
                }
                return Task::none();
            }
            if app.form.is_some() {
                return Task::none();
            }
            // Con la vista congelada, Esc la libera (no se envía al panel).
            if app.sel_snap.is_some() && matches!(&key, Key::Named(Named::Escape)) {
                clear_selection(app);
                return Task::none();
            }
            if modifiers.command() && matches!(&key, Key::Character(c) if c.as_str() == "v") {
                app.paste_pending = true;
                return iced::clipboard::read().map(Message::Paste);
            }
            if modifiers.command() && matches!(&key, Key::Character(c) if c.as_str() == "c") {
                let copied = match (app.sel, shown(app)) {
                    (Some(sel), Some(snap)) => selection_text(snap, sel),
                    _ => String::new(),
                };
                return if copied.is_empty() { Task::none() } else { iced::clipboard::write(copied) };
            }
            if let Some(bytes) = term::encode_cmd(&key, modifiers).or_else(|| term::encode(&key, modifiers, text.as_deref())) {
                back_to_live(app);
                // Escribir en un panel lo redimensiona a la ventana, si hace falta.
                resize_if_needed(app);
                if let Some(bytes) = route_input(app, bytes) {
                    send_text(app, bytes);
                    // Eco rápido: se pide la pantalla poco después de la tecla en vez de esperar al tick de 50 ms.
                    return echo_soon();
                }
            }
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::WheelScrolled { delta })) => {
            // Solo el historial del terminal: la rueda sobre la barra lateral no lo mueve.
            if !pointer_in_grid(app) {
                return Task::none();
            }
            let lines = match delta {
                mouse::ScrollDelta::Lines { y, .. } => y,
                mouse::ScrollDelta::Pixels { y, .. } => y / line_h(),
            };
            // Un trackpad envía fracciones de línea: se acumulan hasta sumar un paso completo.
            app.wheel_acc += lines;
            let steps = app.wheel_acc.trunc() as i32;
            app.wheel_acc -= steps as f32;
            if steps != 0 {
                let max = app.snap.as_ref().map_or(0, |s| s.scrollback_max) as i32;
                let next = (app.scroll as i32 + steps * WHEEL_LINES).clamp(0, max);
                if next as u16 != app.scroll {
                    app.scroll = next as u16;
                    clear_selection(app);
                }
            }
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))) if app.pointer.and_then(|p| divider_under(app, p)).is_some() => {
            // Empieza a arrastrar un divisor de split: tiene prioridad sobre el foco y la selección.
            app.drag_divider = app.pointer.and_then(|p| divider_under(app, p));
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))) => {
            // M4: con una vista superpuesta (paleta, formulario, lanzador, nuevo workspace) el ratón no llega al terminal.
            if body_mode(app) != Body::Screen {
                return Task::none();
            }
            // Un clic en otro panel visible le da el foco; el arrastre empieza en el siguiente clic dentro de él.
            if let Some(id) = app.pointer.and_then(|p| leaf_at(app, p)) {
                if app.selected != Some(id) {
                    return select(app, id);
                }
            }
            if !pointer_in_grid(app) {
                return Task::none();
            }
            if let Some(cell) = app.pointer.and_then(|p| grid_cell(app, p)) {
                // Se congela la pantalla que el usuario ve en este momento.
                app.sel_snap = app.snap.clone();
                app.sel = Some(Selection { anchor: cell, head: cell });
                app.dragging = true;
            }
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::CursorMoved { position })) => {
            app.pointer = Some(position);
            if let Some(d) = app.drag_divider.clone() {
                if let Some(key) = focus_key(app) {
                    app.layouts.set_ratio(&key, &d.path, d.ratio_at(position.x, position.y));
                }
                return Task::none();
            }
            app.hover_divider = divider_under(app, position).map(|d| (d.path, d.vertical));
            if app.dragging {
                if let (Some(cell), Some(sel)) = (grid_cell(app, position), app.sel.as_mut()) {
                    sel.head = cell;
                }
            }
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))) if app.drag_divider.is_some() => {
            // Al soltar el divisor, los paneles toman su tamaño nuevo.
            app.drag_divider = None;
            resize_if_needed(app);
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))) => {
            app.dragging = false;
            // Un clic sin arrastre no deja selección.
            if app.sel.is_some_and(|s| s.anchor == s.head) {
                clear_selection(app);
            }
            Task::none()
        }
        Message::Event(_) => Task::none(),
        Message::Paste(text) => {
            app.paste_pending = false;
            let bracketed = app.snap.as_ref().is_some_and(|s| s.bracketed_paste);
            if let Some(t) = text.filter(|t| !t.is_empty()) {
                back_to_live(app);
                resize_if_needed(app);
                send_text(app, paste_bytes(&t, bracketed));
            }
            // Las teclas que llegaron durante el pegado van ahora, detrás de él.
            for bytes in std::mem::take(&mut app.key_queue) {
                send_text(app, bytes);
            }
            Task::none()
        }

        // ---- Formulario de nuevo panel ----
        Message::ToggleGroup(key) => {
            if !app.collapsed.remove(&key) {
                app.collapsed.insert(key);
            }
            Task::none()
        }
        Message::FormWorkspace(v) => {
            if let Some(f) = app.form.as_mut() { f.workspace = v; }
            Task::none()
        }
        Message::OpenForm => {
            // B4: el workspace empieza vacío. Heredar el del panel seleccionado agrupaba en silencio un panel nuevo
            // de otro repo bajo el workspace equivocado (un workspace explícito anula la deducción por git).
            app.form = Some(NewPane::default());
            app.form_error = None;
            Task::none()
        }
        Message::CancelForm => {
            app.form = None;
            app.form_error = None;
            Task::none()
        }
        Message::FormName(v) => {
            if let Some(f) = app.form.as_mut() { f.name = v; }
            Task::none()
        }
        Message::FormPreset(label) => {
            if let (Some(f), Some(p)) = (app.form.as_mut(), Preset::from_label(&label)) { f.preset = p; }
            Task::none()
        }
        Message::FormCustom(v) => {
            if let Some(f) = app.form.as_mut() { f.custom = v; }
            Task::none()
        }
        Message::FormCwd(v) => {
            if let Some(f) = app.form.as_mut() { f.cwd = v; }
            Task::none()
        }
        Message::FormManager(name) => {
            if let Some(f) = app.form.as_mut() {
                f.manager = if name == NO_MANAGER { None } else { Some(name) };
            }
            Task::none()
        }
        Message::SubmitForm => {
            let Some(form) = app.form.clone() else { return Task::none() };
            let (rows, cols) = grid_for(app.window);
            let (home, shell) = (home_dir(), login_shell());
            match newpane::spawn_request(&form, rows, cols, &newpane::Env { home: &home, shell: &shell }) {
                Ok(req) => {
                    app.form_error = None;
                    app.spawn_size = Some((rows, cols));
                    send_request(req, Message::Spawned)
                }
                Err(e) => {
                    app.form_error = Some(e);
                    Task::none()
                }
            }
        }
        Message::Spawned(Ok(data)) => {
            app.form = None;
            app.form_error = None;
            match serde_json::from_value::<PaneInfo>(data) {
                Ok(p) => {
                    // El panel nace con el tamaño de la ventana: no hace falta otro Resize.
                    let (rows, cols) = app.spawn_size.take().unwrap_or_else(|| grid_for(app.window));
                    app.sizes.insert(p.id, (rows, cols));
                    app.panes.push(p.clone());
                    let task = select(app, p.id);
                    Task::batch([task, load_panes()])
                }
                Err(_) => load_panes(),
            }
        }
        Message::Spawned(Err(e)) => {
            app.form_error = Some(e);
            Task::none()
        }

        // ---- Cerrar el panel seleccionado, con confirmación ----
        Message::AskClose => {
            app.confirm_close = app.selected.is_some();
            Task::none()
        }
        Message::CancelClose => {
            app.confirm_close = false;
            Task::none()
        }
        Message::ConfirmClose => {
            app.confirm_close = false;
            // M1: el foco se decide antes de cerrar, mientras el panel y su hermano aún están en el layout.
            app.pending_focus = app.selected.and_then(|id| successor(app, id));
            match app.selected {
                Some(id) => send_request(Request::Kill { pane: id.to_string() }, Message::Killed),
                None => Task::none(),
            }
        }
        Message::Killed(Ok(_)) => load_panes(),
        Message::Killed(Err(e)) => {
            app.error = Some(e);
            load_panes()
        }
    }
}

/// Un tramo de texto con el estilo de `r`; `invert` lo muestra invertido (cursor o selección).
/// Cómo se pinta una celda: normal, bajo el cursor (bloque invertido) o seleccionada (fondo de acento).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Normal,
    Cursor,
    Selected,
}

/// Fondo de la selección: el acento translúcido sobre el fondo del terminal, con el texto en su color.
const SELECTION_BG: Color = Color::from_rgb(0.153, 0.243, 0.420);

/// Un tramo de texto con el estilo de `r` y la marca de celda.
fn styled(text: String, r: &Run, mark: Mark) -> Span<'static, Message> {
    let (mut fg, mut bg) = (term::color(r.fg), term::color(r.bg));
    if r.inverse ^ (mark == Mark::Cursor) {
        let (f, b) = (bg.unwrap_or(term::DEFAULT_BG), fg.unwrap_or(term::DEFAULT_FG));
        fg = Some(f);
        bg = Some(b);
    }
    if mark == Mark::Selected {
        bg = Some(SELECTION_BG);
    }
    let mut sp: Span<'static, Message> = span(text).color(fg.unwrap_or(term::DEFAULT_FG)).underline(r.underline);
    if let Some(b) = bg { sp = sp.background(b); }
    if r.bold || r.italic {
        sp = sp.font(Font {
            weight: if r.bold { font::Weight::Bold } else { font::Weight::Normal },
            style: if r.italic { font::Style::Italic } else { font::Style::Normal },
            ..terminal_font()
        });
    }
    sp
}

fn spans_for_line(runs: &[Run], cursor_col: Option<u16>, sel: Option<(u16, u16)>) -> Vec<Span<'static, Message>> {
    let mut out = Vec::new();
    let mut col: u16 = 0;
    for r in runs {
        let mut chunk = String::new();
        let mut chunk_mark = Mark::Normal;
        for ch in r.t.chars() {
            let mark = if cursor_col == Some(col) {
                Mark::Cursor
            } else if sel.is_some_and(|(a, b)| (a..=b).contains(&col)) {
                Mark::Selected
            } else {
                Mark::Normal
            };
            if !chunk.is_empty() && mark != chunk_mark {
                out.push(styled(std::mem::take(&mut chunk), r, chunk_mark));
            }
            chunk_mark = mark;
            chunk.push(ch);
            col += 1;
        }
        if !chunk.is_empty() {
            out.push(styled(chunk, r, chunk_mark));
        }
    }
    if out.is_empty() { out.push(span(" ".to_string())); }
    out
}

/// Pantalla de un panel. `cursor`: se dibuja el cursor (solo el panel enfocado, con la ventana activa).
fn screen_view(snap: &Snapshot, sel: Option<Selection>, cursor: bool) -> Element<'static, Message> {
    let mut lines = column![].spacing(0);
    // El cursor solo se dibuja en la pantalla en vivo.
    let live = cursor && snap.scrollback == 0;
    for (r, runs) in snap.lines.iter().enumerate() {
        let cursor = (live && !snap.cursor_hidden && snap.cursor.0 as usize == r).then_some(snap.cursor.1);
        let selected = sel.and_then(|s| selected_cols(s, r as u16, snap.cols));
        let spans = spans_for_line(runs, cursor, selected);
        lines = lines.push(
            rich_text(spans).size(font_size()).font(terminal_font()).line_height(iced::widget::text::LineHeight::Absolute(line_h().into())).wrapping(iced::widget::text::Wrapping::None),
        );
    }
    lines.into()
}


fn view(app: &App) -> Element<'_, Message> {
    let main: Element<'_, Message> = if app.panes.is_empty() {
        chrome::welcome_view(app)
    } else {
        column![chrome::tabs_bar(app), container(chrome::layout_view(app)).width(Length::Fill).height(Length::Fill).style(theme::surface(theme::TERM_BG)), chrome::status_bar(app)].into()
    };
    let base: Element<'_, Message> = container(row![chrome::sidebar_view(app), main]).width(Length::Fill).height(Length::Fill).style(theme::surface(BG)).into();
    match body_mode(app) {
        Body::Palette => chrome::floating(base, chrome::palette_panel(app), 600.0, Message::ClosePalette),
        Body::NewWorkspace => chrome::floating(base, chrome::new_workspace_panel(app), 560.0, Message::CloseNewWorkspace),
        Body::Launcher => chrome::floating(base, chrome::launcher_panel(app), 440.0, Message::CloseLauncher),
        Body::Form => match &app.form {
            Some(f) => chrome::floating(base, chrome::form_panel(app, f), 460.0, Message::CancelForm),
            None => base,
        },
        Body::Welcome | Body::Screen => base,
    }
}

fn main() -> iced::Result {
    iced::application(app_title, update, view)
        .theme(|_| theme::app())
        .subscription(|_: &App| Subscription::batch([
            iced::time::every(Duration::from_millis(50)).map(|_| Message::TickScreen),
            iced::time::every(Duration::from_secs(1)).map(|_| Message::TickList),
            event::listen_with(|event, status, _id| match event {
                Event::Keyboard(keyboard::Event::KeyPressed { .. }) => Some(Message::Event(event)),
                Event::Window(iced::window::Event::Resized(_))
                | Event::Window(iced::window::Event::Opened { .. })
                | Event::Window(iced::window::Event::Focused)
                | Event::Window(iced::window::Event::Unfocused) => Some(Message::Event(event)),
                // Lo que otro widget ya capturó (la lista, un botón, un campo) no llega al terminal.
                // El soltar del botón siempre llega, para no dejar una selección arrastrándose.
                Event::Mouse(mouse::Event::WheelScrolled { .. })
                | Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                    if status == event::Status::Ignored => Some(Message::Event(event)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                | Event::Mouse(mouse::Event::CursorMoved { .. }) => Some(Message::Event(event)),
                _ => None,
            }),
        ]))
        .window_size(iced::Size::new(1100.0, 700.0))
        .run_with(|| {
            let file = recents::file();
            let app = App { window: (1100.0, 700.0), focused: true, recents: recents::load(&file), recents_path: Some(file), ..Default::default() };
            (app, Task::batch([load_panes(), load_limits()]))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &str) -> Run {
        Run { t: t.into(), fg: galgo_proto::Color::Default, bg: galgo_proto::Color::Default, bold: false, italic: false, underline: false, inverse: false }
    }

    fn snap(lines: &[&str], cols: u16) -> Snapshot {
        Snapshot {
            gen: 0,
            rows: lines.len() as u16,
            cols,
            cursor: (0, 0),
            cursor_hidden: true,
            lines: lines.iter().map(|l| vec![run(l)]).collect(),
            scrollback: 0,
            scrollback_max: 0,
            bracketed_paste: false,
            wrapped: vec![],
        }
    }

    /// Píxel del centro de la celda (row, col).
    fn center(row: u16, col: u16) -> (f32, f32) {
        (TERM_ORIGIN.0 + (col as f32 + 0.5) * CELL_W, TERM_ORIGIN.1 + (row as f32 + 0.5) * LINE_H)
    }

    #[test]
    fn pixel_a_celda_dentro_de_la_cuadricula() {
        let (x, y) = center(2, 3);
        assert_eq!(pixel_to_cell(TERM_ORIGIN, x, y, (10, 80)), Some((2, 3)));
        // Esquina superior izquierda del área del terminal.
        assert_eq!(pixel_to_cell(TERM_ORIGIN, TERM_ORIGIN.0, TERM_ORIGIN.1, (10, 80)), Some((0, 0)));
    }

    #[test]
    fn pixel_a_celda_recorta_al_borde_y_rechaza_fuera_por_la_izquierda() {
        assert_eq!(pixel_to_cell(TERM_ORIGIN, TERM_ORIGIN.0 + 10_000.0, TERM_ORIGIN.1 + 10_000.0, (10, 80)), Some((9, 79)));
        assert_eq!(pixel_to_cell(TERM_ORIGIN, TERM_ORIGIN.0 - 1.0, TERM_ORIGIN.1 + 5.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(TERM_ORIGIN, TERM_ORIGIN.0 + 5.0, TERM_ORIGIN.1 - 1.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(TERM_ORIGIN, 50.0, 50.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(TERM_ORIGIN, TERM_ORIGIN.0, TERM_ORIGIN.1, (0, 80)), None);
    }

    /// B3: la rueda y el clic solo cuentan dentro de la cuadrícula.
    #[test]
    fn dentro_de_la_cuadricula_solo() {
        assert!(inside_grid(TERM_ORIGIN, TERM_ORIGIN.0, TERM_ORIGIN.1, (10, 80)));
        assert!(!inside_grid(TERM_ORIGIN, TERM_ORIGIN.0 + 10_000.0, TERM_ORIGIN.1, (10, 80)));
        assert!(!inside_grid(TERM_ORIGIN, TERM_ORIGIN.0, TERM_ORIGIN.1 + 10.0 * LINE_H, (10, 80)));
        assert!(!inside_grid(TERM_ORIGIN, 50.0, 300.0, (10, 80)));
    }

    #[test]
    fn texto_de_una_fila_recorta_columnas_y_espacios_finales() {
        let s = snap(&["hola mundo    ", "segunda"], 14);
        let sel = Selection { anchor: (0, 0), head: (0, 3) };
        assert_eq!(selection_text(&s, sel), "hola");
        // Selección que incluye el relleno: los espacios finales desaparecen.
        let sel = Selection { anchor: (0, 4), head: (0, 13) };
        assert_eq!(selection_text(&s, sel), " mundo");
    }

    #[test]
    fn texto_de_varias_filas_en_orden_de_lectura() {
        let s = snap(&["uno   ", "dos   ", "tres  "], 6);
        // Anclaje después del head: la selección se normaliza.
        let sel = Selection { anchor: (1, 1), head: (0, 2) };
        assert_eq!(selection_text(&s, sel), "o\ndo");
        let sel = Selection { anchor: (0, 0), head: (2, 2) };
        assert_eq!(selection_text(&s, sel), "uno\ndos\ntre");
    }

    /// M2: una línea partida por el terminal se copia entera, sin salto de línea.
    #[test]
    fn copia_une_las_filas_con_soft_wrap() {
        let mut s = snap(&["https://ej", "emplo.com", "abc"], 10);
        s.wrapped = vec![true, true, false];
        let sel = Selection { anchor: (0, 0), head: (2, 2) };
        assert_eq!(selection_text(&s, sel), "https://ejemplo.comabc");
        // Sin el marcador de soft wrap, cada fila es una línea.
        s.wrapped = vec![false, false, false];
        assert_eq!(selection_text(&s, sel), "https://ej\nemplo.com\nabc");
    }

    /// M1: la selección se resalta y se copia desde la pantalla que el usuario vio al empezarla,
    /// aunque lleguen líneas nuevas.
    #[test]
    fn la_seleccion_no_se_mueve_con_la_salida_nueva() {
        // Con un panel seleccionado, la vista es la del terminal (M4: fuera de ella el ratón no cuenta).
        let mut app = App { window: (1100.0, 700.0), panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        app.snap = Some(snap(&["aaaa", "bbbb"], 4));
        app.pointer = Some(Point::new(center(0, 0).0, center(0, 0).1));
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))));
        app.pointer = Some(Point::new(center(1, 1).0, center(1, 1).1));
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::CursorMoved { position: Point::new(center(1, 1).0, center(1, 1).1) })));
        // Llega salida nueva: la pantalla cambia, la selección no.
        app.snap = Some(snap(&["cccc", "dddd"], 4));
        let sel = app.sel.unwrap();
        assert_eq!(selection_text(shown(&app).unwrap(), sel), "aaaa\nbb");
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))));
        assert!(app.sel_snap.is_some());
    }

    #[test]
    fn celdas_seleccionadas_por_fila() {
        let sel = Selection { anchor: (1, 4), head: (3, 2) };
        assert_eq!(selected_cols(sel, 0, 10), None);
        assert_eq!(selected_cols(sel, 1, 10), Some((4, 9)));
        assert_eq!(selected_cols(sel, 2, 10), Some((0, 9)));
        assert_eq!(selected_cols(sel, 3, 10), Some((0, 2)));
        assert_eq!(selected_cols(sel, 4, 10), None);
    }

    #[test]
    fn pegado_con_y_sin_bracketed_paste() {
        assert_eq!(paste_bytes("uno\ndos", true), "\x1b[200~uno\rdos\x1b[201~");
        // Un ESC[201~ dentro del texto no puede cerrar el pegado antes de tiempo.
        assert_eq!(paste_bytes("a\x1b[201~b", true), "\x1b[200~ab\x1b[201~");
        // Sin bracketed paste no hay envoltorio, y los ESC del texto se quitan.
        assert_eq!(paste_bytes("uno\ndos", false), "uno\rdos");
        assert_eq!(paste_bytes("a\x1b[2Jb", false), "a[2Jb");
    }

    #[test]
    fn fusion_de_envios_consecutivos_del_mismo_panel() {
        let send = |p: &str, t: &str, e: bool| Request::Send { pane: p.into(), text: t.into(), enter: e };
        let batch = vec![
            send("1", "a", false),
            send("1", "b", false),
            send("2", "c", false),
            send("2", "", true),
            send("2", "d", false),
        ];
        let out = coalesce(batch);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Request::Send { pane, text, enter: false } if pane == "1" && text == "ab"));
        assert!(matches!(&out[1], Request::Send { pane, text, enter: true } if pane == "2" && text == "c"));
        assert!(matches!(&out[2], Request::Send { pane, text, enter: false } if pane == "2" && text == "d"));
    }

    #[test]
    fn el_tamano_se_envia_al_escribir_o_redimensionar_no_al_mirar() {
        let mut app = App { window: (1100.0, 700.0), selected: Some(1), ..Default::default() };
        // Mirar (seleccionar) no pide nada: no hay petición hasta que se escribe o se redimensiona.
        assert!(app.sizes.is_empty());
        // M3: el panel 1 (sin split) se redimensiona a su rectángulo una vez; luego nada cambia.
        let mut p1 = pane_info(1, "uno", Status::Idle);
        p1.workspace = Some("w".into());
        app.panes = vec![p1];
        app.layouts.sync(&pane_keys(&app));
        assert!(matches!(layout_resizes(&mut app).as_slice(), [Request::Resize { pane, .. }] if pane == "1"));
        assert!(layout_resizes(&mut app).is_empty());
    }

    #[test]
    fn las_teclas_esperan_detras_de_un_pegado_pendiente() {
        let mut app = App { paste_pending: true, ..Default::default() };
        assert_eq!(route_input(&mut app, "x".into()), None);
        assert_eq!(route_input(&mut app, "y".into()), None);
        assert_eq!(app.key_queue, vec!["x".to_string(), "y".to_string()]);
        app.paste_pending = false;
        assert_eq!(route_input(&mut app, "z".into()), Some("z".into()));
    }

    fn pane_info(id: u32, name: &str, status: Status) -> PaneInfo {
        PaneInfo {
            id,
            name: name.into(),
            status,
            manager: None,
            cmd: vec![],
            unacked: 0,
            oldest_unacked_secs: None,
            orphan: false,
            waiting_on: None,
            waiting_secs: None,
            workspace: None,
            cwd: None,
            usage: None,
        }
    }

    /// C: la lista de paneles alimenta la atención; el panel seleccionado no queda sin leer.
    #[test]
    fn panes_actualiza_atencion_y_alertas() {
        let mut app = App { focused: true, selected: Some(1), ..Default::default() };
        app.panes = vec![pane_info(1, "a", Status::Working), pane_info(2, "b", Status::Working)];
        let first = app.panes.clone();
        let _ = update(&mut app, Message::Panes(Ok(first)));
        let mut next = app.panes.clone();
        next[0].status = Status::Done;
        next[1].status = Status::Done;
        let _ = update(&mut app, Message::Panes(Ok(next)));
        assert!(!app.attention.is_unread(1), "el seleccionado no queda sin leer");
        assert!(app.attention.is_unread(2));
        assert_eq!(app_title(&app), "galgo (1)");
        // Al seleccionarlo, se limpia.
        let _ = update(&mut app, Message::Select(2));
        assert_eq!(app_title(&app), "galgo");
    }

    /// C: el formulario bloquea el teclado del panel mientras está abierto.
    #[test]
    fn con_el_formulario_abierto_el_teclado_no_va_al_panel() {
        let mut app = App { selected: Some(1), form: Some(NewPane::default()), ..Default::default() };
        let key = Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character("a".into()),
            modified_key: Key::Character("a".into()),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("a".into()),
        });
        let _ = update(&mut app, Message::Event(key));
        // Sin formulario, la tecla habría quedado pendiente tras un pegado; aquí no se toca nada.
        assert!(app.key_queue.is_empty());
        assert!(app.form.is_some());
    }

    /// Rendimiento: una respuesta `unchanged` no se trata como snapshot, y un snapshot completo sí.
    #[test]
    fn respuesta_unchanged_y_completa() {
        match parse_snapshot_reply(r#"{"ok":true,"error":null,"data":{"unchanged":true,"gen":42}}"#) {
            Ok(ScreenReply::Unchanged(g)) => assert_eq!(g, 42),
            other => panic!("esperaba unchanged: {other:?}"),
        }
        let full = snap(&["hola"], 4);
        let line = format!(r#"{{"ok":true,"error":null,"data":{}}}"#, serde_json::to_string(&full).unwrap());
        match parse_snapshot_reply(&line) {
            Ok(ScreenReply::Full(s)) => assert_eq!(s.rows, 1),
            other => panic!("esperaba snapshot completo: {other:?}"),
        }
        assert_eq!(parse_snapshot_reply(r#"{"ok":false,"error":"panel no encontrado: x","data":null}"#).unwrap_err(), "panel no encontrado: x");
    }

    fn key_press(key: Key, text: Option<&str>) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: text.map(Into::into),
        })
    }

    /// M1: una respuesta `unchanged` que llega después de cambiar de panel no puede dejar la pantalla vacía.
    #[test]
    fn unchanged_en_vuelo_no_deja_la_pantalla_vacia() {
        let mut app = App { selected: Some(1), ..Default::default() };
        // Se pidió con since=5 y la vista se perdió (seleccionar otro panel y volver deja snap=None).
        let _ = update(&mut app, Message::Screen(1, 0, Some(5), Ok(ScreenReply::Unchanged(5))));
        assert!(app.snap.is_none());
        assert_eq!(app.snap_gen, None, "la siguiente petición debe ser completa");
    }

    /// M1, caso válido: con la misma vista, `unchanged` conserva la generación.
    #[test]
    fn unchanged_con_la_misma_vista_conserva_la_generacion() {
        let mut app = App { selected: Some(1), snap: Some(snap(&["a"], 1)), snap_gen: Some(5), ..Default::default() };
        let _ = update(&mut app, Message::Screen(1, 0, Some(5), Ok(ScreenReply::Unchanged(5))));
        assert_eq!(app.snap_gen, Some(5));
    }

    /// B2: con la vista congelada, Esc la libera y no llega al panel.
    #[test]
    fn escape_libera_la_vista_congelada() {
        let mut app = App { selected: Some(1), snap: Some(snap(&["a"], 1)), ..Default::default() };
        app.sel_snap = app.snap.clone();
        app.sel = Some(Selection { anchor: (0, 0), head: (0, 0) });
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Escape), None)));
        assert!(app.sel_snap.is_none());
        assert!(app.sel.is_none());
    }

    /// B7: el panel nace con el tamaño con el que se pidió, aunque la ventana haya cambiado después.
    #[test]
    fn el_panel_nace_con_el_tamano_de_la_peticion() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        app.form = Some(NewPane { name: "w".into(), preset: Preset::Shell, ..Default::default() });
        let _ = update(&mut app, Message::SubmitForm);
        let pedido = grid_for(app.window);
        app.window = (1600.0, 1000.0);
        let data = serde_json::to_value(pane_info(7, "w", Status::Idle)).unwrap();
        let _ = update(&mut app, Message::Spawned(Ok(data)));
        assert_eq!(app.sizes.get(&7), Some(&pedido));
    }

    /// B1: tras un fallo de conexión no se confía en la generación guardada.
    #[test]
    fn fallo_de_paneles_invalida_la_generacion() {
        let mut app = App { selected: Some(1), snap: Some(snap(&["a"], 1)), snap_gen: Some(3), ..Default::default() };
        let _ = update(&mut app, Message::Panes(Err("no hay daemon".into())));
        assert_eq!(app.snap_gen, None);
    }

    /// Workspace: el formulario de un panel nuevo hereda el workspace del seleccionado.
    #[test]
    fn el_formulario_no_hereda_el_workspace_del_seleccionado() {
        let mut p = pane_info(1, "w", Status::Idle);
        p.workspace = Some("inventia/cotizacion".into());
        let mut app = App { panes: vec![p], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::OpenForm);
        assert_eq!(app.form.as_ref().unwrap().workspace, "", "B4: el campo empieza vacío");
        let _ = update(&mut app, Message::FormWorkspace("galgo/main".into()));
        assert_eq!(app.form.as_ref().unwrap().workspace, "galgo/main");
    }

    /// Grupos: plegar y desplegar alterna la clave del grupo.
    #[test]
    fn plegar_y_desplegar_un_grupo() {
        let mut app = App::default();
        let _ = update(&mut app, Message::ToggleGroup("inventia/cotizacion".into()));
        assert!(app.collapsed.contains("inventia/cotizacion"));
        let _ = update(&mut app, Message::ToggleGroup("inventia/cotizacion".into()));
        assert!(!app.collapsed.contains("inventia/cotizacion"));
    }

    /// Las cuotas llegan al estado; un error de lectura se guarda aparte y no borra las últimas cuotas.
    #[test]
    fn cuotas_llegan_y_un_error_no_las_borra() {
        let mut app = App::default();
        let limits = vec![Limits { agent: "claude".into(), plan: None, windows: vec![], observed_at: 1 }];
        let _ = update(&mut app, Message::Limits(Ok(limits)));
        assert_eq!(app.limits.len(), 1);
        let _ = update(&mut app, Message::Limits(Err("sin daemon".into())));
        assert_eq!(app.limits.len(), 1, "las últimas cuotas se conservan");
        assert_eq!(app.limits_error.as_deref(), Some("sin daemon"));
    }

    /// A: el cuerpo muestra la bienvenida sin paneles; «Nuevo workspace» tiene prioridad sobre el resto.
    #[test]
    fn cuerpo_sin_paneles_es_la_bienvenida() {
        let mut app = App::default();
        assert_eq!(body_mode(&app), Body::Welcome);
        let _ = update(&mut app, Message::OpenNewWorkspace);
        assert_eq!(body_mode(&app), Body::NewWorkspace);
        let _ = update(&mut app, Message::CloseNewWorkspace);
        assert_eq!(body_mode(&app), Body::Welcome);
    }

    /// A: Cmd+N abre «Nuevo workspace»; las demás teclas con Cmd siguen siendo de la ventana o del panel.
    #[test]
    fn cmd_n_es_nuevo_workspace() {
        let n = Key::Character("n".into());
        assert!(matches!(shortcut(&n, keyboard::Modifiers::COMMAND), Some(Message::OpenNewWorkspace)));
        assert!(shortcut(&Key::Character("x".into()), keyboard::Modifiers::COMMAND).is_none());
    }

    /// A: una carpeta elegida crea (vía el daemon) un shell que queda seleccionado, y la carpeta pasa a recientes.
    #[test]
    fn carpeta_abierta_queda_seleccionada_y_reciente() {
        let mut app = App { new_ws: Some(String::new()), ..Default::default() };
        let pane = serde_json::to_value(pane_info(7, "shell", Status::Idle)).unwrap();
        let _ = update(&mut app, Message::WorkspaceOpened("/Users/x/proyecto".into(), Ok(pane)));
        assert_eq!(app.selected, Some(7));
        assert!(app.new_ws.is_none());
        assert_eq!(app.recents.first().map(String::as_str), Some("/Users/x/proyecto"));
    }

    /// A: si el daemon no puede crear el shell, el error se muestra y «Nuevo workspace» sigue abierto.
    #[test]
    fn error_al_abrir_carpeta_se_muestra() {
        let mut app = App { new_ws: Some(String::new()), ..Default::default() };
        let _ = update(&mut app, Message::WorkspaceOpened("/x".into(), Err("sin daemon".into())));
        assert_eq!(app.error.as_deref(), Some("sin daemon"));
        assert!(app.new_ws.is_some());
        assert!(app.recents.is_empty());
    }

    /// B: los atajos nuevos, y sus teclas con Cmd (Shift cambia el número de la tecla de letra).
    #[test]
    fn atajos_de_la_fase_b() {
        let cmd = keyboard::Modifiers::COMMAND;
        let shift = keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT;
        let k = |c: &str| Key::Character(c.into());
        assert!(matches!(shortcut(&k("t"), cmd), Some(Message::NewTab)));
        assert!(matches!(shortcut(&k("w"), cmd), Some(Message::CloseFocused)));
        assert!(matches!(shortcut(&k("U"), shift), Some(Message::NextAttention)));
        assert!(matches!(shortcut(&k("l"), shift), Some(Message::OpenLauncher)));
        assert!(matches!(shortcut(&k("3"), cmd), Some(Message::SelectWorkspaceAt(2))));
        assert!(matches!(shortcut(&k("0"), cmd), Some(Message::Zoom(0))), "Cmd+0 vuelve al tamaño de letra normal");
        assert!(shortcut(&k("9"), cmd).is_some());
    }

    /// ⌘⇧Enter maximiza el enfocado de un split: ocupa toda el área y se redimensiona a ella; enfocar otro panel
    /// (o pulsar de nuevo) devuelve el split. Con un solo panel no hace nada.
    #[test]
    fn maximizar_un_panel_del_split() {
        let mut app = App { window: (1280.0, 800.0), ..Default::default() };
        let mut a = pane_info(1, "a", Status::Idle);
        a.workspace = Some("r/main".into());
        let _ = update(&mut app, Message::Panes(Ok(vec![a.clone()])));
        let _ = update(&mut app, Message::Select(1));
        let _ = update(&mut app, Message::ToggleMaximize);
        assert_eq!(app.maximized, None, "un panel solo no se maximiza");
        let mut b = pane_info(2, "b", Status::Idle);
        b.workspace = Some("r/main".into());
        app.layouts.split(1, true, 2);
        let _ = update(&mut app, Message::Panes(Ok(vec![a, b])));
        let _ = update(&mut app, Message::Select(1));
        let mitad = pane_rect(&app, 1).unwrap();
        let _ = update(&mut app, Message::ToggleMaximize);
        assert_eq!(app.maximized, Some(1));
        assert_eq!(pane_rect(&app, 1), Some(terminal_area_for(&app)), "ocupa el área entera");
        assert!(pane_rect(&app, 2).is_none(), "el otro no se ve");
        assert_eq!(app.sizes.get(&1), Some(&grid_in(terminal_area_for(&app))), "y se redimensiona a ella");
        let _ = update(&mut app, Message::Select(2));
        assert_eq!(app.maximized, None, "enfocar otro devuelve el split");
        assert_eq!(pane_rect(&app, 1), Some(mitad));
    }

    /// Arrastrar el divisor de un split lo mueve y, al soltar, los paneles se redimensionan a su parte.
    #[test]
    fn arrastrar_el_divisor_de_un_split() {
        let mut app = App { window: (1280.0, 800.0), ..Default::default() };
        let mut a = pane_info(1, "a", Status::Idle);
        a.workspace = Some("r/main".into());
        let mut b = pane_info(2, "b", Status::Idle);
        b.workspace = Some("r/main".into());
        let _ = update(&mut app, Message::Panes(Ok(vec![a.clone()])));
        app.layouts.split(1, true, 2);
        let _ = update(&mut app, Message::Panes(Ok(vec![a, b])));
        let _ = update(&mut app, Message::Select(1));
        let area = terminal_area_for(&app);
        let mid = Point::new(area.x + area.w / 2.0 + 2.0, area.y + 100.0);
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::CursorMoved { position: mid })));
        assert!(app.hover_divider.is_some(), "el puntero está sobre el divisor");
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))));
        assert!(app.drag_divider.is_some());
        assert!(app.sel.is_none(), "arrastrar un divisor no empieza una selección");
        let to = Point::new(area.x + area.w * 0.3, area.y + 100.0);
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::CursorMoved { position: to })));
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))));
        assert!(app.drag_divider.is_none());
        let r1 = pane_rect(&app, 1).unwrap();
        assert!((r1.w - area.w * 0.3).abs() < 1.5, "el izquierdo ocupa el 30 %: {}", r1.w);
        assert_eq!(app.sizes.get(&1), Some(&grid_in(r1)), "y se redimensiona a su parte");
    }

    /// El zoom escala la letra, la línea y la celda en proporción; Cmd+= / Cmd+- / Cmd+0 lo manejan.
    #[test]
    fn zoom_de_la_letra() {
        assert_eq!(metrics_for(0), (FONT_SIZE, LINE_H, CELL_W));
        let (size, line, cell) = metrics_for(4);
        assert_eq!(size, FONT_SIZE + 4.0);
        assert!(line > LINE_H && cell > CELL_W);
        let cmd = keyboard::Modifiers::COMMAND;
        assert!(matches!(shortcut(&Key::Character("=".into()), cmd), Some(Message::Zoom(1))));
        assert!(matches!(shortcut(&Key::Character("+".into()), cmd | keyboard::Modifiers::SHIFT), Some(Message::Zoom(1))));
        assert!(matches!(shortcut(&Key::Character("-".into()), cmd), Some(Message::Zoom(-1))));
    }

    /// B: Cmd+1..9 selecciona el workspace n-ésimo en el orden de la barra lateral.
    #[test]
    fn cmd_numero_elige_workspace_en_orden_de_barra() {
        let mut a = pane_info(1, "a", Status::Idle);
        a.workspace = Some("repo/main".into());
        let mut b = pane_info(2, "b", Status::Idle);
        b.workspace = Some("otro/dev".into());
        let mut c = pane_info(3, "c", Status::Idle);
        c.workspace = Some("repo/main".into());
        let panes = vec![a, b, c];
        // El orden es el de la barra lateral: alfabético por workspace (otro/dev antes que repo/main).
        assert_eq!(workspace_at(&panes, &layout::Layouts::default(), 0), Some(2), "otro/dev va primero");
        assert_eq!(workspace_at(&panes, &layout::Layouts::default(), 1), Some(1), "repo/main, con su primer panel");
        assert_eq!(workspace_at(&panes, &layout::Layouts::default(), 2), None);
    }

    /// B: Cmd+Shift+U recorre los paneles que necesitan atención, dando la vuelta.
    #[test]
    fn siguiente_que_necesita_atencion_da_la_vuelta() {
        let mut bloq = pane_info(4, "bloq", Status::Blocked);
        bloq.workspace = None;
        let panes = vec![pane_info(1, "ok", Status::Idle), bloq, pane_info(3, "hecho", Status::Done), pane_info(5, "ok2", Status::Working)];
        // El panel 3 tiene «terminó» sin leer (unread); el 4 está bloqueado.
        let unread = |id: u32| id == 3;
        // En orden de la lista de paneles: el 4 (bloqueado) está antes que el 3 (sin leer).
        assert_eq!(next_attention(&panes, None, unread), Some(4));
        assert_eq!(next_attention(&panes, Some(4), unread), Some(3));
        assert_eq!(next_attention(&panes, Some(3), unread), Some(4), "da la vuelta");
        assert_eq!(next_attention(&panes[..1], None, unread), None, "nadie necesita atención");
    }

    /// B: un agente nuevo hereda el workspace y la carpeta del panel enfocado; su nombre no repite ninguno.
    #[test]
    fn lanzar_agente_hereda_el_contexto_del_enfocado() {
        let mut p = pane_info(1, "claude", Status::Idle);
        p.workspace = Some("repo/main".into());
        p.cwd = Some("/Users/x/repo".into());
        let app = App { panes: vec![p], selected: Some(1), ..Default::default() };
        match context_request(&app, launch::Agent::Claude) {
            galgo_proto::Request::Spawn { name, cmd, cwd, workspace, .. } => {
                assert_eq!(name, "claude-2", "claude ya existe");
                assert_eq!(cmd, vec!["claude"]);
                assert_eq!(cwd.as_deref(), Some("/Users/x/repo"));
                assert_eq!(workspace.as_deref(), Some("repo/main"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// B: Cmd+W sobre un panel parado no pregunta; sobre uno que trabaja, pide confirmación.
    #[test]
    fn cmd_w_confirma_solo_si_hay_agente_trabajando() {
        let mut app = App { panes: vec![pane_info(1, "p", Status::Working)], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::CloseFocused);
        assert!(app.confirm_close, "trabajando: pregunta");
        // M2: un panel parado puede estar ejecutando algo (shell, vim, un build): también pregunta.
        let mut app = App { panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::CloseFocused);
        assert!(app.confirm_close, "parado: también pregunta (M2)");
    }

    /// C: la paleta lista workspaces, paneles y acciones; la búsqueda los filtra y Enter ejecuta el elegido.
    #[test]
    fn paleta_busca_y_ejecuta() {
        let mut p = pane_info(4, "claude", Status::Working);
        p.workspace = Some("repo/main".into());
        let mut app = App { panes: vec![p], ..Default::default() };
        let _ = update(&mut app, Message::OpenPalette);
        assert_eq!(body_mode(&app), Body::Palette);
        let _ = update(&mut app, Message::PaletteQuery("panel claude".into()));
        // El panel aparece entre los resultados; el primero puede ser una acción que también casa.
        let index = palette_results(&app).iter().position(|i| i.label == "Panel · claude").expect("el panel casa");
        let _ = update(&mut app, Message::PaletteRun(index));
        assert!(app.palette.is_none(), "al ejecutar, la paleta se cierra");
        assert_eq!(app.selected, Some(4), "el panel elegido queda seleccionado");
    }

    /// C: las flechas dan la vuelta por los resultados; una búsqueda sin resultados no rompe nada.
    #[test]
    fn paleta_flechas_dan_la_vuelta_y_busqueda_vacia() {
        let mut app = App { palette: Some(String::new()), ..Default::default() };
        let total = palette_results(&app).len().min(PALETTE_VISIBLE);
        assert!(total >= 9, "al menos los comandos");
        let _ = update(&mut app, Message::PaletteMove(-1));
        assert_eq!(app.palette_cursor, total - 1, "da la vuelta dentro de lo visible (B4)");
        let _ = update(&mut app, Message::PaletteQuery("zzqx".into()));
        assert!(palette_results(&app).is_empty());
        let _ = update(&mut app, Message::PaletteMove(1));
        let _ = update(&mut app, Message::PaletteRun(0));
        assert!(app.palette.is_none());
    }

    /// C: Cmd+K y Cmd+Shift+P abren la paleta.
    #[test]
    fn atajos_de_la_paleta() {
        let k = Key::Character("k".into());
        assert!(matches!(shortcut(&k, keyboard::Modifiers::COMMAND), Some(Message::OpenPalette)));
        let p = Key::Character("P".into());
        assert!(matches!(shortcut(&p, keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT), Some(Message::OpenPalette)));
    }

    /// E: dividir pide un panel al lado del enfocado; al llegar, queda en la misma pestaña y con el foco.
    #[test]
    fn dividir_pone_el_panel_al_lado_y_le_da_el_foco() {
        let mut p1 = pane_info(1, "uno", Status::Idle);
        p1.workspace = Some("repo/main".into());
        let mut app = App { panes: vec![p1], selected: Some(1), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        let _ = update(&mut app, Message::SplitFocused(true));
        assert_eq!(app.pending_split.get("shell"), Some(&(1, true)));
        let mut p2 = pane_info(2, "shell", Status::Idle);
        p2.workspace = Some("repo/main".into());
        let json = serde_json::to_value(p2).unwrap();
        let _ = update(&mut app, Message::PaneLaunched(Ok(json)));
        assert!(app.pending_split.is_empty());
        let tab = app.layouts.active_tab("repo/main").expect("la pestaña del workspace");
        assert_eq!(tab.root.ids(), vec![1, 2], "lado a lado en la misma pestaña");
        assert_eq!(tab.focus, 2, "el panel nuevo tiene el foco");
        assert_eq!(app.layouts.by_key["repo/main"].tabs.len(), 1, "no crea una pestaña extra");
    }

    /// E: si el lanzamiento falla, el split pendiente se descarta: el siguiente panel no se coloca mal.
    #[test]
    fn un_split_fallido_no_deja_el_pendiente() {
        let mut app = App { panes: vec![pane_info(1, "uno", Status::Idle)], selected: Some(1), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        let _ = update(&mut app, Message::SplitFocused(false));
        let _ = update(&mut app, Message::LaunchFailed("shell".into(), "sin daemon".into()));
        assert!(app.pending_split.is_empty());
    }

    /// E: Cmd+Opt+flecha mueve el foco al split vecino; sin vecino en esa dirección, no se mueve.
    #[test]
    fn foco_por_direccion_entre_splits() {
        let mut app = App { panes: vec![pane_info(1, "izq", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        app.layouts.split(1, true, 2);
        app.panes.push(pane_info(2, "der", Status::Idle));
        app.layouts.sync(&pane_keys(&app));
        app.selected = Some(2);
        let _ = update(&mut app, Message::FocusDir(layout::Dir::Left));
        assert_eq!(app.selected, Some(1));
        let _ = update(&mut app, Message::FocusDir(layout::Dir::Left));
        assert_eq!(app.selected, Some(1), "no hay nada más a la izquierda");
    }

    /// E: con un solo panel, el origen de su cuadrícula es el de referencia; un split lo desplaza.
    #[test]
    fn geometria_del_panel_unico_y_del_split() {
        let mut app = App { panes: vec![pane_info(1, "uno", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        assert_eq!(focus_origin(&app), TERM_ORIGIN, "un panel sin split usa el origen de referencia");
        app.layouts.split(1, true, 2);
        app.panes.push(pane_info(2, "dos", Status::Idle));
        app.selected = Some(2);
        let (x, _) = focus_origin(&app);
        assert!(x > TERM_ORIGIN.0, "el panel de la derecha empieza más a la derecha");
        let (rows, cols) = grid_in(pane_rect(&app, 2).unwrap());
        assert!(cols < grid_for((1100.0, 700.0)).1, "y tiene menos columnas que la ventana entera");
        assert!(rows > 5);
    }

    /// E: las pantallas de los paneles visibles sin foco se sondean por turnos, y la respuesta se guarda.
    #[test]
    fn paneles_visibles_se_sondean_por_turnos() {
        let mut app = App { panes: vec![pane_info(1, "a", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        app.layouts.split(1, true, 2);
        app.panes.push(pane_info(2, "b", Status::Idle));
        app.layouts.sync(&pane_keys(&app));
        let first = next_other(&mut app);
        assert_eq!(first, Some(2), "el único sin foco");
        // Como en el tick: mientras su respuesta no llega, no se vuelve a pedir.
        app.others_loading.insert(2);
        assert_eq!(next_other(&mut app), None);
        let _ = update(&mut app, Message::OtherScreen(2, Ok(ScreenReply::Full(snap(&["hola"], 20)))));
        assert!(app.others.contains_key(&2));
        assert!(!app.others_loading.contains(&2));
        assert_eq!(app.others_gen.get(&2).copied(), Some(snap(&["hola"], 20).gen));
    }

    /// E: Cmd+] y Cmd+[ cambian de pestaña y el foco va a su panel.
    #[test]
    fn cambiar_de_pestana_lleva_el_foco() {
        let mut app = App { panes: vec![pane_info(1, "a", Status::Idle), pane_info(2, "b", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        app.layouts.focus(1);
        assert_eq!(app.layouts.by_key[""].tabs.len(), 2, "dos paneles sin workspace, dos pestañas");
        let _ = update(&mut app, Message::NextTab(1));
        assert_eq!(app.selected, Some(2));
        let _ = update(&mut app, Message::NextTab(1));
        assert_eq!(app.selected, Some(1), "da la vuelta");
        assert!(matches!(shortcut(&Key::Character("]".into()), keyboard::Modifiers::COMMAND), Some(Message::NextTab(1))));
    }

    /// r13 G1 (arranque): con tres paneles en un workspace, el seleccionado queda en la pestaña que se ve.
    #[test]
    fn r13_arranque_foco_visible() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        let panes: Vec<PaneInfo> = (1..=3)
            .map(|i| {
                let mut p = pane_info(i, &format!("p{i}"), Status::Idle);
                p.workspace = Some("w".into());
                p
            })
            .collect();
        let _ = update(&mut app, Message::Panes(Ok(panes)));
        let sel = app.selected.expect("hay selección");
        let visibles = app.layouts.active_tab("w").expect("pestaña activa").root.ids();
        assert!(visibles.contains(&sel), "seleccionado {sel}, visibles {visibles:?}");
    }

    /// r13 G1 (spawn externo): un panel que llega de fuera no cambia la vista ni el foco.
    #[test]
    fn r13_spawn_externo_no_roba_la_vista() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        let mut panes: Vec<PaneInfo> = (1..=3)
            .map(|i| {
                let mut p = pane_info(i, &format!("p{i}"), Status::Idle);
                p.workspace = Some("w".into());
                p
            })
            .collect();
        let _ = update(&mut app, Message::Panes(Ok(panes.clone())));
        let _ = update(&mut app, Message::Select(1));
        let mut externo = pane_info(4, "externo", Status::Idle);
        externo.workspace = Some("w".into());
        panes.push(externo);
        let _ = update(&mut app, Message::Panes(Ok(panes)));
        assert_eq!(app.selected, Some(1), "el foco no se mueve");
        let visibles = app.layouts.active_tab("w").expect("pestaña activa").root.ids();
        assert!(visibles.contains(&1), "el seleccionado sigue visible: {visibles:?}");
        assert!(!visibles.contains(&4), "el externo está en otra pestaña");
    }

    /// r13 M1: cerrar el panel 2 de un split en r/main no salta al panel de otro workspace que el daemon lista primero.
    #[test]
    fn r13_cerrar_split_no_salta_a_otro_workspace() {
        let mut otro = pane_info(3, "otro", Status::Idle);
        otro.workspace = Some("otro/main".into());
        let mut uno = pane_info(1, "uno", Status::Idle);
        uno.workspace = Some("r/main".into());
        let mut dos = pane_info(2, "dos", Status::Idle);
        dos.workspace = Some("r/main".into());
        let mut app = App { panes: vec![otro, uno, dos], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        app.layouts.split(1, true, 2);
        app.layouts.sync(&pane_keys(&app));
        let _ = update(&mut app, Message::Select(2));
        let _ = update(&mut app, Message::ConfirmClose);
        app.panes.retain(|p| p.id != 2);
        let restantes = app.panes.clone();
        let _ = update(&mut app, Message::Panes(Ok(restantes)));
        assert_eq!(app.selected, Some(1), "el hermano del split, no el primero de la lista");
        assert_eq!(app.layouts.key_of(1).as_deref(), Some("r/main"));
    }

    /// r13 M2: Cmd+W sobre un agente bloqueado o en espera pide confirmación; sobre uno terminado, no.
    /// Con el formulario abierto, Cmd+W cierra el formulario y no toca el panel de detrás.
    #[test]
    fn r13_cmdw_con_formulario_y_estados() {
        let cmd = keyboard::Modifiers::COMMAND;
        let w = Key::Character("w".into());
        let mut app = App { panes: vec![pane_info(1, "claude", Status::Blocked)], selected: Some(1), form: Some(NewPane::default()), ..Default::default() };
        assert!(matches!(shortcut_in_state(&app, &w, cmd), Some(Message::CancelForm)));
        app.form = None;
        let _ = update(&mut app, Message::CloseFocused);
        assert!(app.confirm_close, "bloqueado: pide confirmación");
        let mut app = App { panes: vec![pane_info(2, "shell", Status::Idle)], selected: Some(2), ..Default::default() };
        let _ = update(&mut app, Message::CloseFocused);
        assert!(app.confirm_close, "un shell en Idle puede estar ejecutando algo: pide confirmación");
        let mut app = App { panes: vec![pane_info(3, "viejo", Status::Done)], selected: Some(3), ..Default::default() };
        let _ = update(&mut app, Message::CloseFocused);
        assert!(!app.confirm_close, "terminado: no pregunta");
    }

    /// r13 M3: al dividir, el panel original se redimensiona a su mitad (no se queda con toda la ventana).
    #[test]
    fn r13_split_no_redimensiona_el_original() {
        let mut uno = pane_info(1, "uno", Status::Idle);
        uno.workspace = Some("r/main".into());
        let mut app = App { panes: vec![uno], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        let _ = layout_resizes(&mut app);
        let _ = update(&mut app, Message::SplitFocused(true));
        let mut dos = pane_info(2, "shell", Status::Idle);
        dos.workspace = Some("r/main".into());
        let _ = update(&mut app, Message::PaneLaunched(Ok(serde_json::to_value(dos).unwrap())));
        let envios = layout_resizes(&mut app);
        let mitad = grid_in(layout::halves(terminal_area_for(&app), true).0);
        assert_eq!(app.sizes.get(&1), Some(&mitad), "el original cabe en su mitad");
        assert!(envios.iter().any(|r| matches!(r, Request::Resize { pane, .. } if pane == "1")), "{envios:?}");
    }

    /// r13 M4: la rueda y el clic sobre el panel vecino no actúan sobre el enfocado; con la paleta abierta, el clic no
    /// llega al terminal.
    #[test]
    fn r13_rueda_y_clic_no_van_al_panel_de_al_lado() {
        let mut uno = pane_info(1, "uno", Status::Idle);
        uno.workspace = Some("r/main".into());
        let mut dos = pane_info(2, "dos", Status::Idle);
        dos.workspace = Some("r/main".into());
        let mut app = App { panes: vec![uno, dos], selected: Some(1), window: (1100.0, 700.0), snap: Some(snap(&["x"; 40], 120)), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        app.layouts.split(1, true, 2);
        app.layouts.sync(&pane_keys(&app));
        let (_, derecha) = layout::halves(terminal_area_for(&app), true);
        let punto = Point::new(derecha.x + derecha.w / 2.0, derecha.y + derecha.h / 2.0);
        app.pointer = Some(punto);
        assert_eq!(leaf_at(&app, punto), Some(2));
        assert!(!pointer_in_grid(&app), "la rueda sobre el vecino no desplaza al enfocado");
        app.palette = Some(String::new());
        let _ = update(&mut app, Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))));
        assert_eq!(app.selected, Some(1), "con la paleta abierta el clic no cambia el foco");
    }

    /// r13 M5: Cmd+1 vuelve a la pestaña activa del workspace, no a su primer panel.
    #[test]
    fn r13_cmd1_vuelve_a_la_primera_pestana() {
        let panes: Vec<PaneInfo> = (1..=3)
            .map(|i| {
                let mut p = pane_info(i, &format!("p{i}"), Status::Idle);
                p.workspace = Some("w".into());
                p
            })
            .collect();
        let mut app = App { panes, window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        let _ = update(&mut app, Message::Select(3));
        assert_eq!(app.layouts.active_tab("w").unwrap().focus, 3);
        let _ = update(&mut app, Message::SelectWorkspaceAt(0));
        assert_eq!(app.selected, Some(3), "se queda en la pestaña activa, no en el panel 1");
    }

    /// r13 B1+B2: dos splits en vuelo reciben nombres distintos, y cada respuesta cae en su split aunque lleguen al revés.
    #[test]
    fn r13_dos_splits_en_vuelo_se_casan_por_nombre() {
        let mut uno = pane_info(1, "uno", Status::Idle);
        uno.workspace = Some("r/main".into());
        let mut app = App { panes: vec![uno], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        let _ = update(&mut app, Message::SplitFocused(true));
        let _ = update(&mut app, Message::SplitFocused(false));
        assert_eq!(app.inflight, vec!["shell".to_string(), "shell-2".to_string()], "nombres distintos aunque estén en vuelo");
        let mut abajo = pane_info(3, "shell-2", Status::Idle);
        abajo.workspace = Some("r/main".into());
        let _ = update(&mut app, Message::PaneLaunched(Ok(serde_json::to_value(abajo).unwrap())));
        let mut derecha = pane_info(2, "shell", Status::Idle);
        derecha.workspace = Some("r/main".into());
        let _ = update(&mut app, Message::PaneLaunched(Ok(serde_json::to_value(derecha).unwrap())));
        let area = terminal_area_for(&app);
        let rects = app.layouts.active_tab("r/main").unwrap().root.rects(area);
        let r = |id: u32| rects.iter().find(|(p, _)| *p == id).unwrap().1;
        assert!(r(2).x > r(1).x && (r(2).y - r(1).y).abs() < 1.0, "2 a la derecha de 1");
        assert!(r(3).y > r(1).y && (r(3).x - r(1).x).abs() < 1.0, "3 debajo de 1");
        assert!(app.inflight.is_empty() && app.pending_split.is_empty());
    }

    /// r13 B2: con un lanzamiento en vuelo, el nombre siguiente no coincide con él.
    #[test]
    fn r13_nombres_en_vuelo_no_se_repiten() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        let _ = update(&mut app, Message::NewTab);
        let _ = update(&mut app, Message::NewTab);
        assert_eq!(app.inflight, vec!["shell".to_string(), "shell-2".to_string()]);
    }

    /// r13 B6: al cambiar de panel, la pantalla del que pierde el foco se conserva y se muestra al volver.
    #[test]
    fn r13_cambiar_de_foco_no_deja_la_pantalla_vacia() {
        let mut app = App { panes: vec![pane_info(1, "a", Status::Idle), pane_info(2, "b", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), snap: Some(snap(&["uno"], 8)), ..Default::default() };
        app.layouts.sync(&pane_keys(&app));
        let _ = update(&mut app, Message::Select(2));
        assert!(app.others.contains_key(&1), "la pantalla del 1 se guarda al perder el foco");
        assert!(app.snap.is_none(), "el panel 2 no tiene pantalla todavía: no se muestra la del 1");
        let _ = update(&mut app, Message::Select(1));
        assert!(shown(&app).is_some(), "al volver al 1 se ve su última pantalla, no «Esperando salida…»");
    }

    /// r13 B7: Cmd+Shift+U sigue el orden de la barra lateral, no el de la lista del daemon.
    #[test]
    fn r13_siguiente_atencion_sigue_el_orden_de_la_barra() {
        let mut z = pane_info(1, "z", Status::Blocked);
        z.workspace = Some("z/main".into());
        let mut a = pane_info(2, "a", Status::Blocked);
        a.workspace = Some("a/main".into());
        let mut app = App { panes: vec![z, a], window: (1100.0, 700.0), ..Default::default() };
        let _ = update(&mut app, Message::NextAttention);
        assert_eq!(app.selected, Some(2), "la barra pone a/main antes que z/main");
    }

    /// r13 B4: el resaltado de la paleta no pasa de los resultados que se dibujan.
    #[test]
    fn r13_paleta_resaltado_dentro_de_lo_visible() {
        let mut app = App { palette: Some(String::new()), ..Default::default() };
        for _ in 0..40 {
            let _ = update(&mut app, Message::PaletteMove(1));
        }
        assert!(app.palette_cursor < PALETTE_VISIBLE, "cursor {}", app.palette_cursor);
        assert!(palette_items(&App::default()).len() >= 15, "la paleta tiene los comandos de división, pestañas y lanzadores");
    }

    /// r13 B10: una carpeta que no es UTF-8 se rechaza con un aviso, no se abre con otro nombre.
    #[test]
    fn r13_ruta_no_utf8_se_rechaza() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        let _ = update(&mut app, Message::FolderRejected);
        assert!(app.error.as_deref().is_some_and(|e| e.contains("UTF-8")));
        assert!(app.recents.is_empty());
    }

    /// r13 B5: el error va en la barra de estado, de altura fija: mostrarlo no cambia el área de los paneles (ni
    /// pide un `Resize`).
    #[test]
    fn r13_error_no_mueve_el_terminal() {
        let mut app = App { window: (1100.0, 700.0), panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        let sin = terminal_area_for(&app);
        app.error = Some("no se envió".into());
        assert_eq!(terminal_area_for(&app), sin);
    }
}

/// Render headless: dibuja `view(app)` a un pixmap con el renderer de tiny-skia y guarda un PNG. No abre ventanas ni
/// toca el escritorio. Se usa con `cargo test render_capturas -- --ignored` y `GALGO_UX_DIR`.
#[cfg(test)]
mod render {
    use super::*;
    use iced_runtime::user_interface::{Cache, UserInterface};

    fn pane_info(id: u32, name: &str, status: Status) -> PaneInfo {
        PaneInfo { id, name: name.into(), status, manager: None, cmd: vec![], unacked: 0, oldest_unacked_secs: None, orphan: false, waiting_on: None, waiting_secs: None, workspace: None, cwd: None, usage: None }
    }

    fn snap(lines: &[&str], cols: u16) -> Snapshot {
        let run = |t: &str| Run { t: t.into(), fg: galgo_proto::Color::Default, bg: galgo_proto::Color::Default, bold: false, italic: false, underline: false, inverse: false };
        Snapshot { gen: 0, rows: lines.len() as u16, cols, cursor: (0, 0), cursor_hidden: true, lines: lines.iter().map(|l| vec![run(l)]).collect(), scrollback: 0, scrollback_max: 0, bracketed_paste: false, wrapped: vec![] }
    }

    pub fn render(app: &App, (w, h): (u32, u32), path: &std::path::Path) {
        // El `Element` de la vista usa el renderer de respaldo de iced; el de tiny-skia va en su variante Secondary.
        let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(iced::Font::DEFAULT, iced::Pixels(FONT_SIZE)));
        let viewport = iced_graphics::Viewport::with_physical_size(iced::Size::new(w, h), 1.0);
        let size = iced::Size::new(w as f32, h as f32);
        let mut ui = UserInterface::build(view(app), size, Cache::default(), &mut renderer);
        let style = iced_core::renderer::Style { text_color: TEXT };
        ui.draw(&mut renderer, &theme::app(), &style, iced::mouse::Cursor::Unavailable);
        let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");
        let mut mask = tiny_skia::Mask::new(w, h).expect("mascara");
        let damage = [iced::Rectangle::with_size(size)];
        if let iced::Renderer::Secondary(r) = &mut renderer {
            r.draw::<&str>(&mut pixmap.as_mut(), &mut mask, &viewport, &damage, BG, &[]);
        }
        // El buffer de tiny-skia tiene R y B en el orden contrario al de save_png: se intercambian antes de guardar.
        for px in pixmap.data_mut().chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        pixmap.save_png(path).expect("guardar png");
    }

    fn colored(parts: &[(&str, galgo_proto::Color, bool)]) -> Vec<Run> {
        parts.iter().map(|(t, fg, bold)| Run { t: (*t).into(), fg: *fg, bg: galgo_proto::Color::Default, bold: *bold, italic: false, underline: false, inverse: false }).collect()
    }

    /// Pantalla de ejemplo con colores, como la de un agente trabajando.
    fn rich_snap(cols: u16) -> Snapshot {
        use galgo_proto::Color::{Default as D, Idx};
        let lines = vec![
            colored(&[("╭─", Idx(8), false), (" claude ", Idx(13), true), ("· sonnet · ~/Development/nexo", Idx(8), false)]),
            colored(&[("│ ", Idx(8), false), ("> ", Idx(12), true), ("arregla el test de recientes y corre cargo test", D, false)]),
            colored(&[("╰─", Idx(8), false)]),
            colored(&[("", D, false)]),
            colored(&[("● ", Idx(10), false), ("Read", D, true), ("(crates/galgo-gui/src/recents.rs)", Idx(8), false)]),
            colored(&[("  ⎿ ", Idx(8), false), ("Read 92 lines", Idx(8), false)]),
            colored(&[("● ", Idx(10), false), ("Bash", D, true), ("(cargo test -p galgo-gui)", Idx(8), false)]),
            colored(&[("  ⎿ ", Idx(8), false), ("   Compiling galgo-gui v0.1.0", Idx(2), false)]),
            colored(&[("     ", D, false), ("test recents::tests::la_lista_se_recorta ... ", D, false), ("ok", Idx(2), true)]),
            colored(&[("     ", D, false), ("test result: ", D, false), ("ok", Idx(2), true), (". 110 passed; 0 failed", D, false)]),
            colored(&[("", D, false)]),
            colored(&[("✻ ", Idx(3), false), ("Pensando… ", Idx(3), false), ("(12s · ↓ 1.2k tokens · esc para interrumpir)", Idx(8), false)]),
        ];
        let rows = 40u16;
        let mut all = lines;
        while all.len() < rows as usize { all.push(vec![]); }
        Snapshot { gen: 1, rows, cols, cursor: (1, 52), cursor_hidden: false, lines: all, scrollback: 0, scrollback_max: 0, bracketed_paste: false, wrapped: vec![] }
    }

    fn shell_snap(cols: u16) -> Snapshot {
        use galgo_proto::Color::{Default as D, Idx};
        let lines = vec![
            colored(&[("~/Development/nexo ", Idx(4), true), ("main ", Idx(5), false), ("❯ ", Idx(2), true), ("git status --short", D, false)]),
            colored(&[(" M ", Idx(1), false), ("crates/galgo-gui/src/main.rs", D, false)]),
            colored(&[("?? ", Idx(1), false), ("crates/galgo-gui/src/chrome.rs", D, false)]),
            colored(&[("~/Development/nexo ", Idx(4), true), ("main ", Idx(5), false), ("❯ ", Idx(2), true)]),
        ];
        let rows = 40u16;
        let mut all = lines;
        while all.len() < rows as usize { all.push(vec![]); }
        Snapshot { gen: 1, rows, cols, cursor: (3, 23), cursor_hidden: false, lines: all, scrollback: 0, scrollback_max: 0, bracketed_paste: false, wrapped: vec![] }
    }

    fn usage(agent: &str, ctx: f32, cost: f64) -> galgo_usage::PaneUsage {
        galgo_usage::PaneUsage { agent: agent.into(), model: Some("claude-sonnet-4-5".into()), input: 120_000, cache_read: 900_000, cache_write: 40_000, cache_write_5m: 40_000, cache_write_1h: 0, output: 18_000, reasoning: 0, cost_usd: Some(cost), cost_source: galgo_usage::CostSource::Unknown, context_tokens: Some((ctx * 2000.0) as u64), context_window: Some(200_000), context_pct: Some(ctx), updated_secs: 4, partial: false }
    }

    /// Estado rico: dos proyectos, un workspace con split, atención, uso y cuotas.
    fn scene() -> App {
        let mut app = App { window: (1280.0, 800.0), focused: true, ..Default::default() };
        let mut ws = |id: u32, name: &str, ws: &str, st: Status| {
            let mut p = pane_info(id, name, st);
            p.workspace = Some(ws.into());
            p.cwd = Some(format!("{}/Development/nexo", home_dir()));
            p
        };
        let mut c = ws(1, "claude", "nexo/main", Status::Working);
        c.usage = Some(usage("claude", 34.0, 1.42));
        let s = ws(2, "shell", "nexo/main", Status::Idle);
        let mut w = ws(3, "worker-tests", "nexo/main", Status::Blocked);
        w.manager = Some("claude".into());
        let mut x = ws(4, "codex", "inventia/cotizacion", Status::Done);
        x.usage = Some(usage("codex", 71.0, 0.38));
        let y = ws(5, "shell", "inventia/cotizacion", Status::Idle);
        let _ = &mut ws;
        app.panes = vec![c, s, w, x, y];
        app.layouts.sync(&pane_keys(&app));
        // nexo/main: pestaña 1 = claude | shell (split), pestaña 2 = worker-tests.
        let mut l = layout::Layouts::default();
        l.new_tab("nexo/main", 1);
        l.split(1, true, 2);
        l.new_tab("nexo/main", 3);
        l.new_tab("inventia/cotizacion", 4);
        l.new_tab("inventia/cotizacion", 5);
        app.layouts = l;
        app.layouts.focus(1);
        app.selected = Some(1);
        app.snap = Some(rich_snap(70));
        app.others.insert(2, shell_snap(70));
        app.attention.observe(&app.panes, app.selected);
        app.limits = vec![galgo_usage::Limits { agent: "claude".into(), plan: Some("max".into()), windows: vec![galgo_usage::LimitWindow { minutes: 300, used_pct: 42.0, resets_at: Some(now_secs() + 7200) }, galgo_usage::LimitWindow { minutes: 10080, used_pct: 76.0, resets_at: Some(now_secs() + 3 * 86400) }], observed_at: now_secs() - 30 }];
        app
    }

    #[test]
    #[ignore]
    fn render_capturas() {
        let dir = std::path::PathBuf::from(std::env::var("GALGO_UX_DIR").unwrap_or_else(|_| "/tmp/galgo-ux".into()));
        std::fs::create_dir_all(&dir).unwrap();
        let size = (1280, 800);
        let mut welcome = App { window: (1280.0, 800.0), recents: vec![format!("{}/Development/nexo", home_dir()), format!("{}/Development/inventia", home_dir()), "/tmp/demo".into()], ..Default::default() };
        render(&welcome, size, &dir.join("r-bienvenida.png"));
        welcome.new_ws = Some(String::new());
        render(&welcome, size, &dir.join("r-nuevo-workspace.png"));
        let app = scene();
        render(&app, size, &dir.join("r-paneles.png"));
        let mut palette = scene();
        palette.palette = Some("cla".into());
        render(&palette, size, &dir.join("r-paleta.png"));
        let mut lanz = scene();
        lanz.launcher = true;
        render(&lanz, size, &dir.join("r-lanzador.png"));
        let mut form = scene();
        form.form = Some(NewPane::default());
        render(&form, size, &dir.join("r-formulario.png"));
        let mut close = scene();
        close.confirm_close = true;
        render(&close, size, &dir.join("r-cerrar.png"));
        let mut sel = scene();
        sel.sel = Some(Selection { anchor: (4, 2), head: (5, 20) });
        sel.layouts.set_ratio("nexo/main", &[], 0.62);
        sel.hover_divider = Some((Vec::new(), true));
        render(&sel, size, &dir.join("r-seleccion-y-divisor.png"));
        let mut max = scene();
        max.maximized = Some(1);
        render(&max, size, &dir.join("r-maximizado.png"));
        let mut err = scene();
        err.error = Some("no se envió: el daemon no responde".into());
        render(&err, size, &dir.join("r-error.png"));
    }
}

#[cfg(test)]
impl App {
    /// Copia para renderizar variantes del mismo estado (la app no implementa Clone).
    fn clone_for_render(&self) -> App {
        App {
            panes: self.panes.clone(),
            selected: self.selected,
            snap: self.snap.clone(),
            window: self.window,
            layouts: self.layouts.clone(),
            ..Default::default()
        }
    }
}
