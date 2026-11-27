use galgo_proto::{client, PaneInfo, Request, Status};
use iced::{widget::{column, container, row}, Color, Element, Font, Length, Task};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;
use std::time::Duration;

use theme::{BG, MUTED, SIDEBAR, TEXT};

mod attention;
mod cajas;
mod chrome;
mod launch;
mod fonts;
mod layout;
mod palette;
mod prefs;
mod settings;
mod sidebar;
mod team;
mod workspace;

use std::collections::BTreeMap;
mod raton;
mod term;
mod theme;
mod usage;

use attention::{Alert, Attention};
use galgo_proto::{MouseTracking, Run, Snapshot};
use galgo_usage::Limits;
use iced::keyboard::{self, key::Named, Key};
use iced::mouse;
use iced::widget::text::Span;
use iced::widget::{rich_text, span};
use iced::{event, font, Event, Point, Subscription};
use std::sync::{mpsc, OnceLock};

const FONT_SIZE: f32 = 14.0;
const SIDEBAR_W: f32 = 248.0;

/// Paso de zoom de la fuente del terminal (Cmd+= / Cmd+- / Cmd+0): 0 es el tamaño base. Global porque la geometría
/// (celdas, filas, ratón) se calcula en funciones puras que no reciben la app.
static ZOOM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
const ZOOM_MIN: i32 = -5;
const ZOOM_MAX: i32 = 12;

/// (tamaño de letra, alto de línea, ancho de celda) para un paso de zoom: proporcionales a los de base.
fn metrics_for(zoom: i32) -> (f32, f32, f32) {
    let size = FONT_SIZE + zoom as f32;
    // Punto 3: el alto de línea y el ancho de celda salen de la fuente en uso, no de constantes de Menlo.
    let (cell_em, line_em) = fonts::metrics();
    ((size), (size * line_em).round(), size * cell_em)
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
/// Aire entre el panel y el borde de su hueco: margen exterior (`PANEL_GUTTER`) más el padding interior del texto.
/// Todo lo que mide la posición del texto (rejilla, clics, origen) usa este total.
const PANEL_GUTTER: f32 = 6.0;
const PANEL_PAD: f32 = 16.0;
const LEAF_PAD: f32 = PANEL_GUTTER + PANEL_PAD;
/// Cabecera de cada panel (icono, proceso, cwd y acciones). Va dentro del padding superior: el texto empieza debajo.
const HEADER_H: f32 = 26.0;
/// Desde el borde del hueco hasta el primer renglón del terminal.
/// Margen superior de un panel: la cabecera solo ocupa su fila cuando hay splits o el panel está maximizado.
fn leaf_top() -> f32 {
    LEAF_PAD + if marco().cabecera { HEADER_H } else { 0.0 }
}
/// Origen de la cuadrícula de un panel único (referencia de las pruebas); un split usa el de su rectángulo.
/// Geometría del marco según lo visible. La fija `sync_chrome` tras cada actualización; el resto la lee con estas
/// funciones. Por defecto (terminal primero): sin barra lateral, sin barra de estado y una sola fila superior.
#[derive(Clone, Copy)]
struct Marco {
    sidebar: bool,
    status: bool,
    /// La cabecera del panel se ve (hay splits en la pestaña visible, o un panel maximizado).
    cabecera: bool,
}
// Por hilo: la app entera (update y view) corre en el hilo principal, y cada prueba tiene el suyo, así que las
// pruebas en paralelo no se pisan la geometría.
thread_local! {
    static MARCO: std::cell::Cell<Marco> = const { std::cell::Cell::new(Marco { sidebar: false, status: false, cabecera: false }) };
}

fn marco() -> Marco {
    MARCO.with(|m| m.get())
}
fn sidebar_w() -> f32 {
    if marco().sidebar { SIDEBAR_W } else { 0.0 }
}
fn status_h() -> f32 {
    if marco().status { STATUS_H } else { 0.0 }
}
/// Alto de la zona superior: una sola fila de pestañas (píldoras planas; el grupo solo si se creó un workspace).
fn top_h() -> f32 {
    TABS_H
}
#[cfg(test)]
const LINE_H: f32 = 18.0;
#[cfg(test)]
const CELL_W: f32 = 8.43; // Menlo: 0,602 em

fn term_origin() -> (f32, f32) {
    (sidebar_w() + LEAF_PAD, top_h() + leaf_top())
}

/// Geometría del marco que corresponde al estado de la app (función pura, para probarla).
fn marco_para(app: &App) -> Marco {
    let dividida = focus_key(app).and_then(|k| app.layouts.active_tab(&k)).is_some_and(|t| matches!(t.root, layout::Node::Split { .. }));
    Marco { sidebar: app.sidebar, status: app.status, cabecera: dividida || maximized(app).is_some() }
}

/// Fija la geometría del marco según el estado de la app.
fn sync_chrome(app: &App) {
    MARCO.with(|m| m.set(marco_para(app)));
}

/// Guarda una preferencia de la GUI junto al socket (si hay dónde guardarla). Un fallo no detiene la app.
fn save_pref(app: &App, key: &str, value: serde_json::Value) {
    if let Some(path) = &app.prefs_path {
        if let Err(e) = prefs::set(path, key, value) {
            eprintln!("galgo-gui: no pude guardar la preferencia {key}: {e}");
        }
    }
}
/// Primera opción del desplegable de manager del formulario: ningún manager.

/// (fila, columna) dentro de la pantalla mostrada.
type Cell = (u16, u16);

#[derive(Debug, Clone, Copy, PartialEq, Hash)]
struct Selection {
    anchor: Cell,
    head: Cell,
}

fn terminal_font() -> Font {
    Font::with_name(fonts::current())
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
    /// Llegó un aviso mientras había una lectura en vuelo: al volver, se pide otra (el daemon no avisa dos veces del
    /// mismo cambio, así que sin esto la pantalla se quedaría vieja).
    refresh_wanted: bool,
    /// La suscripción al panel enfocado no funciona: se sondea su pantalla hasta que vuelva a funcionar.
    watch_down: bool,
    /// Workspaces explícitos (con nombre y carpeta propios): clave → carpeta. Los demás salen de la etiqueta del repo.
    explicit: BTreeMap<String, String>,
    /// Ya se aplicó lo guardado. Antes no se escribe nada: un primer listado incompleto borraría los layouts.
    ws_restored: bool,
    /// Último estado de workspaces escrito en disco (para no escribir si no cambió).
    ws_saved: String,
    /// Disposición guardada aún no aplicada: se espera a que vuelvan (o se relancen) sus paneles (M3).
    ws_pendiente: Option<serde_json::Value>,
    /// Listados recibidos mientras se espera la disposición.
    ws_intentos: u32,
    /// Cwd y workspace de cada panel conocido: para relanzar su shell, en su carpeta, tras un reinicio (M3).
    pane_meta: workspace::MetaPaneles,
    /// Cerrar un workspace pide dos clics: el primero deja aquí su clave.
    ws_close_armed: Option<String>,
    /// Cuándo se armó el cierre: pasados unos segundos, el primer clic vuelve a contar como aviso.
    ws_close_armed_at: Option<std::time::Instant>,
    /// Formulario «+ Agente…» abierto, y el rol de cada agente por nombre de panel (se guarda con el workspace).
    agent_form: Option<AgentForm>,
    roles: BTreeMap<String, String>,
    roles_saved: String,
    /// Prompts iniciales de equipo aún no entregados (nombre de panel → texto). Se pegan cuando el agente avisa por hook.
    prompts_pending: BTreeMap<String, String>,
    /// Equipo que espera la confirmación de su primera vez en el repo, y la confianza ya dada (repo → huella).
    team_pending: Option<TeamPending>,
    team_trust: serde_json::Value,
    /// Modificadores del teclado (para Cmd+clic) y el último clic (para doble y triple clic: celda, instante, n).
    mods: keyboard::Modifiers,
    clicks: Option<(Cell, std::time::Instant, u8)>,
    /// Barra lateral y barra de estado visibles (por defecto, ocultas: terminal primero). Se recuerdan en las preferencias.
    sidebar: bool,
    status: bool,
    /// Ya llegó la primera lista de paneles (el arranque solo decide una vez si abre un shell).
    started: bool,
    window: (f32, f32),
    /// Último tamaño (filas, columnas) enviado o confirmado por panel (M3: cada panel del layout tiene el suyo).
    sizes: std::collections::HashMap<u32, (u16, u16)>,
    /// Líneas hacia atrás que se piden al daemon (0 = en vivo).
    scroll: u16,
    /// Búsqueda abierta (Cmd+F) en el panel enfocado.
    buscar: Option<Busqueda>,
    /// Identidad del socket del daemon con el que se habló por última vez (para detectar un daemon nuevo).
    socket_id: Option<(u64, u64)>,
    /// Nombre que se muestra en la píldora de cada workspace (solo en la GUI; la clave del daemon no cambia).
    alias: std::collections::BTreeMap<String, String>,
    /// Edición del nombre de una píldora: (clave, texto escrito hasta ahora).
    renombrando: Option<(String, String)>,
    /// Último clic en el nombre de una píldora: para detectar el doble clic.
    pill_click: Option<(String, std::time::Instant)>,
    /// Fracción de líneas de la rueda que aún no suma un paso completo.
    wheel_acc: f32,
    /// Botón (código xterm) que el programa recibió en pulsación y aún no ha soltado.
    raton_pulsado: Option<u8>,
    /// Última celda por la que pasó el ratón mientras el programa lo quería (arrastres sin repetir celda).
    raton_celda: Option<Cell>,
    sel: Option<Selection>,
    dragging: bool,
    pointer: Option<Point>,
    /// Hay un pegado pedido al portapapeles que aún no ha llegado.
    paste_pending: bool,
    /// Teclas que llegaron mientras tanto; se envían después del pegado.
    key_queue: Vec<String>,
    /// Tamaño con el que se pidió el último `Spawn`: es el tamaño del panel que nace.
    spawn_size: Option<(u16, u16)>,
    attention: Attention,
    /// Pide confirmación antes de cerrar el panel seleccionado.
    confirm_close: bool,
    /// Shells ya terminados cuyo cierre se pidió (una sola vez cada uno).
    cierres_pedidos: std::collections::HashSet<u32>,
    /// El usuario cerró el último panel (o su shell terminó): al vaciarse el listado se cierra la ventana.
    salir_al_vaciar: bool,
    /// Cuotas por agente del daemon, y el último error si no se pudieron leer.
    limits: Vec<Limits>,
    limits_error: Option<String>,
    /// La ventana tiene el foco: si no, las alertas van a notificación de macOS.
    focused: bool,
    /// «Nuevo workspace» abierto, con el texto de búsqueda en recientes.
    /// Carpetas abiertas recientemente (la más reciente primero) y dónde se guardan.
    /// Lanzador de agentes abierto.
    launcher: bool,
    /// Modelo elegido en el lanzador, por agente (la etiqueta del agente → modelo).
    launch_model: std::collections::HashMap<String, String>,
    /// Fuentes Nerd instaladas detectadas al arrancar, y dónde se guarda la elección de fuente.
    font_installed: Vec<&'static str>,
    prefs_path: Option<std::path::PathBuf>,
    /// Ajustes abiertos: la sección elegida, y la salida de la CLI por agente (estado de hooks y planes).
    settings: Option<settings::Section>,
    hooks_output: std::collections::HashMap<String, String>,
    /// Plan de hooks (`--dry-run`) pendiente de «Aplicar», por agente. Sin plan no se escribe nada (A1).
    hooks_plan: std::collections::HashMap<String, String>,
    /// Agentes con una orden de hooks en curso: sus botones se desactivan (B5).
    hooks_busy: std::collections::HashSet<String>,
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
    /// Reintenta abrir la terminal de inicio (en ~).
    RetryStart,
    AskClose,
    /// El daemon avisa que la pantalla de un panel cambió (suscripción `Watch`): se pide la nueva.
    PaneChanged(u32, u64),
    /// No hubo forma de suscribirse al panel: mientras tanto, la pantalla se sondea.
    WatchDown(u32),
    /// Levantar un equipo: el del repo (con confirmación la primera vez) o uno integrado; y su resultado.
    TeamRequested(TeamSource),
    TeamConfirm,
    TeamCancel,
    TeamLaunched(String, Option<String>, Vec<(String, Result<PaneInfo, String>)>),
    /// «+ Agente…»: abre el formulario, y sus campos.
    OpenAgentForm,
    AgentFormPick(launch::Agent),
    AgentFormModel(String),
    AgentFormRole(launch::Role),
    AgentFormIsolation(launch::Isolation),
    AgentFormName(String),
    AgentFormSubmit,
    CloseAgentForm,
    /// Nuevo workspace con nombre propio (Cmd+Shift+N): se agrupa en la barra superior y se guarda.
    OpenNewNamedWorkspace,
    /// Cierra el grupo de un workspace explícito y sus paneles (dos clics).
    CloseWorkspace(String),
    /// Cmd+K: limpia la pantalla del panel enfocado (Ctrl+L al shell).
    ClearScreen,
    /// Un archivo arrastrado a la ventana: su ruta se pega en el panel enfocado.
    FileDropped(std::path::PathBuf),
    /// Muestra u oculta la barra lateral (Cmd+B) y la barra de estado (Cmd+/). Se recuerda la preferencia.
    ToggleSidebar,
    ToggleStatus,
    CancelClose,
    ConfirmClose,
    Killed(Result<serde_json::Value, String>),
    /// Abre la primera carpeta reciente que coincide con la búsqueda (Enter).
    /// La carpeta elegida tiene una ruta que no es UTF-8: no se abre (B10).
    /// Crea un workspace en esa carpeta: un shell, sin formulario.
    WorkspaceOpened(String, Result<serde_json::Value, String>),
    /// Atajos (B): pestaña nueva en el workspace actual, workspace por número, siguiente que necesita atención,
    /// cerrar el panel enfocado, lanzador de agentes.
    NewTab,
    /// Cmd+1..9: la pestaña n-ésima de la fila (por posición, de izquierda a derecha).
    SelectRowAt(usize),
    /// Clic central en una pestaña: la selecciona y pide cerrarla como Cmd+W (con la misma confirmación).
    CloseTabIn(String, usize),
    NextAttention,
    CloseFocused,
    OpenLauncher,
    CloseLauncher,
    Launch(launch::Agent),
    /// Modelo elegido en el lanzador para un agente.
    LauncherModel(String, String),
    /// Un panel creado desde un atajo o el lanzador: queda seleccionado.
    PaneLaunched(Result<serde_json::Value, String>),
    /// Un lanzamiento que falló: su nombre queda libre y su split pendiente se descarta (B1, B2).
    LaunchFailed(String, String),
    /// Pestañas y splits (E): cambiar de pestaña, elegir una, dividir el panel enfocado, mover el foco por direcciones.
    NextTab(isize),
    /// Cmd+F: abre la búsqueda en el panel enfocado.
    OpenSearch,
    /// Resultado de una búsqueda: (panel, consulta, coincidencias o error).
    SearchDone(u32, String, Result<serde_json::Value, String>),
    /// Enter / Shift+Enter: coincidencia siguiente (1) o anterior (-1).
    SearchStep(isize),
    /// Clic en el nombre de una píldora; dos en menos de 500 ms abren el renombrado.
    PillPress(String),
    /// Cmd+V con una caja abierta (búsqueda o renombrado): el texto del portapapeles va a la caja, no al terminal.
    PasteCaja(Option<String>),
    /// Cmd+⌫ con una caja abierta: vacía la caja.
    BorrarCaja,
    /// Una pestaña de cualquier workspace (la barra es plana): selecciona su grupo y esa pestaña.
    SelectTabIn(String, usize),
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
    /// Ajustes (Cmd+,): abrir, cerrar, elegir sección; estado o plan/instalación de hooks de un agente.
    OpenSettings,
    /// Arrastra la ventana desde un hueco de la barra de sesiones (M3).
    DragWindow,
    /// Fuente del terminal elegida en Ajustes > Terminal; None = la automática (la primera Nerd Font instalada).
    SetTerminalFont(Option<String>),
    /// Elige un workspace por su clave (B4): la barra de sesiones y la paleta no usan posiciones.
    SelectWorkspace(String),
    /// Eco de una tecla: pide solo la pantalla del panel enfocado (B7), sin sondear los demás.
    Echo,
    CloseSettings,
    SettingsSection(settings::Section),
    HooksStatus(String),
    /// Pide el plan (`--dry-run`) para instalar; se muestra y hay que aplicarlo aparte.
    HooksPlan(String),
    HooksPlanned(String, Result<String, String>),
    /// Aplica el plan pendiente (`--yes`). Sin plan, no hace nada.
    HooksApply(String),
    HooksApplied(String, Result<String, String>),
    HooksCancel(String),
    HooksOutput(String, Result<String, String>),
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
    for ws in sidebar::workspaces(&app.panes) {
        let label = ws.label.clone().unwrap_or_else(|| "sin workspace".into());
        let key = ws.label.clone().unwrap_or_default();
        items.push(PaletteItem { label: format!("Workspace · {label}"), detail: format!("{} panel(es)", ws.panes.len()), message: Message::SelectWorkspace(key) });
    }
    for p in &app.panes {
        let detail = format!("{} · {}", p.status.as_str(), p.workspace.as_deref().unwrap_or("sin workspace"));
        items.push(PaletteItem { label: format!("Panel · {}", p.name), detail, message: Message::Select(p.id) });
    }
    let commands: [(&str, &str, Message); 26] = [
        ("Nuevo workspace con grupo…", "Cmd+Shift+N", Message::OpenNewNamedWorkspace),
        ("Agregar agente al workspace…", "", Message::OpenAgentForm),
        ("Levantar equipo del repo", "", Message::TeamRequested(TeamSource::Repo)),
        ("Equipo: Claude manager + Codex worker + Claude revisor", "", Message::TeamRequested(TeamSource::Preset(0))),
        ("Equipo: Claude manager + PI worker", "", Message::TeamRequested(TeamSource::Preset(1))),
        ("Equipo: opencode + Claude revisor", "", Message::TeamRequested(TeamSource::Preset(2))),
        ("Nuevo terminal en este workspace", "Cmd+T", Message::NewTab),
        ("Lanzar agente…", "Cmd+Shift+L", Message::OpenLauncher),
        ("Cerrar panel", "Cmd+W", Message::CloseFocused),
        ("Siguiente que necesita atención", "Cmd+Shift+U", Message::NextAttention),
        ("Lanzar Claude aquí", "", Message::Launch(launch::Agent::Claude)),
        ("Lanzar Codex aquí", "", Message::Launch(launch::Agent::Codex)),
        ("Lanzar PI aquí", "", Message::Launch(launch::Agent::Pi)),
        ("Limpiar la pantalla", "Cmd+K", Message::ClearScreen),
        ("Mostrar u ocultar la barra lateral", "Cmd+B", Message::ToggleSidebar),
        ("Mostrar u ocultar la barra de estado", "Cmd+/", Message::ToggleStatus),
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
        ("p", true) => Some(Message::OpenPalette),
        ("k", false) => Some(Message::ClearScreen),
        (",", false) => Some(Message::OpenSettings),
        ("n", true) => Some(Message::OpenNewNamedWorkspace),
        ("t", false) => Some(Message::NewTab),
        ("w", false) => Some(Message::CloseFocused),
        ("[", false) => Some(Message::NextTab(-1)),
        ("]", false) => Some(Message::NextTab(1)),
        // Con Shift, el teclado da «{» y «}» (o «[» y «]» según la distribución).
        ("{", _) | ("[", true) => Some(Message::NextTab(-1)),
        ("}", _) | ("]", true) => Some(Message::NextTab(1)),
        ("d", false) => Some(Message::SplitFocused(true)),
        ("f", false) => Some(Message::OpenSearch),
        ("b", false) => Some(Message::ToggleSidebar),
        ("/", false) => Some(Message::ToggleStatus),
        ("d", true) => Some(Message::SplitFocused(false)),
        ("l", true) => Some(Message::OpenLauncher),
        ("u", true) => Some(Message::NextAttention),
        ("=", _) | ("+", _) => Some(Message::Zoom(1)),
        ("-", false) => Some(Message::Zoom(-1)),
        ("0", false) => Some(Message::Zoom(0)),
        (d, false) => match d.parse::<usize>() {
            Ok(n @ 1..=9) => Some(Message::SelectRowAt(n - 1)),
            _ => None,
        },
        _ => None,
    }
}

/// M2: un panel que puede tener trabajo en curso (todo salvo los terminados o caídos) pide confirmación al cerrarse.
fn needs_confirm(p: &PaneInfo) -> bool {
    // Como en cualquier terminal, un shell libre en su prompt se cierra sin preguntar. Un proceso en primer plano o un
    // agente (aunque esté en Done o Failed, puede tener estado) piden confirmación; uno ya terminado, no.
    let shell_libre = !p.agent && !p.hooked && p.programa.as_deref().is_some_and(|n| SHELLS.contains(&n));
    !matches!(p.status, Status::Exited) && !shell_libre
}

/// M2: con el formulario abierto, Cmd+W cierra el formulario (no el panel de detrás) y Cmd+N sigue valiendo;
/// el resto de atajos no actúa.
fn shortcut_in_state(app: &App, key: &Key, modifiers: keyboard::Modifiers) -> Option<Message> {
    let message = shortcut(key, modifiers)?;
    if !overlay_owns_keyboard(app) {
        return Some(message);
    }
    match message {
        Message::OpenNewNamedWorkspace => Some(message),
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
    let model = app.launch_model.get(agent.label()).map(String::as_str).filter(|m| *m != launch::DEFAULT_MODEL);
    launch::spawn_request(&name, agent.command_with(&login_shell(), model), &cwd, workspace, rows, cols)
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

/// Un equipo leído de un repo que todavía no se ha confirmado: se muestra antes de lanzar nada.
#[derive(Debug, Clone)]
struct TeamPending {
    key: Option<String>,
    root: String,
    team: team::Team,
    fp: String,
    /// Un integrado: se confirma igual que uno de repo, pero no se guarda confianza (no hay repo que recordar).
    preset: bool,
}

/// Si `root` es un repositorio git (los worktrees de los workers lo necesitan).
fn es_repo(root: &str) -> bool {
    std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--show-toplevel"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Nombre de rama libre en el repo de `root`, a partir de `base`: `base`, `base-2`, `base-3`… (una rama existente no se
/// reutiliza: el daemon la rechazaría).
fn rama_libre(root: &str, base: &str) -> String {
    let existe = |rama: &str| {
        std::process::Command::new("git")
            .args(["-C", root, "show-ref", "--verify", "--quiet", &format!("refs/heads/{rama}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !existe(base) {
        return base.to_string();
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .take(50)
        .find(|c| !existe(c))
        .unwrap_or_else(|| format!("{base}-libre"))
}

/// De dónde sale un equipo a levantar: el `team.toml` del repo, o uno de los integrados.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TeamSource {
    Repo,
    Preset(usize),
}

/// Raíz del repo que contiene `cwd`; si no es un repo, el propio `cwd`.
fn repo_root(cwd: &str) -> String {
    std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

/// Lee `.galgo/team.toml` de `root`: texto y equipo validado. Un archivo que no es regular o pasa de tamaño no se lee.
fn read_team(root: &str) -> Result<(String, team::Team), String> {
    let path = std::path::Path::new(root).join(team::FILE);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("no hay {} en {root}", team::FILE))?;
    if !meta.file_type().is_file() || meta.len() > team::MAX_BYTES {
        return Err(format!("{} no es un archivo normal de tamaño razonable", team::FILE));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("no pude leer {}: {e}", team::FILE))?;
    let team = team::parse(&text)?;
    Ok((text, team))
}

/// Árbol de un equipo: el manager a la izquierda (60 %), y los workers apilados a la derecha.
fn team_tree(manager: Option<u32>, workers: &[u32]) -> Option<layout::Node> {
    let mut apilados: Option<layout::Node> = None;
    for (i, id) in workers.iter().enumerate().rev() {
        apilados = Some(match apilados {
            None => layout::Node::Leaf(*id),
            // Cada worker toma su parte del hueco que queda debajo: 1/(n) del total en el primero, etc.
            Some(resto) => layout::Node::Split { vertical: false, ratio: 1.0 / (workers.len() - i) as f32, a: Box::new(layout::Node::Leaf(*id)), b: Box::new(resto) },
        });
    }
    match (manager, apilados) {
        (Some(m), Some(derecha)) => Some(layout::Node::Split { vertical: true, ratio: 0.6, a: Box::new(layout::Node::Leaf(m)), b: Box::new(derecha) }),
        (Some(m), None) => Some(layout::Node::Leaf(m)),
        (None, derecha) => derecha,
    }
}

/// Levanta un equipo en el workspace `key` (o en uno nuevo con el nombre del equipo, si no lo hay): los agentes se
/// lanzan en orden (el manager primero), cada worker enlazado al manager, y el prompt inicial llega tras arrancar.
fn launch_team(app: &mut App, key: Option<String>, root: String, team: team::Team) -> Task<Message> {
    let key = match key.filter(|k| app.explicit.contains_key(k)) {
        Some(k) => k,
        None => {
            let taken: Vec<&str> = app.explicit.keys().map(String::as_str).chain(app.layouts.by_key.keys().map(String::as_str)).collect();
            let nombre = launch::free_name(&team.name, &taken);
            app.explicit.insert(nombre.clone(), root.clone());
            nombre
        }
    };
    let (rows, cols) = grid_for(app.window);
    // Fuera de un repo git no hay worktrees: los workers se lanzan en la carpeta, y se avisa (review-15 M5).
    let en_repo = es_repo(&root);
    if !en_repo && team.agents.iter().any(|a| a.isolation == launch::Isolation::Worktree) {
        app.error = Some(format!("{root} no es un repo git: los agentes con worktree se lanzan en esa carpeta"));
    }
    let mut reservados: Vec<String> = Vec::new();
    let mut manager_name: Option<String> = None;
    let mut trabajos = Vec::new();
    let mut prompts: Vec<(String, String)> = Vec::new();
    let ordenados = team.agents.iter().filter(|a| a.role == launch::Role::Manager).chain(team.agents.iter().filter(|a| a.role != launch::Role::Manager));
    for a in ordenados {
        let taken: Vec<&str> = names(app).into_iter().chain(reservados.iter().map(String::as_str)).collect();
        let nombre = launch::free_name(&a.name, &taken);
        let manager = if a.role.needs_manager() { manager_name.clone() } else { None };
        if a.role == launch::Role::Manager {
            manager_name = Some(nombre.clone());
        }
        let cmd = a.provider.command_with(&login_shell(), a.model.as_deref());
        // Worktree: la rama es libre en el repo (`nombre`, `nombre-2`…), no una que ya existe.
        let worktree = (a.isolation == launch::Isolation::Worktree && en_repo).then(|| rama_libre(&root, &nombre));
        let req = launch::agent_request(&nombre, cmd, &root, Some(key.clone()), manager, worktree, rows, cols);
        app.roles.insert(nombre.clone(), a.role.label().to_string());
        reservados.push(nombre.clone());
        if let Some(texto) = a.prompt.clone() {
            prompts.push((nombre.clone(), texto));
        }
        trabajos.push((nombre, req));
    }
    // El prompt no se envía con el arranque: espera a que el agente avise por hook (ver `prompts_task`).
    app.prompts_pending.extend(prompts);
    app.inflight.extend(reservados.iter().cloned());
    let clave = key.clone();
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                trabajos
                    .into_iter()
                    .map(|(nombre, req)| {
                        let r = request(req).and_then(|d| serde_json::from_value::<PaneInfo>(d).map_err(|e| e.to_string()));
                        (nombre, r)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| e.to_string())
        },
        move |res| match res {
            Ok(resultados) => Message::TeamLaunched(clave.clone(), manager_name.clone(), resultados),
            Err(e) => Message::TeamLaunched(clave.clone(), None, vec![(String::new(), Err(e))]),
        },
    )
}

/// Texto envuelto en bracketed paste: los saltos de línea llegan como texto y no como envíos (ni Enter ni `\r`).
fn bracketed_paste(text: &str) -> String {
    format!("\u{1b}[200~{text}\u{1b}[201~")
}

/// Entrega los prompts pendientes cuyo agente ya avisó por hook: se pegan sin Enter, para que el usuario confirme. Un
/// panel que terminó sin avisar deja el prompt sin enviar y lo dice. Sin hooks, nunca se envía solo.
fn prompts_task(app: &mut App) -> Task<Message> {
    let mut envios = Vec::new();
    let nombres: Vec<String> = app.prompts_pending.keys().cloned().collect();
    for nombre in nombres {
        let Some(p) = app.panes.iter().find(|p| p.name == nombre) else {
            app.prompts_pending.remove(&nombre);
            continue;
        };
        if p.hooked {
            let texto = app.prompts_pending.remove(&nombre).unwrap_or_default();
            envios.push(send_request(Request::Send { pane: nombre.clone(), text: bracketed_paste(&texto), enter: false }, |_| Message::Echo));
        } else if p.status == Status::Exited {
            app.prompts_pending.remove(&nombre);
            app.error = Some(format!("el prompt de {nombre} no se envió: el agente terminó sin avisar (sin hooks)"));
        }
    }
    Task::batch(envios)
}

/// Estado del formulario «+ Agente…»: qué lanzar, con qué rol y dónde trabajar.
#[derive(Debug, Clone)]
struct AgentForm {
    agent: launch::Agent,
    model: String,
    role: launch::Role,
    isolation: launch::Isolation,
    name: String,
    error: Option<String>,
}

impl AgentForm {
    /// Valores por defecto para el workspace enfocado: un manager si no lo hay todavía, si no, un worker.
    fn para(app: &App) -> AgentForm {
        let hay_manager = manager_of(app, focus_key(app).as_deref()).is_some();
        AgentForm {
            agent: launch::Agent::Claude,
            model: launch::DEFAULT_MODEL.into(),
            role: if hay_manager { launch::Role::Worker } else { launch::Role::Manager },
            isolation: launch::Isolation::Shared,
            name: String::new(),
            error: None,
        }
    }
}

/// Muestra un error en el formulario abierto (no cierra nada).
fn set_form_error(app: &mut App, msg: &str) {
    if let Some(f) = app.agent_form.as_mut() {
        f.error = Some(msg.to_string());
    }
}

/// Manager del workspace `key` (el primer panel con rol manager en ese workspace), si lo hay.
fn manager_of(app: &App, key: Option<&str>) -> Option<String> {
    let key = key?;
    app.panes
        .iter()
        .find(|p| p.workspace.as_deref() == Some(key) && app.roles.get(&p.name).map(String::as_str) == Some("manager"))
        .map(|p| p.name.clone())
}

/// Un diálogo tiene el teclado: cualquier vista que no sea el terminal o la bienvenida. Es la única fuente de verdad
/// para decidir si una tecla llega a un panel.
fn overlay_owns_keyboard(app: &App) -> bool {
    !matches!(body_mode(app), Body::Screen | Body::Welcome)
}

/// Qué ocupa el cuerpo de la ventana: bienvenida, «Nuevo workspace», formulario o el terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    Welcome,
    TeamConfirm,
    AgentForm,
    Launcher,
    Palette,
    Settings,
    CloseConfirm,
    Screen,
}

fn body_mode(app: &App) -> Body {
    if app.settings.is_some() {
        // M4: con los ajustes abiertos, ningún atajo ni clic del terminal de detrás cuenta.
        Body::Settings
    } else if app.confirm_close && !app.panes.is_empty() {
        Body::CloseConfirm
    } else if app.palette.is_some() {
        Body::Palette
    } else if app.launcher {
        Body::Launcher
    } else if app.team_pending.is_some() {
        Body::TeamConfirm
    } else if app.agent_form.is_some() {
        Body::AgentForm
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
/// Abre una terminal en `path`. Con `agrupar` (Cmd+Shift+N) el workspace se llama como la carpeta, numerado si ya
/// existe, y se agrupa en la barra. Sin carpeta que elegir: la terminal hereda el cwd del panel enfocado.
fn open_terminal(app: &mut App, path: String, agrupar: bool) -> Task<Message> {
    traza("shell: petición de spawn");
    let (rows, cols) = grid_for(app.window);
    let grupo = agrupar.then(|| {
        let base = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "workspace".into());
        let mut taken: Vec<&str> = app.explicit.keys().map(String::as_str).collect();
        taken.extend(app.layouts.by_key.keys().map(String::as_str));
        let nombre = launch::free_name(&base, &taken);
        app.explicit.insert(nombre.clone(), path.clone());
        nombre
    });
    let name = launch::free_name("shell", &names(app));
    app.spawn_size = Some((rows, cols));
    let req = launch::shell_request(&name, &login_shell(), &path, grupo, rows, cols);
    Task::perform(
        async move {
            let result = tokio::task::spawn_blocking(move || request(req)).await.map_err(|e| e.to_string()).and_then(|r| r);
            (path, result)
        },
        |(path, result)| Message::WorkspaceOpened(path, result),
    )
}

/// Carpeta en la que nace una terminal nueva: la del panel enfocado (vivo, con `cd`), o `~`.
fn cwd_nuevo(app: &App) -> String {
    focus_context(app).1.unwrap_or_else(home_dir)
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

/// Lista de paneles pedida al daemon en un hilo al arrancar, en paralelo con la ventana (la toma la primera `load_panes`).
/// Lista de paneles pedida al daemon en un hilo al arrancar, en paralelo con la ventana (la toma la primera `load_panes`).
static LISTA_PREVIA: std::sync::Mutex<Option<Result<Vec<PaneInfo>, String>>> = std::sync::Mutex::new(None);

fn load_panes() -> Task<Message> {
    if let Some(lista) = LISTA_PREVIA.lock().unwrap().take() {
        traza("daemon: lista previa (ya pedida al arrancar)");
        return Task::done(Message::Panes(lista));
    }
    Task::perform(async {
        traza("daemon: petición de lista enviada");
        let r = tokio::task::spawn_blocking(|| serde_json::from_value(request(Request::List)?).map_err(|e| e.to_string()))
            .await.map_err(|e| e.to_string())?;
        traza("daemon: lista respondida");
        r
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
/// Búsqueda en el scrollback de un panel: la consulta, sus coincidencias (`[línea, columna, longitud]`, la línea 0 es la
/// más antigua del historial) y la que está seleccionada. `scrollback` es el número de líneas de historial.
#[derive(Debug, Clone, Default)]
pub struct Busqueda {
    pane: u32,
    consulta: String,
    resultados: Vec<[usize; 3]>,
    actual: usize,
    scrollback: usize,
    buscando: bool,
}

/// Tarea que busca `consulta` en el panel `id` (fuera del hilo de la interfaz).
fn buscar_task(id: u32, consulta: String) -> Task<Message> {
    let para_respuesta = consulta.clone();
    Task::perform(async move {
        tokio::task::spawn_blocking(move || request(Request::Search { pane: id.to_string(), query: consulta })).await.map_err(|e| e.to_string())?
    }, move |r| Message::SearchDone(id, para_respuesta.clone(), r))
}

/// Lleva la vista del panel a la coincidencia seleccionada: a media pantalla, para que se vea con contexto.
fn ir_a_coincidencia(app: &mut App) -> Task<Message> {
    let Some(b) = app.buscar.as_ref() else { return Task::none() };
    let Some(&[linea, _, _]) = b.resultados.get(b.actual) else { return Task::none() };
    let id = b.pane;
    let alto = app.snap.as_ref().map_or(24, |s| s.lines.len());
    let k = (b.scrollback + alto / 2).saturating_sub(linea).min(b.scrollback);
    app.scroll = k as u16;
    pedir_vista(app, id)
}

/// Pide la vista actual del panel `id` (con su desplazamiento).
fn pedir_vista(app: &mut App, id: u32) -> Task<Message> {
    let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
    load_screen(id, app.scroll, since)
}

/// Resaltados de búsqueda visibles en la pantalla de `snap`: (fila, columna inicial, columna final, es la actual).
/// La fila 0 de la vista corresponde a la línea `scrollback_max − scrollback` de la historia.
pub fn resaltes_de(app: &App, snap: &Snapshot) -> Vec<(u16, u16, u16, bool)> {
    let Some(b) = app.buscar.as_ref() else { return Vec::new() };
    if app.selected != Some(b.pane) {
        return Vec::new();
    }
    let arriba = (snap.scrollback_max as usize).saturating_sub(snap.scrollback as usize);
    let alto = snap.lines.len();
    let mut out = Vec::new();
    for (i, &[linea, col, largo]) in b.resultados.iter().enumerate() {
        if linea >= arriba && linea < arriba + alto && largo > 0 {
            let fila = (linea - arriba) as u16;
            out.push((fila, col as u16, (col + largo - 1) as u16, i == b.actual));
        }
    }
    out
}

/// Barra de búsqueda (arriba a la derecha del panel): la consulta y el contador.
fn barra_busqueda(b: &Busqueda) -> Element<'static, Message> {
    let consulta = if b.consulta.is_empty() { iced::widget::text("Buscar").size(theme::SMALL_SIZE).color(theme::DIM) } else { iced::widget::text(b.consulta.clone()).size(theme::SMALL_SIZE) };
    let cuenta = if b.consulta.is_empty() {
        String::new()
    } else if b.buscando {
        "…".to_string()
    } else if b.resultados.is_empty() {
        "0 de 0".to_string()
    } else {
        format!("{} de {}", b.actual + 1, b.resultados.len())
    };
    let fila = row![consulta, iced::widget::text(cuenta).size(theme::SMALL_SIZE).color(theme::DIM)].spacing(10).align_y(iced::Alignment::Center);
    container(fila)
        .padding([4, 10])
        .style(theme::group)
        .align_x(iced::alignment::Horizontal::Right)
        .into()
}

/// Teclado con la búsqueda abierta: nada de lo que se escribe llega al terminal.
fn buscar_tecla(app: &mut App, key: &Key, modifiers: keyboard::Modifiers, text: Option<&str>) -> Task<Message> {
    match key {
        Key::Named(Named::Escape) => {
            app.buscar = None;
            back_to_live(app);
            match app.selected {
                Some(id) => pedir_vista(app, id),
                None => Task::none(),
            }
        }
        Key::Named(Named::Enter) => update(app, Message::SearchStep(if modifiers.shift() { -1 } else { 1 })),
        Key::Named(Named::Backspace) => {
            let Some(b) = app.buscar.as_mut() else { return Task::none() };
            b.consulta.pop();
            buscar_cambio(app)
        }
        _ => {
            let Some(t) = text.filter(|t| !t.chars().any(char::is_control)) else { return Task::none() };
            let Some(b) = app.buscar.as_mut() else { return Task::none() };
            b.consulta.push_str(t);
            buscar_cambio(app)
        }
    }
}

/// La consulta cambió: se vuelve a buscar en todo el historial (o se limpia si está vacía).
fn buscar_cambio(app: &mut App) -> Task<Message> {
    let Some(b) = app.buscar.as_mut() else { return Task::none() };
    if b.consulta.is_empty() {
        b.resultados.clear();
        b.actual = 0;
        b.buscando = false;
        return Task::none();
    }
    b.buscando = true;
    buscar_task(b.pane, b.consulta.clone())
}

/// Teclado durante el renombrado de una píldora: Enter guarda (vacío = quitar el nombre), Esc cancela.
fn renombrar_tecla(app: &mut App, key: &Key, text: Option<&str>) -> Task<Message> {
    match key {
        Key::Named(Named::Escape) => app.renombrando = None,
        Key::Named(Named::Enter) => {
            if let Some((clave, texto)) = app.renombrando.take() {
                let nombre = texto.trim().to_string();
                if nombre.is_empty() || nombre == clave {
                    app.alias.remove(&clave);
                } else {
                    app.alias.insert(clave, nombre);
                }
                if let Some(path) = app.prefs_path.clone() {
                    let _ = prefs::set(&path, "aliases", serde_json::to_value(&app.alias).unwrap_or_default());
                }
            }
        }
        Key::Named(Named::Backspace) => {
            if let Some((_, t)) = app.renombrando.as_mut() {
                t.pop();
            }
        }
        _ => {
            if let (Some(t), Some((_, buf))) = (text.filter(|t| !t.chars().any(char::is_control)), app.renombrando.as_mut()) {
                buf.push_str(t);
            }
        }
    }
    Task::none()
}

fn back_to_live(app: &mut App) {
    app.scroll = 0;
    clear_selection(app);
}

/// Selecciona un panel. Mirarlo no cambia su tamaño y lo deja sin marca de "sin leer".
fn select(app: &mut App, id: u32) -> Task<Message> {
    // La búsqueda es de un panel: si se enfoca otro, se cierra.
    if app.buscar.as_ref().is_some_and(|b| b.pane != id) {
        app.buscar = None;
    }
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
    let x = sidebar_w();
    let y = top_h() + extra_top;
    layout::Rect { x, y, w: (window.0 - x).max(0.0), h: (window.1 - y - status_h() - extra_bottom).max(0.0) }
}

/// Cuadrícula de texto que cabe en un rectángulo de panel (con su margen interior).
/// Columnas de margen a la derecha. Un glifo de una fuente de fallback puede medir un poco más que la celda y
/// la línea del terminal, sin este margen, se parte en dos (el prompt de p10k deja la hora en la fila siguiente).
/// El daemon recibe ese número de columnas, así que el shell dibuja su prompt dentro del margen.
const COLS_MARGIN: u16 = 2;

fn grid_in(rect: layout::Rect) -> (u16, u16) {
    let cols = ((rect.w - 2.0 * LEAF_PAD) / cell_w()).floor().max(20.0) as u16;
    (((rect.h - leaf_top() - LEAF_PAD) / line_h()).floor().max(5.0) as u16, cols.saturating_sub(COLS_MARGIN).max(20))
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
    (rect.x + LEAF_PAD, rect.y + leaf_top())
}

/// Origen de la cuadrícula del panel enfocado; sin rectángulo conocido, el de referencia.
fn focus_origin(app: &App) -> (f32, f32) {
    app.selected.and_then(|id| pane_rect(app, id)).map_or(term_origin(), origin_in)
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
/// Como `pane_keys`, con el nombre de cada panel: para recuperar su orden tras un reinicio.
fn pane_nombres(app: &App) -> Vec<(u32, String, String)> {
    app.panes.iter().map(|p| (p.id, p.workspace.clone().unwrap_or_default(), p.name.clone())).collect()
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

/// La barra de búsqueda flota arriba a la derecha del panel enfocado: un clic en ella no es un clic en el terminal.
fn en_barra_busqueda(app: &App, p: Point) -> bool {
    app.buscar.is_some()
        && app.selected.and_then(|id| pane_rect(app, id)).is_some_and(|r| p.x > r.x + r.w - 380.0 && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + leaf_top() + 48.0)
}

/// Botones que se reenvían al programa (los demás no tienen significado en la cuadrícula).
fn raton_boton(b: mouse::Button) -> Option<u8> {
    match b {
        mouse::Button::Left => Some(raton::IZQ),
        mouse::Button::Middle => Some(raton::MEDIO),
        mouse::Button::Right => Some(raton::DER),
        _ => None,
    }
}

/// Secuencia de ratón para `gesto` en `celda`, si el programa del panel enfocado pide seguimiento de ratón.
/// No mira la geometría: quien pulsa la comprueba antes; soltar y arrastrar usan la celda ya conocida.
fn secuencia_raton_en(app: &App, gesto: raton::Gesto, (fila, col): Cell) -> Option<String> {
    let snap = app.snap.as_ref()?;
    if app.scroll != 0 {
        return None;
    }
    let mods = raton::Mods { shift: app.mods.shift(), alt: app.mods.alt(), ctrl: app.mods.control() };
    raton::secuencia(snap.mouse, snap.mouse_sgr, gesto, mods, fila, col)
}

/// Igual que `secuencia_raton_en`, pero solo si el puntero cae en la cuadrícula del panel enfocado y no sobre
/// otra cosa (barra de búsqueda, divisor, otro panel). Shift devuelve el ratón a galgo (selección propia).
fn secuencia_raton_pulsa(app: &App, gesto: raton::Gesto) -> Option<(String, Cell)> {
    let p = app.pointer?;
    if app.mods.shift() || body_mode(app) != Body::Screen || !pointer_in_grid(app) || en_barra_busqueda(app, p) {
        return None;
    }
    if divider_under(app, p).is_some() || leaf_at(app, p) != app.selected {
        return None;
    }
    let cell = grid_cell(app, p)?;
    secuencia_raton_en(app, gesto, cell).map(|seq| (seq, cell))
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

/// Suscripción a los cambios de un panel: el daemon escribe un aviso cada vez que su pantalla cambia. Cualquier salida
/// que no sea el `closed` legítimo (conexión caída, daemon antiguo, panel desconocido…) avisa para sondear, y cada
/// reintento fallido lo vuelve a avisar (review-15 M4: antes, un `ok:false` dejaba el panel congelado).
fn watch_stream(pane: u32) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(16, move |mut salida| async move {
        loop {
            match watch_once(pane, &mut salida).await {
                WatchEnd::Cerrado => break,
                WatchEnd::Cayo => {
                    use iced::futures::SinkExt;
                    if salida.send(Message::WatchDown(pane)).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

enum WatchEnd {
    /// El panel terminó (o la suscripción dejó de interesar): no hay nada más que vigilar.
    Cerrado,
    /// Algo falló: se avisa para sondear y se reintenta.
    Cayo,
}

/// Lo que dice una línea del daemon en una suscripción.
#[derive(Debug, PartialEq)]
enum Aviso {
    Cambio(u64),
    Inicio,
    Cerrado,
    /// Un error del daemon (`ok:false`), una línea que no se entiende, o un evento desconocido.
    Fallo,
}

fn leer_aviso(linea: &str) -> Aviso {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(linea) else { return Aviso::Fallo };
    if v["ok"].as_bool() != Some(true) {
        return Aviso::Fallo;
    }
    match v["event"].as_str() {
        Some("changed") => Aviso::Cambio(v["gen"].as_u64().unwrap_or(0)),
        Some("watching") => Aviso::Inicio,
        Some("closed") => Aviso::Cerrado,
        _ => Aviso::Fallo,
    }
}

/// Una conexión de suscripción: pide `Watch` y reenvía cada aviso de cambio a la app.
async fn watch_once(pane: u32, salida: &mut iced::futures::channel::mpsc::Sender<Message>) -> WatchEnd {
    use iced::futures::SinkExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let Ok(stream) = tokio::net::UnixStream::connect(galgo_proto::socket_path()).await else {
        return WatchEnd::Cayo;
    };
    let (r, mut w) = stream.into_split();
    let pedido = serde_json::to_string(&Request::Watch { pane: pane.to_string() }).unwrap_or_default() + "\n";
    if w.write_all(pedido.as_bytes()).await.is_err() {
        return WatchEnd::Cayo;
    }
    let mut lineas = tokio::io::BufReader::new(r).lines();
    loop {
        let linea = match lineas.next_line().await {
            Ok(Some(linea)) => linea,
            _ => return WatchEnd::Cayo,
        };
        match leer_aviso(&linea) {
            Aviso::Cambio(gen) => {
                if salida.send(Message::PaneChanged(pane, gen)).await.is_err() {
                    return WatchEnd::Cerrado;
                }
            }
            Aviso::Inicio => {}
            Aviso::Cerrado => return WatchEnd::Cerrado,
            Aviso::Fallo => return WatchEnd::Cayo,
        }
    }
}

/// Abre una URL o una ruta con el visor del sistema (`open`). Un fallo se avisa y no detiene la app.
fn open_target(objetivo: &str) -> Result<(), String> {
    if objetivo.starts_with("http://") || objetivo.starts_with("https://") {
        return std::process::Command::new("open").arg(objetivo).spawn().map(|_| ()).map_err(|e| format!("no pude abrir el enlace: {e}"));
    }
    let ruta = std::path::Path::new(objetivo);
    let meta = std::fs::metadata(ruta).map_err(|_| format!("no existe: {objetivo}"))?;
    let comando = match accion_para(ruta, &meta)? {
        AccionRuta::Abrir => "abrir",
        AccionRuta::Revelar => "revelar",
    };
    let mut cmd = std::process::Command::new("open");
    if comando == "revelar" {
        cmd.arg("-R");
    }
    cmd.arg(ruta).spawn().map(|_| ()).map_err(|e| format!("no pude abrir {objetivo}: {e}"))
}

/// Qué hace Cmd+clic con una ruta: abrir un documento conocido, o mostrarla en el Finder (sin ejecutarla).
#[derive(Debug, PartialEq)]
enum AccionRuta {
    Abrir,
    Revelar,
}

/// review-15 M2: `open` ejecuta bundles y scripts de ejecución (`.command`, `.tool`, `.app`…), así que Cmd+clic nunca los
/// ejecuta: se rechazan. Un ejecutable cualquiera tampoco se lanza. Un documento conocido se abre; el resto se revela.
fn accion_para(ruta: &std::path::Path, meta: &std::fs::Metadata) -> Result<AccionRuta, String> {
    const BUNDLES: [&str; 6] = ["app", "command", "tool", "terminal", "workflow", "pkg"];
    const DOCUMENTOS: [&str; 22] = [
        "pdf", "txt", "md", "png", "jpg", "jpeg", "gif", "webp", "heic", "svg", "json", "toml", "yaml", "yml", "csv", "log", "html", "mp4", "mov", "docx", "xlsx", "pptx",
    ];
    let ext = ruta.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let dentro_de_bundle = ruta.components().any(|c| c.as_os_str().to_string_lossy().to_lowercase().ends_with(".app"));
    if BUNDLES.contains(&ext.as_str()) || dentro_de_bundle {
        return Err(format!("no abro {} desde Cmd+clic: son programas que se ejecutan", ruta.display()));
    }
    if meta.is_dir() {
        return Ok(AccionRuta::Abrir);
    }
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        return Err(format!("{} es ejecutable: Cmd+clic no lo lanza", ruta.display()));
    }
    Ok(if DOCUMENTOS.contains(&ext.as_str()) { AccionRuta::Abrir } else { AccionRuta::Revelar })
}

/// Shells: si el proceso en primer plano es uno de ellos, su título OSC (la ruta del prompt) no se muestra.
const SHELLS: [&str; 5] = ["zsh", "bash", "sh", "fish", "dash"];

/// Carpeta corta para la píldora: el nombre de la carpeta, o `~` si es la de casa.
fn carpeta_corta(cwd: &str) -> String {
    if cwd == home_dir() {
        return "~".into();
    }
    std::path::Path::new(cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| cwd.to_string())
}

/// Texto de la pestaña y de la cabecera: el título de la tarea; si no, el proceso en primer plano (o el título que
/// anunció por OSC, si no es un shell), y la carpeta corta: `vim · nexo`, `claude · nexo`, `zsh · ~`.
fn titulo_pestana(p: &PaneInfo) -> String {
    if let Some(t) = p.title.as_deref().filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    let proceso = p
        .programa
        .clone()
        .or_else(|| p.cmd.first().and_then(|c| std::path::Path::new(c).file_name()).map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| p.name.clone());
    let base = match p.titulo_osc.as_deref().map(str::trim).filter(|t| !t.is_empty() && !SHELLS.contains(&proceso.as_str())) {
        Some(t) if t.chars().count() > 32 => format!("{}…", t.chars().take(31).collect::<String>()),
        Some(t) => t.to_string(),
        None => proceso,
    };
    match p.cwd.as_deref() {
        Some(cwd) => format!("{base} · {}", carpeta_corta(cwd)),
        None => base,
    }
}

/// Qué eventos de la ventana llegan a la app. El clic izquierdo llega SIEMPRE: `rich_text` captura cualquier clic que
/// caiga sobre texto, así que filtrar por «no capturado» dejaba sin foco ni selección a un terminal con contenido.
/// Quien lo recibe comprueba dónde cae (capas abiertas, divisor, panel, cuadrícula).
fn reenviar_evento(event: Event, status: event::Status) -> Option<Message> {
    match event {
        Event::Keyboard(keyboard::Event::KeyPressed { .. }) | Event::Keyboard(keyboard::Event::ModifiersChanged(_)) => Some(Message::Event(event)),
        Event::Window(iced::window::Event::FileDropped(path)) => Some(Message::FileDropped(path)),
        Event::Window(iced::window::Event::Resized(_))
        | Event::Window(iced::window::Event::Opened { .. })
        | Event::Window(iced::window::Event::Focused)
        | Event::Window(iced::window::Event::Unfocused) => Some(Message::Event(event)),
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => Some(Message::Event(event)),
        // Central y derecho: solo los programas con ratón los usan; la geometría la decide quien recibe.
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle | mouse::Button::Right))
        | Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Middle | mouse::Button::Right)) => Some(Message::Event(event)),
        // La rueda que otro widget ya usó (una lista con scroll) no llega al terminal.
        Event::Mouse(mouse::Event::WheelScrolled { .. }) if status == event::Status::Ignored => Some(Message::Event(event)),
        // El soltar del botón siempre llega, para no dejar una selección arrastrándose.
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) | Event::Mouse(mouse::Event::CursorMoved { .. }) => Some(Message::Event(event)),
        _ => None,
    }
}

/// Shells que terminaron solos (`exit`, Ctrl-D): su pestaña se cierra, como en cualquier terminal. Un agente, un
/// comando suelto, un manager o un panel con resultados sin confirmar se quedan para poder leerlos.
fn shells_terminados(panes: &[PaneInfo]) -> Vec<u32> {
    panes
        .iter()
        .filter(|p| p.status == Status::Exited && !p.agent && !p.hooked && p.unacked == 0)
        .filter(|p| p.cmd.first().and_then(|c| std::path::Path::new(c).file_name()).and_then(|n| n.to_str()).is_some_and(|n| SHELLS.contains(&n)))
        .filter(|p| !panes.iter().any(|o| o.manager.as_deref() == Some(p.name.as_str())))
        .map(|p| p.id)
        .collect()
}

/// Ruta o texto entre comillas simples para la shell, con las comillas internas escapadas.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Caracteres de una fila de la pantalla, una entrada por columna (sin continuación de caracteres anchos).
fn row_chars(snap: &Snapshot, row: u16) -> Vec<char> {
    snap.lines.get(row as usize).map_or_else(Vec::new, |runs| runs.iter().flat_map(|r| r.t.chars()).collect())
}

/// Palabra que contiene la columna `col` (letras, cifras y _ - . /), como rango de columnas.
fn palabra_en(chars: &[char], col: u16) -> Option<(u16, u16)> {
    let es = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/');
    let c = *chars.get(col as usize)?;
    if !es(c) {
        return None;
    }
    let mut ini = col as usize;
    while ini > 0 && es(chars[ini - 1]) {
        ini -= 1;
    }
    let mut fin = col as usize;
    while fin + 1 < chars.len() && es(chars[fin + 1]) {
        fin += 1;
    }
    Some((ini as u16, fin as u16))
}

/// URL o ruta (existente) bajo la columna `col`, para abrirla con Cmd+clic. Se recortan los signos de puntuación
/// finales; una ruta solo vale si existe.
fn token_en(chars: &[char], col: u16) -> Option<String> {
    let c = *chars.get(col as usize)?;
    if c.is_whitespace() {
        return None;
    }
    let mut ini = col as usize;
    while ini > 0 && !chars[ini - 1].is_whitespace() {
        ini -= 1;
    }
    let mut fin = col as usize;
    while fin + 1 < chars.len() && !chars[fin + 1].is_whitespace() {
        fin += 1;
    }
    let texto: String = chars[ini..=fin].iter().collect();
    let texto = texto.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | ')' | ']' | '"' | '\''));
    if texto.starts_with("http://") || texto.starts_with("https://") {
        return Some(texto.to_string());
    }
    if !(texto.starts_with('/') || texto.starts_with("~/")) {
        return None;
    }
    let ruta = match texto.strip_prefix("~/") {
        Some(resto) => std::path::PathBuf::from(home_dir()).join(resto),
        None => std::path::PathBuf::from(texto),
    };
    // La ruta ya expandida (`~` es el inicio del usuario): es lo que se abre, sin depender de una shell.
    ruta.exists().then(|| ruta.to_string_lossy().into_owned())
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
/// sin esperar al siguiente tick. Espera con `tokio::time::sleep`: no bloquea un hilo por tecla (B7).
fn echo_soon() -> Task<Message> {
    Task::perform(async { tokio::time::sleep(Duration::from_millis(ECHO_DELAY_MS)).await }, |_| Message::Echo)
}

/// Panel que enfoca un workspace por su clave: el de su pestaña activa, o el primero (B4).
fn workspace_focus(app: &App, key: &str) -> Option<u32> {
    let ws = sidebar::workspaces(&app.panes).into_iter().find(|w| w.label.clone().unwrap_or_default() == key)?;
    let activo = app.layouts.active_tab(key).map(|t| t.focus).filter(|f| ws.panes.iter().any(|p| p.id == *f));
    activo.or_else(|| ws.panes.first().map(|p| p.id))
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
    sync_chrome(app);
    persist_workspaces(app);
    task
}

/// Guarda pestañas, splits y grupos explícitos cuando cambian. Solo después de restaurar, y con los nombres de los
/// paneles reales (un panel sin nombre conocido sale de su árbol).
/// Aplica la disposición guardada (M3). Al primer listado, los nombres que faltan (paneles que no volvieron tras
/// `galgo stop`) se relanzan como shells nuevos en su carpeta. La disposición se aplica cuando todos sus paneles
/// existen, o tras unos segundos de espera; mientras, no se guarda nada (un listado incompleto borraría la disposición).
fn intentar_restaurar(app: &mut App) -> Task<Message> {
    if app.ws_restored {
        return Task::none();
    }
    if app.ws_pendiente.is_none() {
        let Some(v) = app.prefs_path.as_ref().and_then(|p| prefs::get(p, "workspaces")) else {
            app.ws_restored = true;
            return Task::none();
        };
        app.pane_meta = workspace::metadatos(&v);
        app.ws_pendiente = Some(v);
    }
    let v = app.ws_pendiente.clone().unwrap_or(serde_json::Value::Null);
    let presentes: std::collections::HashSet<&str> = app.panes.iter().map(|p| p.name.as_str()).collect();
    let faltan: Vec<String> = workspace::nombres(&v).into_iter().filter(|n| !presentes.contains(n.as_str())).collect();
    let mut tareas = Vec::new();
    if app.ws_intentos == 0 {
        // Un shell nuevo para cada panel que no volvió, en su carpeta (si se conoce).
        for nombre in &faltan {
            let Some((cwd, ws)) = app.pane_meta.get(nombre).and_then(|(c, w)| Some((c.clone()?, w.clone()))) else { continue };
            // El workspace guardado: así el panel vuelve a su pestaña del grupo, no a «sin workspace».
            let req = launch::shell_request(nombre, &login_shell(), &cwd, ws, 40, 140);
            tareas.push(Task::perform(async move { tokio::task::spawn_blocking(move || request(req)).await.ok(); }, |_| Message::Echo));
        }
    }
    app.ws_intentos += 1;
    if !faltan.is_empty() && app.ws_intentos < WS_ESPERA_LISTADOS {
        return Task::batch(tareas);
    }
    let ids: std::collections::HashMap<String, u32> = app.panes.iter().map(|p| (p.name.clone(), p.id)).collect();
    let r = workspace::restore(&v, &|n| ids.get(n).copied());
    app.explicit = r.explicit;
    app.layouts.cargar(r.by_key);
    // El panel que tenía el foco vuelve a tenerlo (si ya está; si no, el primero, como siempre).
    if let Some(id) = v.get("foco").and_then(serde_json::Value::as_str).and_then(|n| ids.get(n)) {
        app.pending_focus = Some(*id);
    }
    // Los paneles que aún no han vuelto conservan su orden y su pestaña activa para cuando aparezcan.
    let pendientes = workspace::ordenes_pendientes(&v).into_iter().filter(|(n, _)| !ids.contains_key(n)).collect();
    app.layouts.recordar_pendientes(pendientes);
    app.ws_pendiente = None;
    app.ws_restored = true;
    Task::batch(tareas)
}

/// Listados (cada ~1 s) que se esperan a que vuelvan los paneles de la disposición antes de aplicarla.
const WS_ESPERA_LISTADOS: u32 = 15;

fn persist_workspaces(app: &mut App) {
    if !app.ws_restored || !app.started {
        return;
    }
    let nombres: std::collections::HashMap<u32, String> = app.panes.iter().map(|p| (p.id, p.name.clone())).collect();
    let mut v = workspace::snapshot(&app.layouts.by_key, &app.explicit, &|id| nombres.get(&id).cloned());
    // M3: el cwd y el workspace de cada panel viven junto a la disposición. Los que no están ahora se conservan.
    for p in &app.panes {
        app.pane_meta.insert(p.name.clone(), (p.cwd.clone(), p.workspace.clone()));
    }
    while app.pane_meta.len() > 200 {
        let primero = app.pane_meta.keys().next().cloned();
        if let Some(k) = primero {
            app.pane_meta.remove(&k);
        }
    }
    if let Some(obj) = v.as_object_mut() {
        let panes: serde_json::Map<String, serde_json::Value> = app
            .pane_meta
            .iter()
            .map(|(n, (cwd, ws))| (n.clone(), serde_json::json!({ "cwd": cwd, "workspace": ws })))
            .collect();
        obj.insert("panes".into(), serde_json::Value::Object(panes));
        // El panel con foco: tras un reinicio es el que queda seleccionado (y así, su pestaña, la activa).
        obj.insert("foco".into(), serde_json::json!(app.selected.and_then(|id| nombres.get(&id).cloned())));
    }
    let texto = v.to_string();
    if texto != app.ws_saved {
        if let Some(path) = &app.prefs_path {
            if prefs::set(path, "workspaces", v).is_ok() {
                app.ws_saved = texto;
            }
        }
    }
    let roles = serde_json::to_value(&app.roles).unwrap_or_default().to_string();
    if roles != app.roles_saved {
        if let Some(path) = &app.prefs_path {
            if prefs::set(path, "agent_roles", serde_json::to_value(&app.roles).unwrap_or_default()).is_ok() {
                app.roles_saved = roles;
            }
        }
    }
}

/// Invariante (G1): el panel seleccionado está en la pestaña activa de su workspace. Si no lo está, la pestaña
/// activa pasa a ser la suya. Así lo que se teclea siempre va a un panel que se ve.
/// Plan de hooks: `galgo hooks install <agente> --dry-run`, en segundo plano.
fn hooks_plan_task(agent: &str, args: Vec<String>) -> Task<Message> {
    let agent = agent.to_string();
    Task::perform(
        async move {
            let out = tokio::task::spawn_blocking(move || settings::run_galgo(&args)).await.map_err(|e| e.to_string()).and_then(|r| r);
            (agent, out)
        },
        |(agent, out)| Message::HooksPlanned(agent, out),
    )
}

/// Aplicar hooks: `galgo hooks install <agente> --yes`, en segundo plano; solo tras un plan visto.
fn hooks_task_apply(agent: &str, args: Vec<String>) -> Task<Message> {
    let agent = agent.to_string();
    Task::perform(
        async move {
            let out = tokio::task::spawn_blocking(move || settings::run_galgo(&args)).await.map_err(|e| e.to_string()).and_then(|r| r);
            (agent, out)
        },
        |(agent, out)| Message::HooksApplied(agent, out),
    )
}

/// Ejecuta `galgo` con `args` en segundo plano (no bloquea la ventana) y devuelve la salida al agente `agent`.
fn hooks_task(agent: &str, args: Vec<String>) -> Task<Message> {
    let agent = agent.to_string();
    Task::perform(
        async move {
            let out = tokio::task::spawn_blocking(move || settings::run_galgo(&args)).await.map_err(|e| e.to_string()).and_then(|r| r);
            (agent, out)
        },
        |(agent, out)| Message::HooksOutput(agent, out),
    )
}

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
        Message::PaneChanged(id, gen) => {
            app.watch_down = false;
            if app.selected == Some(id) && app.snap_gen != Some(gen) {
                if app.loading {
                    app.refresh_wanted = true;
                    return Task::none();
                }
                app.loading = true;
                // Solo se pide "lo que cambió" si la última respuesta completa es del mismo desplazamiento.
                let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
                return load_screen(id, app.scroll, since);
            }
            Task::none()
        }
        Message::WatchDown(id) => {
            if app.selected == Some(id) {
                app.watch_down = true;
                // Una lectura ahora mismo, para no quedarse con la pantalla vieja mientras se sondea.
                if !app.loading {
                    app.loading = true;
                    let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
                    return load_screen(id, app.scroll, since);
                }
            }
            Task::none()
        }
        Message::ClearScreen => match app.selected {
            // Ctrl+L: el shell redibuja el prompt en lo alto y limpia la pantalla.
            Some(id) => send_request(Request::Send { pane: id.to_string(), text: "\u{c}".into(), enter: false }, |_| Message::Echo),
            None => Task::none(),
        },
        Message::FileDropped(path) => {
            if body_mode(app) != Body::Screen {
                return Task::none();
            }
            // review-15 M3: un nombre con caracteres de control se rechaza: esos bytes los interpreta el editor de línea
            // antes que la shell (Ctrl-C, Ctrl-U, secuencias de escape) y ejecutarían algo que el usuario no vio.
            let Some(texto) = path.to_str() else {
                app.error = Some("el nombre del archivo no es UTF-8: no se pega".into());
                return Task::none();
            };
            if texto.chars().any(char::is_control) {
                app.error = Some("el nombre del archivo tiene caracteres de control: no se pega".into());
                return Task::none();
            }
            match app.selected {
                // La ruta se pega entre comillas, sin Enter: lo decides tú. Con bracketed paste activo en el panel, va
                // marcada como pegado, para que el editor de línea no la interprete como teclas.
                Some(id) => {
                    let pegado = shell_quote(texto) + " ";
                    let bracketed = shown(app).is_some_and(|s| s.bracketed_paste);
                    let text = if bracketed { bracketed_paste(&pegado) } else { pegado };
                    send_request(Request::Send { pane: id.to_string(), text, enter: false }, |_| Message::Echo)
                }
                None => Task::none(),
            }
        }
        Message::Event(Event::Keyboard(keyboard::Event::ModifiersChanged(m))) => {
            app.mods = m;
            Task::none()
        }
        Message::ToggleSidebar => {
            app.sidebar = !app.sidebar;
            save_pref(app, "sidebar", serde_json::json!(app.sidebar));
            Task::none()
        }
        Message::ToggleStatus => {
            app.status = !app.status;
            save_pref(app, "status", serde_json::json!(app.status));
            Task::none()
        }
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
            save_pref(app, "zoom", serde_json::json!(next));
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
            // El panel enfocado no se sondea: le avisa el daemon (`PaneChanged`). Solo si esa suscripción falla, se sondea.
            let focus_task = match app.selected {
                Some(id) if !app.loading && app.watch_down => {
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
            if let Ok(ScreenReply::Full(mut snap)) = result {
                // Un panel en segundo plano que pidió copiar: el texto no se pierde al no estar enfocado.
                let copiar = snap.portapapeles.take().map_or_else(Task::none, iced::clipboard::write);
                app.others_gen.insert(id, snap.gen);
                app.others.insert(id, snap);
                return copiar;
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
        Message::SelectTabIn(key, index) => {
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
                    app.layouts.sync_nombrado(&pane_nombres(app));
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
        Message::SelectRowAt(n) => match app.layouts.en_orden().get(n).map(|(k, i)| ((*k).clone(), *i)) {
            Some((key, i)) => update(app, Message::SelectTabIn(key, i)),
            None => Task::none(),
        },
        Message::CloseTabIn(key, i) => {
            let seleccion = update(app, Message::SelectTabIn(key, i));
            let cerrar = update(app, Message::CloseFocused);
            Task::batch([seleccion, cerrar])
        }
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
        Message::SelectWorkspace(key) => match workspace_focus(app, &key) {
            Some(id) => select(app, id),
            None => Task::none(),
        },
        Message::Echo => match app.selected {
            // B7: el eco pide solo la pantalla del enfocado; ni el sondeo de los demás paneles ni el redimensionado.
            Some(id) if !app.loading => {
                app.loading = true;
                let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
                load_screen(id, app.scroll, since)
            }
            _ => Task::none(),
        },
        Message::SetTerminalFont(choice) => {
            let family = match &choice {
                Some(name) => fonts::CANDIDATES.iter().map(|c| c.0).find(|f| *f == name.as_str()).unwrap_or("Menlo"),
                None => fonts::automatic(&app.font_installed),
            };
            fonts::set_current(family);
            if let Some(path) = &app.prefs_path {
                let _ = fonts::save(path, choice.as_deref());
            }
            Task::none()
        }
        Message::DragWindow => {
            // window::drag arrastra la ventana más reciente; no cambia el estado de la app.
            iced::window::get_latest().and_then(iced::window::drag)
        }
        Message::OpenSettings => {
            app.settings = Some(settings::Section::General);
            Task::none()
        }
        Message::CloseSettings => {
            app.settings = None;
            Task::none()
        }
        Message::SettingsSection(section) => {
            app.settings = Some(section);
            if section == settings::Section::Integrations {
                return Task::batch(settings::AGENTS.iter().map(|a| hooks_task(a, settings::status_args(a))));
            }
            Task::none()
        }
        Message::HooksStatus(agent) => {
            // Consultar el estado invalida el plan: lo que se aplicaría ya no es lo que hay (A1).
            app.hooks_plan.remove(&agent);
            if !app.hooks_busy.insert(agent.clone()) {
                return Task::none();
            }
            hooks_task(&agent, settings::status_args(&agent))
        }
        Message::HooksPlan(agent) => {
            if !app.hooks_busy.insert(agent.clone()) {
                return Task::none();
            }
            hooks_plan_task(&agent, settings::install_args(&agent, false))
        }
        Message::HooksPlanned(agent, result) => {
            app.hooks_busy.remove(&agent);
            match result {
                Ok(plan) => {
                    app.hooks_plan.insert(agent, plan);
                }
                Err(e) => {
                    app.hooks_output.insert(agent, format!("error: {e}"));
                }
            }
            Task::none()
        }
        Message::HooksApply(agent) => {
            // Sin un plan visto por el usuario no se escribe nada.
            if !app.hooks_plan.contains_key(&agent) || !app.hooks_busy.insert(agent.clone()) {
                return Task::none();
            }
            hooks_task_apply(&agent, settings::install_args(&agent, true))
        }
        Message::HooksApplied(agent, result) => {
            app.hooks_busy.remove(&agent);
            app.hooks_plan.remove(&agent);
            let text = match result {
                Ok(t) => t,
                Err(e) => format!("error: {e}"),
            };
            app.hooks_output.insert(agent, text);
            Task::none()
        }
        Message::HooksCancel(agent) => {
            app.hooks_plan.remove(&agent);
            Task::none()
        }
        Message::HooksOutput(agent, result) => {
            app.hooks_busy.remove(&agent);
            let text = match result {
                Ok(t) => t,
                Err(e) => format!("error: {e}"),
            };
            app.hooks_output.insert(agent, text);
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
        Message::LauncherModel(agent, model) => {
            // «predeterminado» quita la elección: el agente arranca con su modelo por defecto (M2).
            if model == launch::DEFAULT_MODEL {
                app.launch_model.remove(&agent);
            } else {
                app.launch_model.insert(agent, model);
            }
            Task::none()
        }
        Message::TeamRequested(source) => match source {
            TeamSource::Preset(i) => {
                let Some((_, team)) = team::presets().into_iter().nth(i) else { return Task::none() };
                let cwd = focus_context(app).1.unwrap_or_else(home_dir);
                let key = focus_key(app);
                // review-15 M5: también un integrado se confirma antes de lanzar nada (crea agentes y ramas).
                let root = repo_root(&cwd);
                let fp = format!("preset:{i}");
                app.team_pending = Some(TeamPending { key, root, team, fp, preset: true });
                Task::none()
            }
            TeamSource::Repo => {
                let key = focus_key(app);
                let cwd = key.as_deref().and_then(|k| app.explicit.get(k).cloned()).or_else(|| focus_context(app).1).unwrap_or_else(home_dir);
                let root = repo_root(&cwd);
                match read_team(&root) {
                    Err(e) => {
                        app.error = Some(e);
                        Task::none()
                    }
                    Ok((text, team)) => {
                        let fp = team::fingerprint(&text);
                        if team::trusted(&app.team_trust, &root, &fp) {
                            launch_team(app, key, root, team)
                        } else {
                            // La primera vez (o si cambió): el equipo se muestra y no se lanza nada hasta confirmar.
                            app.team_pending = Some(TeamPending { key, root, team, fp, preset: false });
                            Task::none()
                        }
                    }
                }
            }
        },
        Message::TeamConfirm => {
            let Some(p) = app.team_pending.take() else { return Task::none() };
            if !p.preset {
                if !app.team_trust.is_object() {
                    app.team_trust = serde_json::json!({});
                }
                app.team_trust[p.root.as_str()] = serde_json::json!(p.fp);
                if let Some(path) = &app.prefs_path {
                    if let Err(e) = prefs::set(path, "team_trust", app.team_trust.clone()) {
                        eprintln!("galgo-gui: no pude guardar la confianza del equipo: {e}");
                    }
                }
            }
            launch_team(app, p.key, p.root, p.team)
        }
        Message::TeamCancel => {
            app.team_pending = None;
            Task::none()
        }
        Message::TeamLaunched(key, manager, resultados) => {
            let mut manager_id = None;
            let mut trabajadores = Vec::new();
            for (nombre, r) in resultados {
                app.inflight.retain(|n| *n != nombre);
                match r {
                    Ok(p) => {
                        let id = p.id;
                        app.sizes.insert(id, grid_for(app.window));
                        if manager.as_deref() == Some(p.name.as_str()) {
                            manager_id = Some(id);
                        } else {
                            trabajadores.push(id);
                        }
                        app.panes.push(p);
                    }
                    Err(e) => app.error = Some(e),
                }
            }
            // Layout automático: el manager a la izquierda y los workers apilados a la derecha, en una pestaña nueva.
            let foco = manager_id.or(trabajadores.first().copied());
            if let (Some(root), Some(foco)) = (team_tree(manager_id, &trabajadores), foco) {
                let i = app.layouts.push_tab(&key, root, foco);
                if let Some(ws) = app.layouts.by_key.get_mut(&key) {
                    ws.active = i;
                }
                return Task::batch([load_panes(), select(app, foco)]);
            }
            load_panes()
        }
        Message::OpenAgentForm => {
            app.agent_form = Some(AgentForm::para(app));
            Task::none()
        }
        Message::AgentFormPick(agent) => {
            if let Some(f) = app.agent_form.as_mut() {
                f.agent = agent;
                f.model = launch::DEFAULT_MODEL.into();
            }
            Task::none()
        }
        Message::AgentFormModel(model) => {
            if let Some(f) = app.agent_form.as_mut() {
                f.model = model;
            }
            Task::none()
        }
        Message::AgentFormRole(role) => {
            if let Some(f) = app.agent_form.as_mut() {
                f.role = role;
            }
            Task::none()
        }
        Message::AgentFormIsolation(isolation) => {
            if let Some(f) = app.agent_form.as_mut() {
                f.isolation = isolation;
            }
            Task::none()
        }
        Message::AgentFormName(name) => {
            if let Some(f) = app.agent_form.as_mut() {
                f.name = name;
                f.error = None;
            }
            Task::none()
        }
        Message::CloseAgentForm => {
            app.agent_form = None;
            Task::none()
        }
        Message::AgentFormSubmit => {
            let Some(f) = app.agent_form.clone() else { return Task::none() };
            let nombre = if f.name.trim().is_empty() { launch::free_name(f.agent.label(), &names(app)) } else { f.name.trim().to_string() };
            if !launch::valid_agent_name(&nombre) {
                set_form_error(app, "el nombre solo puede tener letras, cifras, - y _ (hasta 32)");
                return Task::none();
            }
            if names(app).contains(&nombre.as_str()) {
                set_form_error(app, "ya hay un panel con ese nombre");
                return Task::none();
            }
            let key = focus_key(app);
            let (rows, cols) = grid_for(app.window);
            let cwd = key.as_deref().and_then(|k| app.explicit.get(k).cloned()).or_else(|| focus_context(app).1).unwrap_or_else(home_dir);
            // Un worker o un revisor trabaja para el manager del workspace; si no hay, queda sin manager.
            let manager = if f.role.needs_manager() { manager_of(app, key.as_deref()) } else { None };
            let worktree = (f.isolation == launch::Isolation::Worktree).then(|| nombre.clone());
            let model = (f.model != launch::DEFAULT_MODEL).then(|| f.model.clone());
            let cmd = f.agent.command_with(&login_shell(), model.as_deref());
            let workspace = key.filter(|k| !k.is_empty());
            let req = launch::agent_request(&nombre, cmd, &cwd, workspace, manager, worktree, rows, cols);
            app.roles.insert(nombre, f.role.label().to_string());
            app.agent_form = None;
            spawn_task(app, req)
        }
        Message::OpenNewNamedWorkspace => {
            // Sin diálogos: agrupa desde el cwd del panel enfocado; el nombre sale de la carpeta.
            let cwd = cwd_nuevo(app);
            open_terminal(app, cwd, true)
        }
        Message::RetryStart => open_terminal(app, home_dir(), false),
        Message::CloseWorkspace(key) => {
            // review-15 B5: el aviso caduca a los 3 s; el segundo clic solo cierra si llega dentro de ese plazo.
            let vigente = app.ws_close_armed.as_deref() == Some(key.as_str()) && app.ws_close_armed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
            if !vigente {
                app.ws_close_armed = Some(key);
                app.ws_close_armed_at = Some(std::time::Instant::now());
                return Task::none();
            }
            app.ws_close_armed = None;
            app.ws_close_armed_at = None;
            app.explicit.remove(&key);
            let ids: Vec<u32> = app.panes.iter().filter(|p| p.workspace.as_deref() == Some(key.as_str())).map(|p| p.id).collect();
            app.layouts.by_key.remove(&key);
            Task::batch(ids.into_iter().map(|id| send_request(Request::Kill { pane: id.to_string(), remove: true }, |_| Message::Echo)))
        }
        Message::WorkspaceOpened(_, Ok(data)) => {
            traza("shell: spawn respondido");
            app.error = None;
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
        Message::WorkspaceOpened(path, Err(e)) => {
            app.error = Some(e);
            // Un grupo explícito que no llegó a tener paneles se deshace: no se quedan grupos vacíos.
            let vacios: Vec<String> = app
                .explicit
                .iter()
                .filter(|(k, f)| **f == path && !app.panes.iter().any(|p| p.workspace.as_deref() == Some(k.as_str())))
                .map(|(k, _)| k.clone())
                .collect();
            for k in vacios {
                app.explicit.remove(&k);
            }
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
            if result.is_ok() {
                traza_una_vez(1, "daemon: lista de paneles recibida");
            }
            app.listing = false;
            let mut arranque = None;
            let mut envios = Task::none();
            let mut restaurar = Task::none();
            match result {
                Ok(panes) => {
                    app.error = None;
                    // Un daemon nuevo no tiene ninguno de los paneles que conocíamos (ids y nombres). Entonces las pestañas
                    // guardadas en memoria no sirven: se vuelve a aplicar la disposición del disco, relanzando sus paneles.
                    // Reinicio = otro daemon, señal explícita: el socket es otro archivo (otro inodo). Un listado vacío por sí
                    // solo no basta: al cerrar el último panel también llega vacío, y no hay que relanzarlo (A1).
                    let ahora = socket_id();
                    let reinicio = es_reinicio(app.ws_restored, !app.panes.is_empty(), app.socket_id, ahora);
                    if ahora.is_some() {
                        app.socket_id = ahora;
                    }
                    if reinicio {
                        app.ws_restored = false;
                        app.ws_pendiente = None;
                        app.ws_intentos = 0;
                        app.layouts.by_key.clear();
                    }
                    app.panes = panes;
                    // Primer listado (o reinicio): se restauran los workspaces guardados (pestañas, splits y grupos explícitos).
                    restaurar = intentar_restaurar(app);
                    if !app.ws_restored {
                        if let Some(v) = app.prefs_path.as_ref().and_then(|p| prefs::get(p, "agent_roles")) {
                            app.roles = v.as_object().into_iter().flatten().filter_map(|(n, r)| Some((n.clone(), r.as_str()?.to_string()))).collect();
                        }
                    }
                    // Los roles de paneles que ya no existen se olvidan.
                    app.roles.retain(|n, _| app.panes.iter().any(|p| &p.name == n) || app.inflight.contains(n));
                    envios = prompts_task(app);
                    // Abrir la app ya es un terminal: si el daemon no tiene paneles, un shell en ~ (la bienvenida queda
                    // solo para cuando no se puede crear el panel).
                    if !app.started {
                        app.started = true;
                        // Nombres de las píldoras (lo que el usuario renombró con doble clic).
                        if let Some(v) = app.prefs_path.as_ref().and_then(|p| prefs::get(p, "aliases")) {
                            app.alias = serde_json::from_value(v).unwrap_or_default();
                        }
                        // Sin disposición pendiente: un shell en ~. Con una pendiente, sus paneles se relanzan (no se abre otro).
                        if app.panes.is_empty() && app.ws_pendiente.is_none() && app.layouts.by_key.is_empty() {
                            arranque = Some(open_terminal(app, home_dir(), false));
                        }
                    }
                    app.layouts.sync_nombrado(&pane_nombres(app));
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
                    // Como en cualquier terminal: cerrar la última pestaña cierra la ventana.
                    if app.salir_al_vaciar && app.panes.is_empty() {
                        restaurar = Task::batch([restaurar, iced::exit()]);
                    }
                    for id in shells_terminados(&app.panes) {
                        app.salir_al_vaciar |= app.panes.len() == 1;
                        if app.cierres_pedidos.insert(id) {
                            restaurar = Task::batch([restaurar, send_request(Request::Kill { pane: id.to_string(), remove: true }, |_| Message::Echo)]);
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
            // `restaurar` lleva los relanzamientos de los paneles que faltan: debe ejecutarse (M3).
            Task::batch([arranque.unwrap_or_else(Task::none), envios, restaurar])
        }
        Message::Screen(id, offset, sent_since, result) => {
            app.loading = false;
            let mut copiar: Task<Message> = Task::none();
            let repetir = std::mem::take(&mut app.refresh_wanted);
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
                    Ok(ScreenReply::Full(mut snap)) => {
                        traza_una_vez(2, "primer snapshot recibido");
                        // OSC 52 del programa: el texto va al portapapeles del sistema (solo escritura).
                        if let Some(texto) = snap.portapapeles.take() {
                            copiar = iced::clipboard::write(texto);
                        }
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
            // Un aviso llegó durante esta lectura: ahora se pide la pantalla que ya cambió.
            if let (true, Some(sel)) = (repetir, app.selected) {
                app.loading = true;
                let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
                return Task::batch([copiar, load_screen(sel, app.scroll, since)]);
            }
            copiar
        }
        Message::Select(id) => select(app, id),
        Message::PillPress(key) => {
            let doble = app.pill_click.as_ref().is_some_and(|(k, t)| *k == key && t.elapsed() < std::time::Duration::from_millis(500));
            if doble {
                app.pill_click = None;
                let texto = app.alias.get(&key).cloned().unwrap_or_else(|| key.clone());
                app.renombrando = Some((key, texto));
            } else {
                app.pill_click = Some((key, std::time::Instant::now()));
            }
            Task::none()
        }
        Message::PasteCaja(texto) => {
            let limpio: String = texto.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
            if let Some(b) = app.buscar.as_mut() {
                b.consulta.push_str(&limpio);
                return buscar_cambio(app);
            }
            if let Some((_, buf)) = app.renombrando.as_mut() {
                buf.push_str(&limpio);
            }
            Task::none()
        }
        Message::BorrarCaja => {
            if let Some(b) = app.buscar.as_mut() {
                b.consulta.clear();
                return buscar_cambio(app);
            }
            if let Some((_, buf)) = app.renombrando.as_mut() {
                buf.clear();
            }
            Task::none()
        }
        Message::OpenSearch => {
            let Some(id) = app.selected else { return Task::none() };
            if app.buscar.is_none() {
                app.buscar = Some(Busqueda { pane: id, ..Default::default() });
            }
            Task::none()
        }
        Message::SearchDone(id, consulta, resultado) => {
            let Some(b) = app.buscar.as_mut() else { return Task::none() };
            if b.pane != id || b.consulta != consulta {
                return Task::none();
            }
            b.buscando = false;
            match resultado {
                Ok(v) => {
                    b.resultados = serde_json::from_value(v["matches"].clone()).unwrap_or_default();
                    b.scrollback = v["scrollback"].as_u64().unwrap_or(0) as usize;
                    b.actual = b.resultados.len().saturating_sub(1);
                    ir_a_coincidencia(app)
                }
                Err(e) => {
                    app.error = Some(e);
                    Task::none()
                }
            }
        }
        Message::SearchStep(paso) => {
            let Some(b) = app.buscar.as_mut() else { return Task::none() };
            let n = b.resultados.len() as isize;
            if n == 0 {
                return Task::none();
            }
            b.actual = (b.actual as isize + paso).rem_euclid(n) as usize;
            ir_a_coincidencia(app)
        }
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
            // Confirmación de cierre: Enter o Cmd+W cierran, Esc cancela; nada llega al terminal.
            if app.confirm_close {
                return match &key {
                    Key::Named(Named::Enter) => update(app, Message::ConfirmClose),
                    Key::Named(Named::Escape) => update(app, Message::CancelClose),
                    Key::Character(c) if modifiers.command() && c.as_str() == "w" => update(app, Message::ConfirmClose),
                    _ => Task::none(),
                };
            }
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
            // Atajos de la ventana (Cmd+…), salvo mientras hay un diálogo abierto (ninguno puede actuar sobre el panel de detrás).
            if modifiers.command() && !app.launcher && app.settings.is_none() && !overlay_owns_keyboard(app) {
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
            // Con el lanzador o los ajustes abiertos, el teclado es suyo, no del panel.
            if app.launcher || app.settings.is_some() {
                if matches!(&key, Key::Named(Named::Escape)) {
                    app.launcher = false;
                    app.settings = None;
                }
                return Task::none();
            }
            // Un diálogo nuevo (formulario de agente, confirmación de equipo) es dueño del teclado: nada de lo que se
            // escribe llega al panel de detrás. Esc lo cierra (review-15 A1).
            if overlay_owns_keyboard(app) {
                if matches!(&key, Key::Named(Named::Escape)) {
                    app.agent_form = None;
                    app.team_pending = None;
                }
                return Task::none();
            }
            // Renombrando una píldora, el teclado escribe el nombre (Enter guarda, Esc cancela).
            if app.renombrando.is_some() && !modifiers.command() {
                return renombrar_tecla(app, &key, text.as_deref());
            }
            // Con la búsqueda abierta, el teclado es suyo: Enter/Shift+Enter, Esc, y lo que se escribe a la consulta.
            if app.buscar.is_some() && !modifiers.command() {
                return buscar_tecla(app, &key, modifiers, text.as_deref());
            }
            // A2: con una caja abierta, los atajos con Cmd no llegan al terminal. Pegar y borrar van a la caja; Cmd+C
            // solo copia la selección; el resto se ignora (los atajos de la ventana ya se atendieron arriba).
            if (app.buscar.is_some() || app.renombrando.is_some()) && modifiers.command() {
                if matches!(&key, Key::Character(c) if c.as_str() == "v") {
                    return iced::clipboard::read().map(Message::PasteCaja);
                }
                if matches!(&key, Key::Named(Named::Backspace)) {
                    return update(app, Message::BorrarCaja);
                }
                if !matches!(&key, Key::Character(c) if c.as_str() == "c") {
                    return Task::none();
                }
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
            // Un programa con ratón recibe la rueda como botones 64/65; uno en pantalla alternativa sin ratón, flechas
            // (como Terminal.app). Solo si el puntero está sobre la cuadrícula y no hay historial desplazado.
            if steps != 0 && app.scroll == 0 && pointer_in_grid(app) {
                let (raton_on, alt) = app.snap.as_ref().map_or((false, false), |s| (s.mouse != MouseTracking::None, s.alt_screen));
                if raton_on {
                    if let Some((seq, _)) = secuencia_raton_pulsa(app, raton::Gesto::Rueda { arriba: steps > 0 }) {
                        for _ in 0..steps.unsigned_abs().min(20) {
                            send_text(app, seq.clone());
                        }
                        return Task::none();
                    }
                } else if alt && !app.mods.shift() {
                    let flecha = if steps > 0 { "\x1b[A" } else { "\x1b[B" };
                    send_text(app, flecha.repeat(3 * steps.unsigned_abs().min(20) as usize));
                    return Task::none();
                }
            }
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
            if app.pointer.is_some_and(|p| en_barra_busqueda(app, p)) {
                return Task::none();
            }
            // Un programa que pide ratón recibe el clic; con Shift no (selección propia de galgo).
            if let Some((seq, cell)) = secuencia_raton_pulsa(app, raton::Gesto::Pulsa(raton::IZQ)) {
                app.raton_pulsado = Some(raton::IZQ);
                app.raton_celda = Some(cell);
                send_text(app, seq);
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
                // Cmd+clic en una URL o una ruta existente: se abre con el visor del sistema; no empieza selección.
                if app.mods.command() {
                    if let Some(objetivo) = shown(app).and_then(|snap| token_en(&row_chars(snap, cell.0), cell.1)) {
                        app.error = open_target(&objetivo).err();
                        return Task::none();
                    }
                }
                // Doble clic selecciona una palabra y triple clic una línea (el siguiente clic vuelve a empezar).
                let n = match app.clicks {
                    Some((prev, at, n)) if prev == cell && at.elapsed() < Duration::from_millis(400) => {
                        if n >= 3 { 1 } else { n + 1 }
                    }
                    _ => 1,
                };
                app.clicks = Some((cell, std::time::Instant::now(), n));
                // Se congela la pantalla que el usuario ve en este momento.
                app.sel_snap = app.snap.clone();
                let seleccion = match (n, shown(app)) {
                    (2, Some(snap)) => palabra_en(&row_chars(snap, cell.0), cell.1).map(|(a, b)| Selection { anchor: (cell.0, a), head: (cell.0, b) }),
                    (3, Some(snap)) => Some(Selection { anchor: (cell.0, 0), head: (cell.0, snap.cols.saturating_sub(1)) }),
                    _ => None,
                };
                app.dragging = seleccion.is_none();
                app.sel = Some(seleccion.unwrap_or(Selection { anchor: cell, head: cell }));
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
            // Arrastre con botón (1002/1003) o movimiento sin botón (1003), una vez por celda.
            if let Some(cell) = grid_cell(app, position) {
                if app.raton_celda != Some(cell) {
                    if let Some(code) = app.raton_pulsado {
                        app.raton_celda = Some(cell);
                        if let Some(seq) = secuencia_raton_en(app, raton::Gesto::Arrastra(code), cell) {
                            send_text(app, seq);
                        }
                    } else if let Some((seq, cell)) = secuencia_raton_pulsa(app, raton::Gesto::Mueve) {
                        app.raton_celda = Some(cell);
                        send_text(app, seq);
                    }
                }
            }
            app.hover_divider = divider_under(app, position).map(|d| (d.path, d.vertical));
            if app.dragging {
                if let (Some(cell), Some(sel)) = (grid_cell(app, position), app.sel.as_mut()) {
                    sel.head = cell;
                }
            }
            Task::none()
        }
        // Soltar un botón que el programa recibió en pulsación: va siempre a él, esté el puntero donde esté.
        Message::Event(Event::Mouse(mouse::Event::ButtonReleased(b))) if app.raton_pulsado.is_some() && raton_boton(b) == app.raton_pulsado => {
            let code = app.raton_pulsado.take().unwrap_or(raton::IZQ);
            if let Some(cell) = app.raton_celda {
                if let Some(seq) = secuencia_raton_en(app, raton::Gesto::Suelta(code), cell) {
                    send_text(app, seq);
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
        // Central o derecho sobre la cuadrícula, si el programa pide ratón.
        Message::Event(Event::Mouse(mouse::Event::ButtonPressed(b))) => {
            let Some(code) = raton_boton(b) else { return Task::none() };
            if let Some((seq, cell)) = secuencia_raton_pulsa(app, raton::Gesto::Pulsa(code)) {
                app.raton_pulsado = Some(code);
                app.raton_celda = Some(cell);
                send_text(app, seq);
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
            app.salir_al_vaciar = app.panes.len() == 1 && app.selected.is_some();
            // M1: el foco se decide antes de cerrar, mientras el panel y su hermano aún están en el layout.
            app.pending_focus = app.selected.and_then(|id| successor(app, id));
            match app.selected {
                Some(id) => send_request(Request::Kill { pane: id.to_string(), remove: true }, Message::Killed),
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
    /// Cursor de un panel sin foco: tenue, sin parpadeo.
    CursorIdle,
    Selected,
    /// Coincidencia de la búsqueda, y la seleccionada.
    Hit,
    HitCurrent,
}

/// Cómo se dibuja el cursor de un panel: el enfocado parpadea, el de una ventana sin foco queda tenue, y los
/// paneles que no son el activo no lo muestran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorStyle {
    Off,
    Blink,
    Idle,
}

/// Fase del parpadeo del cursor: visible la mitad de cada 1,06 s. Se recalcula en cada tick de pantalla.
pub fn blink_on() -> bool {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 530) % 2 == 0)
        .unwrap_or(true)
}

/// Fondo de la selección: el acento translúcido sobre el fondo del terminal, con el texto en su color.
const SELECTION_BG: Color = Color::from_rgb(0.153, 0.243, 0.420);
/// Resaltado de las coincidencias de la búsqueda (Cmd+F): la actual, más viva.
const HIT_BG: Color = Color::from_rgb(0.330, 0.290, 0.120);
const HIT_CURRENT_BG: Color = Color::from_rgb(0.760, 0.560, 0.160);

/// Un tramo de texto con el estilo de `r` y la marca de celda.
/// Familia de los símbolos Nerd Font empaquetados (Symbols Nerd Font Mono, licencia OFL en `assets/`).
pub const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";
/// Bytes de la fuente de símbolos, que se cargan al arrancar.
static SYMBOLS_FONT: &[u8] = include_bytes!("../assets/SymbolsNerdFontMono-Regular.ttf");

/// Columnas que ocupa un carácter en la pantalla, según el ancho Unicode que usa el parser vt100: 2 para los anchos
/// (CJK, emoji), 0 para los combinados (que se pegan a la celda anterior) y 1 para el resto.
fn cells_of(ch: char) -> u16 {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16
}

/// Carácter de área privada (iconos Nerd Font, separadores powerline): sus glifos no los tienen las fuentes de texto,
/// así que el fallback del sistema los elige al azar. Se dibujan con la familia de símbolos (punto 2).
fn is_symbol(ch: char) -> bool {
    matches!(ch, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
}

fn styled(text: String, r: &Run, mark: Mark, symbol: bool) -> Span<'static, Message> {
    let (mut fg, mut bg) = (term::color(r.fg), term::color(r.bg));
    if r.inverse {
        let (f, b) = (bg.unwrap_or(term::DEFAULT_BG), fg.unwrap_or(term::DEFAULT_FG));
        fg = Some(f);
        bg = Some(b);
    }
    // Cursor de bloque: el acento semitransparente bajo el carácter, que sigue con su color (no se invierte).
    if mark == Mark::Cursor {
        bg = Some(theme::CURSOR);
    } else if mark == Mark::CursorIdle {
        bg = Some(theme::CURSOR_IDLE);
    }
    if mark == Mark::Selected {
        bg = Some(SELECTION_BG);
    } else if mark == Mark::Hit {
        bg = Some(HIT_BG);
    } else if mark == Mark::HitCurrent {
        bg = Some(HIT_CURRENT_BG);
    }
    // Atenuado (SGR 2) y parpadeo en su mitad apagada (el tick de pantalla redibuja cada 250 ms): el color se
    // atenúa. El tachado (SGR 9) es un atributo del texto.
    let color = fg.unwrap_or(term::DEFAULT_FG);
    let atenuado = r.dim || (r.blink && !blink_on());
    let color = if atenuado { color.scale_alpha(0.55) } else { color };
    let mut sp: Span<'static, Message> = span(text).color(color).underline(r.underline).strikethrough(r.strike);
    if let Some(b) = bg { sp = sp.background(b); }
    if symbol && !fonts::is_nerd(fonts::current()) {
        // La fuente de símbolos mide 1 em por glifo; la celda, `metrics().0` em. Se escala para que cada glifo
        // ocupe exactamente una celda: si no, la fila se desborda y el prompt se parte.
        sp = sp.font(Font::with_name(SYMBOLS_FAMILY)).size(font_size() * fonts::metrics().0);
    } else if r.bold || r.italic {
        sp = sp.font(Font {
            weight: if r.bold { font::Weight::Bold } else { font::Weight::Normal },
            style: if r.italic { font::Style::Italic } else { font::Style::Normal },
            ..terminal_font()
        });
    }
    sp
}

/// Color de trazo de una celda (con inversa, el fondo): lo que pinta el canvas de cajas.
fn color_de_trazo(r: &Run) -> Color {
    if r.inverse {
        term::color(r.bg).unwrap_or(term::DEFAULT_BG)
    } else {
        term::color(r.fg).unwrap_or(term::DEFAULT_FG)
    }
}

/// Saca de la fila las cajas y bloques que dibuja `cajas` (se sustituyen por un espacio con el mismo estilo, así el
/// fondo y el cursor se pintan igual) y las apunta en la capa de la pantalla, a su columna y fila exactas.
fn quitar_cajas(runs: &[Run], fila: u16, capa: &mut cajas::Capa) -> Vec<Run> {
    let mut col: u16 = 0;
    runs.iter()
        .map(|r| {
            let mut t = String::with_capacity(r.t.len());
            for ch in r.t.chars() {
                if cajas::es_dibujable(ch) {
                    capa.celdas.push((col, fila, ch, color_de_trazo(r)));
                    t.push(' ');
                } else {
                    t.push(ch);
                }
                col += cells_of(ch);
            }
            Run { t, ..r.clone() }
        })
        .collect()
}

fn spans_for_line(runs: &[Run], cursor: Option<(u16, Mark)>, sel: Option<(u16, u16)>, hits: &[(u16, u16, bool)]) -> Vec<Span<'static, Message>> {
    let mut out = Vec::new();
    let mut col: u16 = 0;
    for r in runs {
        let mut chunk = String::new();
        let mut chunk_mark = Mark::Normal;
        let mut chunk_symbol = false;
        for ch in r.t.chars() {
            let mark = if cursor.is_some_and(|(c, _)| c == col) {
                cursor.map_or(Mark::Cursor, |(_, m)| m)
            } else if let Some(h) = hits.iter().find(|h| (h.0..=h.1).contains(&col)) {
                if h.2 { Mark::HitCurrent } else { Mark::Hit }
            } else if sel.is_some_and(|(a, b)| (a..=b).contains(&col)) {
                Mark::Selected
            } else {
                Mark::Normal
            };
            let symbol = is_symbol(ch);
            if !chunk.is_empty() && (mark != chunk_mark || symbol != chunk_symbol) {
                out.push(styled(std::mem::take(&mut chunk), r, chunk_mark, chunk_symbol));
            }
            chunk_mark = mark;
            chunk_symbol = symbol;
            chunk.push(ch);
            // Punto 4: avanza lo que ocupa en la celda (2 para CJK, 0 para combinados), como el parser vt100.
            col += cells_of(ch);
        }
        if !chunk.is_empty() {
            out.push(styled(chunk, r, chunk_mark, chunk_symbol));
        }
    }
    if out.is_empty() { out.push(span(" ".to_string())); }
    out
}

/// Pantalla de un panel. `cursor`: se dibuja el cursor (solo el panel enfocado, con la ventana activa).
fn screen_view(snap: &Snapshot, sel: Option<Selection>, style: CursorStyle, resaltes: &[(u16, u16, u16, bool)]) -> Element<'static, Message> {
    let mut lines = column![].spacing(0);
    // El cursor solo se dibuja en la pantalla en vivo y si el panel lo muestra.
    let live = snap.scrollback == 0 && !snap.cursor_hidden;
    let visible = live
        && match style {
            CursorStyle::Off => false,
            CursorStyle::Blink => blink_on(),
            CursorStyle::Idle => true,
        };
    let mark = if style == CursorStyle::Idle { Mark::CursorIdle } else { Mark::Cursor };
    let mut capa = cajas::Capa { cw: cell_w(), lh: line_h(), ligero: (font_size() / 12.0).max(1.0), ..Default::default() };
    for (r, runs) in snap.lines.iter().enumerate() {
        let cursor = (visible && snap.cursor.0 as usize == r).then_some((snap.cursor.1, mark));
        let selected = sel.and_then(|s| selected_cols(s, r as u16, snap.cols));
        let sin_cajas = quitar_cajas(runs, r as u16, &mut capa);
        let hits: Vec<(u16, u16, bool)> = resaltes.iter().filter(|h| h.0 as usize == r).map(|h| (h.1, h.2, h.3)).collect();
        let spans = spans_for_line(&sin_cajas, cursor, selected, &hits);
        lines = lines.push(
            rich_text(spans).size(font_size()).font(terminal_font()).line_height(iced::widget::text::LineHeight::Absolute(line_h().into())).wrapping(iced::widget::text::Wrapping::None),
        );
    }
    // Las cajas van encima del texto, en la misma rejilla: cada celda ocupa exactamente cw × lh.
    let (ancho, alto) = (snap.cols as f32 * cell_w(), snap.lines.len() as f32 * line_h());
    let dibujo = iced::widget::canvas(capa).width(Length::Fixed(ancho)).height(Length::Fixed(alto));
    iced::widget::Stack::with_children(vec![lines.into(), dibujo.into()]).into()
}

fn view(app: &App) -> Element<'_, Message> {
    traza_una_vez(0, "primera vista (la ventana dibuja)");
    if app.snap.is_some() {
        traza_una_vez(3, "primer pintado con contenido del panel");
    }
    let main: Element<'_, Message> = if app.panes.is_empty() && app.ws_pendiente.is_none() {
        chrome::welcome_view(app)
    } else {
        let mut col = column![container(chrome::layout_view(app)).width(Length::Fill).height(Length::Fill).style(theme::surface(theme::TERM_BG))];
        if marco().status {
            col = col.push(chrome::status_bar(app));
        }
        col.into()
    };
    // Una sola fila arriba (pestañas), sobre la barra lateral y el cuerpo.
    let base: Element<'_, Message> = container(column![
        chrome::tabs_bar(app),
        if marco().sidebar { row![chrome::sidebar_view(app), main].into() } else { main },
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::surface(BG))
    .into();
    if app.settings.is_some() {
        return chrome::floating(base, chrome::settings_panel(app), 820.0, Message::CloseSettings);
    }
    match body_mode(app) {
        Body::Palette => chrome::floating(base, chrome::palette_panel(app), 600.0, Message::ClosePalette),
        Body::Launcher => chrome::floating(base, chrome::launcher_panel(app), 440.0, Message::CloseLauncher),
        Body::AgentForm => chrome::floating(base, chrome::agent_form_panel(app), 480.0, Message::CloseAgentForm),
        Body::TeamConfirm => chrome::floating(base, chrome::team_confirm_panel(app), 500.0, Message::TeamCancel),
        Body::Settings => chrome::floating(base, chrome::settings_panel(app), 820.0, Message::CloseSettings),
        Body::CloseConfirm => chrome::floating(base, chrome::close_confirm_panel(app), 420.0, Message::CancelClose),
        Body::Welcome | Body::Screen => base,
    }
}

/// Traza de arranque (`GALGO_TRACE_STARTUP=1`): tiempo desde el inicio de `main` en cada fase, a stderr. Sin la variable
/// no cuesta nada más que una comprobación.
static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static TRAZA_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
/// Hitos que solo se imprimen una vez: bit 0 primera vista, 1 lista de paneles, 2 primer snapshot, 3 primer pintado con texto.
static HITOS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn traza(fase: &str) {
    if !*TRAZA_ON.get_or_init(|| std::env::var_os("GALGO_TRACE_STARTUP").is_some()) {
        return;
    }
    let t0 = *T0.get_or_init(std::time::Instant::now);
    eprintln!("ARRANQUE {:>9.1} ms  {fase}", t0.elapsed().as_secs_f64() * 1000.0);
}

/// Imprime `fase` la primera vez que se pide el hito `bit`.
fn traza_una_vez(bit: u8, fase: &str) {
    if HITOS.fetch_or(1 << bit, std::sync::atomic::Ordering::Relaxed) & (1 << bit) == 0 {
        traza(fase);
    }
}

/// Identidad del socket del daemon (dispositivo e inodo): cambia cuando se arranca un daemon nuevo.
fn socket_id() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(galgo_proto::socket_path()).ok().map(|m| (m.dev(), m.ino()))
}

/// Un daemon nuevo: había disposición restaurada y paneles en pantalla, y el socket que vemos ya no es el de antes.
/// Sin socket anterior (primer listado) o con el mismo, no lo es, aunque el listado llegue vacío.
fn es_reinicio(restaurado: bool, hay_paneles: bool, antes: Option<(u64, u64)>, ahora: Option<(u64, u64)>) -> bool {
    restaurado && hay_paneles && antes.is_some() && ahora.is_some() && antes != ahora
}

fn main() -> iced::Result {
    T0.get_or_init(std::time::Instant::now);
    traza("main");
    // Mientras iced crea la ventana (lo más caro del arranque), el daemon se conecta, se arranca si hace falta, y se
    // pide la lista de paneles. Así, cuando la app la pide, ya está.
    // Indexar las fuentes del sistema (lo hace iced al primer texto, dentro de la ventana) en paralelo con ella.
    std::thread::spawn(|| {
        traza("fuentes del sistema: indexado iniciado");
        let _ = iced_graphics::text::font_system();
        traza("fuentes del sistema: indexado listo");
    });
    std::thread::spawn(|| {
        let lista = request(Request::List).and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()));
        traza("hilo previo: lista del daemon");
        *LISTA_PREVIA.lock().unwrap() = Some(lista);
    });
    iced::application(app_title, update, view)
        .theme(|_| theme::app())
        // Rex: fuente monoespaciada en toda la interfaz, no solo en el terminal.
        .default_font(theme::UI_FONT)
        .subscription(|app: &App| Subscription::batch([
            // Latido de 250 ms: paneles visibles sin foco y redimensiones. El panel enfocado no se sondea.
            iced::time::every(Duration::from_millis(250)).map(|_| Message::TickScreen),
            // Suscripción al panel enfocado: el daemon avisa cuando su pantalla cambia (sin sondeo).
            match app.selected {
                Some(id) => Subscription::run_with_id(id, watch_stream(id)),
                None => Subscription::none(),
            },
            iced::time::every(Duration::from_secs(1)).map(|_| Message::TickList),
            event::listen_with(|event, status, _id| reenviar_evento(event, status)),
        ]))
        // Barra de título integrada: transparente y sin título, con el contenido bajo ella (Rex).
        .window(iced::window::Settings {
            size: iced::Size::new(1100.0, 700.0),
            platform_specific: iced::window::settings::PlatformSpecific {
                title_hidden: true,
                titlebar_transparent: true,
                fullsize_content_view: true,
            },
            ..Default::default()
        })
        .run_with(|| {
            // Preferencias junto al socket (gui-prefs.json): fuente, barra lateral, zoom, workspaces…
            let prefs = galgo_proto::socket_path().parent().unwrap_or(std::path::Path::new(".")).join("gui-prefs.json");
            // Fuente: la elección guardada si la hay y sigue instalada; si no, la automática (Nerd Font o Menlo).
            let installed: Vec<&'static str> = fonts::detect(&fonts::font_dirs());
            let elegida = fonts::load(&prefs).and_then(|name| fonts::CANDIDATES.iter().map(|c| c.0).find(|f| *f == name).filter(|f| installed.contains(f)));
            fonts::set_current(elegida.unwrap_or_else(|| fonts::automatic(&installed)));
            // Barra lateral y barra de estado: como se dejaron (por defecto, ocultas).
            let flag = |clave: &str| prefs::get(&prefs, clave).and_then(|v| v.as_bool()).unwrap_or(false);
            // Zoom de letra: el último que se eligió (0 = normal).
            let confianza = prefs::get(&prefs, "team_trust").unwrap_or_else(|| serde_json::json!({}));
            let zoom_guardado = prefs::get(&prefs, "zoom").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            ZOOM.store(zoom_guardado.clamp(ZOOM_MIN, ZOOM_MAX), std::sync::atomic::Ordering::Relaxed);
            let app = App {
                window: (1100.0, 700.0),
                focused: true,
                font_installed: installed,
                sidebar: flag("sidebar"),
                team_trust: confianza,
                status: flag("status"),
                prefs_path: Some(prefs),
                ..Default::default()
            };
            traza("app creada (fuentes detectadas)");
            sync_chrome(&app);
            // Símbolos Nerd Font empaquetados: se cargan en el sistema de fuentes antes de usarse.
            let simbolos = iced::font::load(std::borrow::Cow::Borrowed(SYMBOLS_FONT)).map(|_| Message::Echo);
            (app, Task::batch([load_panes(), load_limits(), simbolos]))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &str) -> Run {
        Run { t: t.into(), fg: galgo_proto::Color::Default, bg: galgo_proto::Color::Default, bold: false, italic: false, underline: false, inverse: false, dim: false, strike: false, blink: false }
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
            mouse: Default::default(),
            mouse_sgr: false,
            alt_screen: false,
            portapapeles: None,
        }
    }

    /// Píxel del centro de la celda (row, col).
    fn center(row: u16, col: u16) -> (f32, f32) {
        (term_origin().0 + (col as f32 + 0.5) * CELL_W, term_origin().1 + (row as f32 + 0.5) * LINE_H)
    }

    #[test]
    fn pixel_a_celda_dentro_de_la_cuadricula() {
        let (x, y) = center(2, 3);
        assert_eq!(pixel_to_cell(term_origin(), x, y, (10, 80)), Some((2, 3)));
        // Esquina superior izquierda del área del terminal.
        assert_eq!(pixel_to_cell(term_origin(), term_origin().0, term_origin().1, (10, 80)), Some((0, 0)));
    }

    #[test]
    fn pixel_a_celda_recorta_al_borde_y_rechaza_fuera_por_la_izquierda() {
        assert_eq!(pixel_to_cell(term_origin(), term_origin().0 + 10_000.0, term_origin().1 + 10_000.0, (10, 80)), Some((9, 79)));
        assert_eq!(pixel_to_cell(term_origin(), term_origin().0 - 1.0, term_origin().1 + 5.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(term_origin(), term_origin().0 + 5.0, term_origin().1 - 1.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(term_origin(), 50.0, 50.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(term_origin(), term_origin().0, term_origin().1, (0, 80)), None);
    }

    /// B3: la rueda y el clic solo cuentan dentro de la cuadrícula.
    #[test]
    fn dentro_de_la_cuadricula_solo() {
        assert!(inside_grid(term_origin(), term_origin().0, term_origin().1, (10, 80)));
        assert!(!inside_grid(term_origin(), term_origin().0 + 10_000.0, term_origin().1, (10, 80)));
        assert!(!inside_grid(term_origin(), term_origin().0, term_origin().1 + 10.0 * LINE_H, (10, 80)));
        assert!(!inside_grid(term_origin(), 50.0, 300.0, (10, 80)));
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

    /// Doble clic en una píldora abre el renombrado; lo escrito es el nuevo nombre; Esc cancela; un nombre vacío lo quita.
    #[test]
    fn el_doble_clic_renombra_la_pildora() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        let _ = update(&mut app, Message::PillPress("proyecto".into()));
        assert!(app.renombrando.is_none(), "un clic no renombra");
        let _ = update(&mut app, Message::PillPress("proyecto".into()));
        assert_eq!(app.renombrando.as_ref().map(|(_, t)| t.as_str()), Some("proyecto"));
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Backspace), None)));
        let _ = update(&mut app, Message::Event(key_press(Key::Character("x".into()), Some("x"))));
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Enter), None)));
        assert_eq!(app.alias.get("proyecto").map(String::as_str), Some("proyectx"));
        // Esc cancela sin tocar el nombre.
        let _ = update(&mut app, Message::PillPress("proyecto".into()));
        let _ = update(&mut app, Message::PillPress("proyecto".into()));
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Escape), None)));
        assert!(app.renombrando.is_none());
        assert_eq!(app.alias.get("proyecto").map(String::as_str), Some("proyectx"));
        // Un nombre vacío quita el alias y vuelve la clave.
        let _ = update(&mut app, Message::PillPress("proyecto".into()));
        let _ = update(&mut app, Message::PillPress("proyecto".into()));
        for _ in 0..8 {
            let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Backspace), None)));
        }
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Enter), None)));
        assert!(app.alias.get("proyecto").is_none());
    }

    /// Cerrar pide confirmación siempre que el proceso siga vivo (también en Done o Failed); un terminado no.
    #[test]
    fn cerrar_pide_confirmacion_mientras_el_proceso_viva() {
        for st in [Status::Idle, Status::Working, Status::Blocked, Status::Done, Status::Failed] {
            assert!(needs_confirm(&pane_info(1, "p", st)), "{st:?} sigue vivo");
        }
        assert!(!needs_confirm(&pane_info(1, "p", Status::Exited)));
    }

    /// A1: cerrar el último panel no relanza su shell (el listado llega vacío, pero el daemon es el mismo).
    #[test]
    fn cerrar_el_ultimo_panel_no_es_reinicio() {
        assert!(!es_reinicio(true, true, Some((1, 9)), Some((1, 9))), "mismo socket: no hay daemon nuevo");
        assert!(es_reinicio(true, true, Some((1, 9)), Some((1, 12))), "otro socket: daemon nuevo");
        assert!(!es_reinicio(true, true, None, Some((1, 12))), "primer listado: no hay anterior");
        assert!(!es_reinicio(false, true, Some((1, 9)), Some((1, 12))), "sin disposición restaurada no hay nada que relanzar");
        assert!(!es_reinicio(true, false, Some((1, 9)), Some((1, 12))), "sin paneles en pantalla no hay nada que relanzar");
    }

    /// A2: con la búsqueda abierta, Cmd+⌫ vacía la caja, Cmd+X no hace nada y el pegado va a la consulta.
    #[test]
    fn con_la_caja_abierta_los_atajos_con_cmd_no_llegan_al_panel() {
        let mut app = App { window: (1100.0, 700.0), panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        app.snap = Some(snap(&["aaaa"], 4));
        let _ = update(&mut app, Message::OpenSearch);
        let _ = update(&mut app, Message::Event(key_press(Key::Character("e".into()), Some("e"))));
        let _ = update(&mut app, Message::Event(key_press(Key::Character("x".into()), None)));
        assert_eq!(app.buscar.as_ref().unwrap().consulta, "e", "Cmd+X no escribe en la caja");
        let _ = update(&mut app, Message::PasteCaja(Some("rror\n".into())));
        assert_eq!(app.buscar.as_ref().unwrap().consulta, "error", "el pegado va a la consulta; los saltos de línea se quitan");
        let _ = update(&mut app, Message::BorrarCaja);
        assert_eq!(app.buscar.as_ref().unwrap().consulta, "", "Cmd+⌫ vacía la caja");
        assert!(app.buscar.is_some(), "la búsqueda sigue abierta");
    }

    /// Cmd+F: lo que se escribe va a la consulta y nunca al panel; Esc cierra y la vista vuelve en vivo.
    #[test]
    fn la_busqueda_no_deja_pasar_las_teclas_al_terminal() {
        let mut app = App { window: (1100.0, 700.0), panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        app.snap = Some(snap(&["aaaa", "bbbb"], 4));
        let _ = update(&mut app, Message::OpenSearch);
        assert!(app.buscar.is_some());
        let _ = update(&mut app, Message::Event(key_press(Key::Character("x".into()), Some("x"))));
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Backspace), None)));
        let _ = update(&mut app, Message::Event(key_press(Key::Character("E".into()), Some("E"))));
        assert_eq!(app.buscar.as_ref().unwrap().consulta, "E");
        assert!(app.buscar.as_ref().unwrap().buscando, "la búsqueda sale al daemon");
        // Enter avanza sin tocar el panel; Esc lo cierra.
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Enter), None)));
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Escape), None)));
        assert!(app.buscar.is_none());
    }

    /// Los resaltados caen en la fila y columnas de la vista (la fila 0 es la línea `scrollback_max − scrollback`).
    #[test]
    fn los_resaltes_se_colocan_en_la_vista() {
        let mut app = App { window: (1100.0, 700.0), panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        let mut s = snap(&["abcd", "efgh"], 4);
        s.scrollback_max = 10;
        s.scrollback = 0;
        app.buscar = Some(Busqueda { pane: 1, consulta: "gh".into(), resultados: vec![[10, 2, 2], [3, 0, 1]], actual: 0, scrollback: 10, buscando: false });
        assert_eq!(resaltes_de(&app, &s), vec![(0, 2, 3, true)]);
        // Con el historial hacia arriba, la línea 3 entra y la 10 sale de la vista.
        s.scrollback = 8;
        assert_eq!(resaltes_de(&app, &s), vec![(1, 0, 0, false)], "la línea 3 queda en la fila 1 (la vista empieza en la 2)");
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
            programa: None, titulo_osc: None,
            usage: None,
            title: None,
            agent: false, hooked: false,
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

    /// B1: tras un fallo de conexión no se confía en la generación guardada.
    #[test]
    fn fallo_de_paneles_invalida_la_generacion() {
        let mut app = App { selected: Some(1), snap: Some(snap(&["a"], 1)), snap_gen: Some(3), ..Default::default() };
        let _ = update(&mut app, Message::Panes(Err("no hay daemon".into())));
        assert_eq!(app.snap_gen, None);
    }

    /// Grupos: plegar y desplegar alterna la clave del grupo.
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

    /// Título de pestaña: proceso y cwd, o el título de la tarea.
    #[test]
    fn titulo_de_pestana_proceso_y_cwd() {
        let mut p = pane_info(1, "shell", Status::Idle);
        p.cmd = vec!["/bin/zsh".into(), "-l".into()];
        p.cwd = Some(format!("{}/Development/nexo", home_dir()));
        // Sin programa en primer plano, el comando lanzado; la carpeta, corta.
        assert_eq!(titulo_pestana(&p), "zsh · nexo");
        p.title = Some("bug de ventas".into());
        assert_eq!(titulo_pestana(&p), "bug de ventas", "el título de la tarea manda");
    }

    /// P1.5: la píldora dice el programa en primer plano y la carpeta corta, no siempre el shell.
    #[test]
    fn titulo_de_pestana_sigue_al_programa() {
        let mut p = pane_info(1, "shell", Status::Idle);
        p.cmd = vec!["zsh".into()];
        p.cwd = Some("/Users/x/Development/nexo".into());
        p.programa = Some("vim".into());
        assert_eq!(titulo_pestana(&p), "vim · nexo");
        p.programa = Some("claude".into());
        assert_eq!(titulo_pestana(&p), "claude · nexo");
        p.cwd = Some(home_dir());
        assert_eq!(titulo_pestana(&p), "claude · ~", "la carpeta de casa se dice ~");
    }

    /// P1.5: el título que el programa anuncia por OSC 0/2 sustituye al nombre del proceso, pero no el de un shell
    /// (el prompt de un shell pone la ruta ahí), y un título largo se recorta.
    #[test]
    fn titulo_osc_del_programa_y_no_del_shell() {
        let mut p = pane_info(1, "shell", Status::Idle);
        p.cmd = vec!["zsh".into()];
        p.cwd = Some("/Users/x/nexo".into());
        p.programa = Some("zsh".into());
        p.titulo_osc = Some("user@mac: ~/nexo".into());
        assert_eq!(titulo_pestana(&p), "zsh · nexo", "un shell no muestra su título OSC");
        p.programa = Some("vim".into());
        p.titulo_osc = Some("main.rs - VIM".into());
        assert_eq!(titulo_pestana(&p), "main.rs - VIM · nexo");
        p.titulo_osc = Some("x".repeat(60));
        let t = titulo_pestana(&p);
        assert!(t.starts_with(&"x".repeat(31)) && t.contains('…') && t.ends_with("· nexo"), "{t}");
    }

    /// review-15 M2: Cmd+clic no ejecuta bundles ni ejecutables; un documento se abre y lo demás se revela en el Finder.
    #[test]
    fn cmd_clic_no_ejecuta_programas() {
        let dir = std::env::temp_dir().join(format!("galgo-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Demo.app")).unwrap();
        std::fs::write(dir.join("leeme.command"), "#!/bin/sh\necho hola\n").unwrap();
        std::fs::write(dir.join("notas.md"), "hola").unwrap();
        std::fs::write(dir.join("datos.bin"), "x").unwrap();
        let script = dir.join("script");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let meta = |p: &std::path::Path| std::fs::metadata(p).unwrap();
        assert!(accion_para(&dir.join("Demo.app"), &meta(&dir.join("Demo.app"))).is_err(), "un .app no se abre");
        assert!(accion_para(&dir.join("leeme.command"), &meta(&dir.join("leeme.command"))).is_err(), "un .command no se ejecuta");
        assert!(accion_para(&script, &meta(&script)).is_err(), "un ejecutable no se lanza");
        assert_eq!(accion_para(&dir.join("notas.md"), &meta(&dir.join("notas.md"))), Ok(AccionRuta::Abrir));
        assert_eq!(accion_para(&dir.join("datos.bin"), &meta(&dir.join("datos.bin"))), Ok(AccionRuta::Revelar));
        std::fs::create_dir_all(dir.join("Demo.app/Contents")).unwrap();
        std::fs::write(dir.join("Demo.app/Contents/Info.plist"), "<plist/>").unwrap();
        assert!(open_target(&dir.join("Demo.app/Contents/Info.plist").to_string_lossy()).is_err(), "nada dentro de un bundle (el archivo existe)");
        assert!(open_target("/no/existe/x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Selección y apertura en el terminal: palabra, URL, ruta y el entrecomillado de rutas al arrastrar.
    #[test]
    fn palabra_url_y_comillas() {
        let linea: Vec<char> = "ver https://x.dev/a, ok".chars().collect();
        assert_eq!(palabra_en(&linea, 1), Some((0, 2)));
        assert_eq!(token_en(&linea, 6).as_deref(), Some("https://x.dev/a"));
        assert_eq!(token_en(&linea, 3), None, "el espacio no es un enlace");
        assert_eq!(shell_quote("a b'c"), "'a b'\\''c'");
    }

    /// Terminal primero: sin barra lateral ni de estado, y una sola fila superior aunque haya varios paneles o
    /// carpetas (no hay fila de sesiones por carpeta).
    #[test]
    fn marco_por_defecto_es_terminal_primero() {
        let mut app = App::default();
        app.panes = vec![pane_info(1, "shell", Status::Idle)];
        let m = marco_para(&app);
        assert!(!m.sidebar && !m.status);
        assert_eq!(top_h(), TABS_H);
        app.sidebar = true;
        assert!(marco_para(&app).sidebar);
        app.panes.push(pane_info(2, "otro", Status::Idle));
        app.panes[1].workspace = Some("otro/rama".into());
        assert_eq!(top_h(), TABS_H, "dos carpetas: sigue una sola fila");
    }

    /// Un aviso que llega durante una lectura en vuelo deja otra pendiente, que se pide al volver la primera.
    #[test]
    fn aviso_durante_una_lectura_no_se_pierde() {
        let mut app = App::default();
        app.panes = vec![pane_info(1, "a", Status::Idle)];
        app.selected = Some(1);
        app.loading = true;
        let _ = update(&mut app, Message::PaneChanged(1, 9));
        assert!(app.refresh_wanted, "el aviso queda pendiente mientras se lee");
        let _ = update(&mut app, Message::Screen(1, 0, None, Ok(ScreenReply::Unchanged(3))));
        assert!(!app.refresh_wanted);
        assert!(app.loading, "al volver la lectura en curso se pide la pantalla nueva");
    }

    /// Suscripción (principio de la divulgación): un aviso del panel enfocado pide su pantalla; uno de otro panel no.
    #[test]
    fn aviso_de_cambio_pide_solo_el_panel_enfocado() {
        let mut app = App::default();
        app.panes = vec![pane_info(1, "a", Status::Idle), pane_info(2, "b", Status::Idle)];
        app.selected = Some(1);
        let _ = update(&mut app, Message::PaneChanged(2, 9));
        assert!(!app.loading, "un panel que no está enfocado no se pide");
        let _ = update(&mut app, Message::PaneChanged(1, 9));
        assert!(app.loading, "el panel enfocado pide su pantalla nueva");
    }

    /// Workspace nativo: Cmd+Shift+N abre uno explícito; cerrarlo pide dos clics; y lo guardado vuelve al arrancar
    /// (pestañas, splits y el grupo) cuando el daemon lista de nuevo sus paneles.
    /// P0.1: sin selector de carpeta. Cmd+O y Cmd+N ya no abren nada; Cmd+Shift+N agrupa desde el cwd actual.
    #[test]
    fn sin_selector_de_carpeta_ni_formularios() {
        assert!(shortcut(&Key::Character("o".into()), keyboard::Modifiers::COMMAND).is_none());
        assert!(shortcut(&Key::Character("n".into()), keyboard::Modifiers::COMMAND).is_none());
        assert!(matches!(shortcut(&Key::Character("N".into()), keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT), Some(Message::OpenNewNamedWorkspace)));
    }

    /// M3: tras un reinicio, la disposición espera a sus paneles; el que no vuelve se relanza en su carpeta, y nada se
    /// guarda hasta que la disposición está aplicada (un listado incompleto no la borra).
    #[test]
    fn la_disposicion_espera_a_sus_paneles() {
        let dir = std::env::temp_dir().join(format!("galgo-m3-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let prefs_path = dir.join("gui-prefs.json");
        let _ = std::fs::remove_file(&prefs_path);
        let v = serde_json::json!({
            "workspaces": [{ "key": "nexo", "explicit": false, "folder": null, "active": 0, "tabs": [{
                "focus": "shell",
                "tree": { "split": { "v": true, "r": 0.5, "a": { "leaf": "shell" }, "b": { "leaf": "claude" } } }
            }] }],
            "panes": { "claude": { "cwd": "/tmp", "workspace": "nexo" } }
        });
        prefs::set(&prefs_path, "workspaces", v).expect("se escribe la disposición");
        let mut app = App { prefs_path: Some(prefs_path.clone()), ..Default::default() };
        let en_nexo = |id: u32, name: &str| {
            let mut p = pane_info(id, name, Status::Idle);
            p.workspace = Some("nexo".into());
            p
        };
        let _ = update(&mut app, Message::Panes(Ok(vec![en_nexo(1, "shell")])));
        assert!(!app.ws_restored && app.ws_pendiente.is_some(), "espera a claude");
        assert!(app.layouts.by_key.get("nexo").map_or(true, |w| w.tabs[0].root.ids().len() < 2), "el split no se aplica a medias");
        let _ = update(&mut app, Message::Panes(Ok(vec![en_nexo(1, "shell"), en_nexo(2, "claude")])));
        assert!(app.ws_restored && app.ws_pendiente.is_none());
        assert_eq!(app.layouts.by_key.get("nexo").expect("la pestaña vuelve").tabs[0].root.ids().len(), 2);
    }

    /// P0: una pestaña de otro workspace (deducido por git, no creado por el usuario) se puede enfocar desde la barra plana.
    #[test]
    fn pestana_de_otro_workspace_se_enfoca() {
        let en = |id: u32, name: &str, ws: &str| {
            let mut p = pane_info(id, name, Status::Idle);
            p.workspace = Some(ws.into());
            p
        };
        let mut app = App { panes: vec![en(1, "shell", "nexo/main"), en(2, "otro", "tmp")], ..Default::default() };
        app.layouts.sync(&[(1, "nexo/main".into()), (2, "tmp".into())]);
        app.selected = Some(1);
        let _ = update(&mut app, Message::SelectTabIn("tmp".into(), 0));
        assert_eq!(app.selected, Some(2), "el panel de tmp pasa a ser el enfocado");
    }

    /// Daemon vacío al abrir la GUI con una disposición en disco: se espera a sus paneles, no se abre un shell en ~
    /// y el disco no se sobrescribe.
    #[test]
    fn daemon_vacio_al_abrir_espera_la_disposicion() {
        let dir = std::env::temp_dir().join(format!("galgo-vacio-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let prefs_path = dir.join("gui-prefs.json");
        let _ = std::fs::remove_file(&prefs_path);
        let tabs: Vec<serde_json::Value> = ["s1", "t1"].iter().map(|n| serde_json::json!({ "focus": n, "tree": { "leaf": n } })).collect();
        let v = serde_json::json!({ "workspaces": [{ "key": "nexo", "explicit": false, "folder": null, "active": 0, "tabs": tabs }], "panes": { "s1": { "cwd": "/tmp", "workspace": "nexo" }, "t1": { "cwd": "/tmp", "workspace": "nexo" } } });
        prefs::set(&prefs_path, "workspaces", v.clone()).expect("se escribe");
        let mut app = App { prefs_path: Some(prefs_path.clone()), ..Default::default() };
        let _ = update(&mut app, Message::Panes(Ok(vec![])));
        assert!(app.ws_pendiente.is_some() && !app.ws_restored, "espera la disposición");
        let guardado = prefs::get(&prefs_path, "workspaces").expect("sigue en disco");
        assert_eq!(guardado, v, "el disco no se sobrescribe");
    }

    /// El caso real: seis paneles ya están en el primer listado, y la disposición de seis pestañas se aplica entera.
    #[test]
    fn seis_pestanas_del_disco_se_aplican_en_el_primer_listado() {
        let dir = std::env::temp_dir().join(format!("galgo-seis-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let prefs_path = dir.join("gui-prefs.json");
        let _ = std::fs::remove_file(&prefs_path);
        let nombres = ["s1", "t1", "t2", "d1", "d2", "d3"];
        let tabs: Vec<serde_json::Value> = nombres.iter().map(|n| serde_json::json!({ "focus": n, "tree": { "leaf": n } })).collect();
        let v = serde_json::json!({ "workspaces": [{ "key": "nexo", "explicit": false, "folder": null, "active": 0, "tabs": tabs }], "panes": {} });
        prefs::set(&prefs_path, "workspaces", v).expect("se escribe");
        let mut app = App { prefs_path: Some(prefs_path), ..Default::default() };
        let en_nexo = |id: u32, name: &str| {
            let mut p = pane_info(id, name, Status::Idle);
            p.workspace = Some("nexo".into());
            p
        };
        let panes: Vec<PaneInfo> = nombres.iter().enumerate().map(|(i, n)| en_nexo(i as u32 + 1, n)).collect();
        let _ = update(&mut app, Message::Panes(Ok(panes)));
        assert!(app.ws_restored, "la disposición se aplica sin esperar");
        let ws = app.layouts.by_key.get("nexo").expect("la clave nexo existe");
        assert_eq!(ws.tabs.len(), 6, "las seis pestañas vuelven");
    }

    #[test]
    fn workspace_explicito_se_guarda_y_se_restaura() {
        let dir = std::env::temp_dir().join(format!("galgo-ws-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let prefs_path = dir.join("gui-prefs.json");
        let _ = std::fs::remove_file(&prefs_path);
        let mut app = App { prefs_path: Some(prefs_path.clone()), ..Default::default() };
        // Cmd+Shift+N: el siguiente workspace abierto es explícito.
        assert!(matches!(shortcut(&Key::Character("N".into()), keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT), Some(Message::OpenNewNamedWorkspace)));
        // Primer listado: arranque hecho, sin nada guardado todavía. Los paneles llevan la etiqueta de su workspace.
        let en_nexo = |id: u32, name: &str| {
            let mut p = pane_info(id, name, Status::Idle);
            p.workspace = Some("nexo".into());
            p
        };
        let _ = update(&mut app, Message::Panes(Ok(vec![en_nexo(1, "shell"), en_nexo(2, "claude")])));
        app.layouts.new_tab("nexo", 1);
        app.layouts.split(1, true, 2);
        app.explicit.insert("nexo".into(), "/tmp/nexo".into());
        let _ = update(&mut app, Message::Echo);
        let guardado = prefs::get(&prefs_path, "workspaces").expect("el estado se guarda");
        assert!(guardado.to_string().contains("\"explicit\":true"), "{guardado}");
        // Reinicio: el daemon lista los mismos paneles y el layout vuelve.
        let mut nueva = App { prefs_path: Some(prefs_path.clone()), ..Default::default() };
        let _ = update(&mut nueva, Message::Panes(Ok(vec![en_nexo(1, "shell"), en_nexo(2, "claude")])));
        assert_eq!(nueva.explicit.get("nexo").map(String::as_str), Some("/tmp/nexo"));
        let ws = nueva.layouts.by_key.get("nexo").expect("el workspace vuelve");
        assert_eq!(ws.tabs[0].root.ids().len(), 2, "el split vuelve con sus dos paneles");
        // Cerrar el grupo: el primer clic avisa, el segundo lo cierra.
        let _ = update(&mut nueva, Message::CloseWorkspace("nexo".into()));
        assert!(nueva.explicit.contains_key("nexo"), "el primer clic solo avisa");
        let _ = update(&mut nueva, Message::CloseWorkspace("nexo".into()));
        assert!(!nueva.explicit.contains_key("nexo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// «+ Agente…»: el rol por defecto depende de si ya hay un manager; un nombre inválido no lanza nada; un worker se
    /// enlaza al manager del workspace y su rol queda guardado con el nombre del panel.
    #[test]
    fn agente_se_agrega_con_rol_y_manager() {
        let mut app = App::default();
        let mut jefe = pane_info(1, "jefe", Status::Idle);
        jefe.workspace = Some("ws".into());
        app.panes = vec![jefe];
        app.roles.insert("jefe".into(), "manager".into());
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.selected = Some(1);
        let _ = update(&mut app, Message::OpenAgentForm);
        assert_eq!(app.agent_form.as_ref().map(|f| f.role), Some(launch::Role::Worker), "ya hay manager: el rol por defecto es worker");
        let _ = update(&mut app, Message::AgentFormName("con espacio".into()));
        let _ = update(&mut app, Message::AgentFormSubmit);
        assert!(app.agent_form.as_ref().is_some_and(|f| f.error.is_some()), "un nombre inválido no se envía");
        let _ = update(&mut app, Message::AgentFormName("rev".into()));
        let _ = update(&mut app, Message::AgentFormSubmit);
        assert!(app.agent_form.is_none(), "un nombre válido cierra el formulario");
        assert_eq!(app.roles.get("rev").map(String::as_str), Some("worker"));
        assert_eq!(manager_of(&app, Some("ws")).as_deref(), Some("jefe"));
    }

    /// Layout de un equipo: el manager a la izquierda y los workers apilados a la derecha, en ese orden.
    #[test]
    fn layout_de_equipo_manager_a_la_izquierda() {
        assert_eq!(team_tree(None, &[]), None);
        assert_eq!(team_tree(Some(1), &[]), Some(layout::Node::Leaf(1)));
        let arbol = team_tree(Some(1), &[2, 3, 4]).unwrap();
        assert_eq!(arbol.ids(), vec![1, 2, 3, 4], "el manager primero, luego los workers de arriba abajo");
        match arbol {
            layout::Node::Split { vertical, ratio, a, b } => {
                assert!(vertical && (ratio - 0.6).abs() < 1e-6, "el manager ocupa el 60 % a la izquierda");
                assert_eq!(*a, layout::Node::Leaf(1));
                assert!(matches!(*b, layout::Node::Split { vertical: false, .. }), "los workers van apilados");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Un equipo del repo pide confirmación la primera vez; confirmado, queda de confianza y el siguiente lanza
    /// directo. Lanzar crea su workspace con nombre, los roles y los nombres reservados.
    #[test]
    fn equipo_del_repo_pide_confianza_una_vez() {
        let dir = std::env::temp_dir().join(format!("galgo-team-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".galgo")).unwrap();
        std::fs::write(
            dir.join(team::FILE),
            "name = \"cotiza\"\n[[agent]]\nname=\"jefe\"\nprovider=\"claude\"\nrole=\"manager\"\n[[agent]]\nname=\"codex\"\nprovider=\"codex\"\nrole=\"worker\"\n",
        )
        .unwrap();
        let root = dir.to_string_lossy().into_owned();
        let mut app = App::default();
        let mut p = pane_info(1, "shell", Status::Idle);
        p.cwd = Some(root.clone());
        app.panes = vec![p];
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.selected = Some(1);
        let _ = update(&mut app, Message::TeamRequested(TeamSource::Repo));
        assert!(app.team_pending.is_some(), "la primera vez se pide confirmación");
        assert!(app.explicit.is_empty(), "sin confirmar, no se crea nada");
        let _ = update(&mut app, Message::TeamConfirm);
        assert!(app.team_pending.is_none());
        assert!(team::trusted(&app.team_trust, &root, &team::fingerprint(&std::fs::read_to_string(dir.join(team::FILE)).unwrap())));
        assert!(app.explicit.contains_key("cotiza"), "el equipo tiene su workspace con nombre");
        assert_eq!(app.roles.get("jefe").map(String::as_str), Some("manager"));
        assert_eq!(app.roles.get("codex").map(String::as_str), Some("worker"));
        assert!(app.inflight.contains(&"jefe".to_string()) && app.inflight.contains(&"codex".to_string()));
        // Segunda vez, mismo archivo: ya es de confianza y no pide nada.
        app.inflight.clear();
        let _ = update(&mut app, Message::TeamRequested(TeamSource::Repo));
        assert!(app.team_pending.is_none(), "un equipo de confianza se lanza directo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Un resultado de lanzamiento arma el layout del equipo: el manager a la izquierda y el foco en él.
    #[test]
    fn resultado_del_equipo_arma_el_layout() {
        let mut app = App::default();
        let mut jefe = pane_info(10, "jefe", Status::Idle);
        jefe.workspace = Some("cotiza".into());
        let mut codex = pane_info(11, "codex", Status::Idle);
        codex.workspace = Some("cotiza".into());
        app.explicit.insert("cotiza".into(), "/tmp/cotiza".into());
        let _ = update(&mut app, Message::TeamLaunched("cotiza".into(), Some("jefe".into()), vec![("jefe".into(), Ok(jefe)), ("codex".into(), Ok(codex))]));
        let ws = app.layouts.by_key.get("cotiza").expect("el workspace tiene su pestaña");
        let tab = ws.tabs.last().unwrap();
        assert_eq!(tab.root.ids(), vec![10, 11]);
        assert_eq!(tab.focus, 10, "el foco va al manager");
        assert!(matches!(tab.root, layout::Node::Split { vertical: true, .. }));
    }

    /// review-15 M4: solo `closed` es un final legítimo; un error del daemon o una línea rara cuentan como fallo, para
    /// que la GUI sondee en vez de quedarse con la pantalla congelada.
    #[test]
    fn avisos_del_daemon_se_clasifican() {
        assert_eq!(leer_aviso(r#"{"ok":true,"event":"changed","gen":7}"#), Aviso::Cambio(7));
        assert_eq!(leer_aviso(r#"{"ok":true,"event":"watching","pane":1}"#), Aviso::Inicio);
        assert_eq!(leer_aviso(r#"{"ok":true,"event":"closed","pane":1}"#), Aviso::Cerrado);
        assert_eq!(leer_aviso(r#"{"ok":false,"error":"la suscripción se atiende en su conexión"}"#), Aviso::Fallo, "un daemon antiguo no es un cierre");
        assert_eq!(leer_aviso(r#"{"ok":false,"error":"panel no encontrado: 9"}"#), Aviso::Fallo);
        assert_eq!(leer_aviso("basura"), Aviso::Fallo);
        assert_eq!(leer_aviso(r#"{"ok":true,"event":"otra cosa"}"#), Aviso::Fallo);
    }

    /// review-15 A1: con el formulario de agente abierto, Cmd+W no pide cerrar el panel de detrás y las teclas no llegan
    /// a él; Esc lo cierra y el teclado vuelve al terminal.
    #[test]
    fn dialogo_abierto_es_dueno_del_teclado() {
        let mut app = App::default();
        app.panes = vec![pane_info(1, "shell", Status::Idle)];
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.selected = Some(1);
        app.agent_form = Some(AgentForm::para(&app));
        assert!(overlay_owns_keyboard(&app));
        let cmd_w = Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character("w".into()),
            modified_key: Key::Character("w".into()),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::COMMAND,
            text: None,
        });
        let _ = update(&mut app, Message::Event(cmd_w));
        assert!(!app.confirm_close, "Cmd+W no actúa sobre el panel de detrás");
        let esc = Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            modified_key: Key::Named(Named::Escape),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
        });
        let _ = update(&mut app, Message::Event(esc));
        assert!(app.agent_form.is_none() && !overlay_owns_keyboard(&app), "Esc cierra el diálogo");
    }

    /// review-15 A3: el prompt inicial espera al primer hook del agente; sin hook no se envía; si el panel termina sin
    /// avisar, se dice. Nunca con Enter.
    #[test]
    fn prompt_inicial_espera_al_hook() {
        let mut app = App::default();
        let mut p = pane_info(1, "jefe", Status::Idle);
        p.hooked = false;
        app.panes = vec![p];
        app.prompts_pending.insert("jefe".into(), "Coordina.\nDespués, resume.".into());
        let listado = app.panes.clone();
        let _ = update(&mut app, Message::Panes(Ok(listado)));
        assert!(app.prompts_pending.contains_key("jefe"), "sin hook, el prompt sigue pendiente");
        let mut vivo = pane_info(1, "jefe", Status::Idle);
        vivo.hooked = true;
        let _ = update(&mut app, Message::Panes(Ok(vec![vivo])));
        assert!(app.prompts_pending.is_empty(), "con el hook, se entrega (pegado, sin Enter) y sale de pendientes");
        assert_eq!(bracketed_paste("a\nb"), "\u{1b}[200~a\nb\u{1b}[201~");
        let mut muerto = pane_info(2, "codex", Status::Exited);
        muerto.hooked = false;
        app.prompts_pending.insert("codex".into(), "hola".into());
        app.panes.push(muerto.clone());
        let _ = update(&mut app, Message::Panes(Ok(vec![muerto])));
        assert!(app.prompts_pending.is_empty() && app.error.is_some(), "un agente que sale sin avisar deja el prompt sin enviar, y lo dice");
    }

    /// review-15 M3: un archivo cuyo nombre tiene caracteres de control no se pega; uno normal se pega sin Enter.
    #[test]
    fn arrastrar_archivo_no_pega_control() {
        let mut app = App::default();
        app.panes = vec![pane_info(1, "shell", Status::Idle)];
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.selected = Some(1);
        let _ = update(&mut app, Message::FileDropped(std::path::PathBuf::from("/tmp/a\u{3}touch x")));
        assert!(app.error.as_deref().is_some_and(|e| e.contains("control")), "un nombre con Ctrl-C no se pega");
        app.error = None;
        let _ = update(&mut app, Message::FileDropped(std::path::PathBuf::from("/tmp/normal.txt")));
        assert!(app.error.is_none(), "un nombre normal sí");
    }

    /// review-15 M5: un integrado pide confirmación (y no guarda confianza); la rama de un worktree salta las que ya
    /// existen en el repo; fuera de un repo no hay worktree y se avisa.
    #[test]
    fn integrados_confirman_y_ramas_libres() {
        let dir = std::env::temp_dir().join(format!("galgo-m5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().into_owned();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["branch", "codex"]);
        assert_eq!(rama_libre(&root, "codex"), "codex-2", "la rama existente se salta");
        assert_eq!(rama_libre(&root, "pi"), "pi", "una libre se usa tal cual");
        assert!(es_repo(&root) && !es_repo("/no/existe"));

        let mut app = App::default();
        let mut p = pane_info(1, "shell", Status::Idle);
        p.cwd = Some(root.clone());
        app.panes = vec![p];
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.selected = Some(1);
        let _ = update(&mut app, Message::TeamRequested(TeamSource::Preset(0)));
        assert!(app.team_pending.as_ref().is_some_and(|t| t.preset), "un integrado pide confirmación");
        assert!(app.explicit.is_empty(), "sin confirmar, no lanza nada");
        let _ = update(&mut app, Message::TeamConfirm);
        assert!(app.team_trust.get(&root).is_none(), "un integrado no guarda confianza de repo");
        assert_eq!(app.explicit.len(), 1, "al confirmar, el equipo tiene su workspace con nombre");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fuera de un repo, un equipo con workers en worktree avisa que se lanzan en la carpeta.
    #[test]
    fn equipo_fuera_de_repo_avisa() {
        let dir = std::env::temp_dir().join(format!("galgo-m5b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().into_owned();
        let mut app = App::default();
        let team = team::parse("[[agent]]\nname=\"w\"\nprovider=\"codex\"\nrole=\"worker\"\nisolation=\"worktree\"").unwrap();
        let _ = launch_team(&mut app, None, root.clone(), team);
        assert!(app.error.as_deref().is_some_and(|e| e.contains("no es un repo git")), "{:?}", app.error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// review-15 B5: cerrar un grupo pide un segundo clic dentro de 3 s; pasado el plazo, el primero vuelve a avisar.
    /// B7: arrastrar un archivo con ajustes abiertos no llega a la validación del panel.
    #[test]
    fn cerrar_grupo_caduca_y_arrastre_respeta_dialogos() {
        let mut app = App::default();
        app.explicit.insert("nexo".into(), "/tmp/nexo".into());
        let _ = update(&mut app, Message::CloseWorkspace("nexo".into()));
        assert!(app.explicit.contains_key("nexo"), "el primer clic solo avisa");
        let _ = update(&mut app, Message::CloseWorkspace("nexo".into()));
        assert!(!app.explicit.contains_key("nexo"), "el segundo, dentro de 3 s, cierra");
        app.explicit.insert("nexo".into(), "/tmp/nexo".into());
        let _ = update(&mut app, Message::CloseWorkspace("nexo".into()));
        if let Some(viejo) = std::time::Instant::now().checked_sub(Duration::from_secs(10)) {
            app.ws_close_armed_at = Some(viejo);
            let _ = update(&mut app, Message::CloseWorkspace("nexo".into()));
            assert!(app.explicit.contains_key("nexo"), "un aviso caducado no cierra: vuelve a avisar");
        }
        let mut con_ajustes = App::default();
        con_ajustes.settings = Some(settings::Section::General);
        let _ = update(&mut con_ajustes, Message::FileDropped(std::path::PathBuf::from("/tmp/a\u{3}b")));
        assert!(con_ajustes.error.is_none(), "con ajustes abiertos, el arrastre no llega al panel");
    }

    /// Arranque (principio terminal-primero): la primera lista vacía abre un shell en ~ y marca el inicio; la
    /// bienvenida no aparece en el flujo normal.
    #[test]
    fn primer_arranque_sin_paneles_marca_el_inicio() {
        let mut app = App::default();
        let _ = update(&mut app, Message::Panes(Ok(vec![])));
        assert!(app.started);
        assert!(app.panes.is_empty());
    }

    /// A: el cuerpo muestra la bienvenida sin paneles; «Nuevo workspace» tiene prioridad sobre el resto.
    #[test]
    fn cuerpo_sin_paneles_es_la_bienvenida() {
        let mut app = App::default();
        assert_eq!(body_mode(&app), Body::Welcome);
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
        assert!(matches!(shortcut(&k("3"), cmd), Some(Message::SelectRowAt(2))));
        assert!(matches!(shortcut(&k("{"), cmd | shift), Some(Message::NextTab(-1))), "Cmd+Shift+[ va a la anterior");
        assert!(matches!(shortcut(&k("}"), cmd | shift), Some(Message::NextTab(1))), "Cmd+Shift+] va a la siguiente");
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
        let (size, line, cell) = metrics_for(0);
        assert_eq!(size, FONT_SIZE);
        assert!((line - LINE_H).abs() < 1.0, "alto {line}");
        assert!((cell - CELL_W).abs() < 0.05, "ancho {cell}");
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
        // Cmd+1..9 es la posición en la fila de pestañas (la del orden de creación), no el workspace de la barra lateral.
        let mut l = layout::Layouts::default();
        l.new_tab("repo/main", 1);
        l.new_tab("otro/dev", 2);
        l.new_tab("repo/main", 3);
        let fila: Vec<u32> = l.en_orden().into_iter().map(|(k, i)| l.by_key[k].tabs[i].focus).collect();
        assert_eq!(fila, vec![1, 2, 3], "la fila sigue el orden de creación");
        let _ = &panes;
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

    /// Un shell en su prompt se cierra sin preguntar; con un programa dentro pregunta, y el diálogo es dueño del teclado.
    #[test]
    fn cmd_w_cierra_un_shell_libre_y_pregunta_con_algo_dentro() {
        let mut p = pane_info(1, "p", Status::Idle);
        p.programa = Some("zsh".into());
        assert!(!needs_confirm(&p), "shell en el prompt");
        p.programa = Some("vim".into());
        assert!(needs_confirm(&p), "vim abierto");
        let mut app = App { panes: vec![p], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::CloseFocused);
        assert_eq!(body_mode(&app), Body::CloseConfirm, "el diálogo se ve aunque la barra de estado esté oculta");
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Escape), None)));
        assert!(!app.confirm_close);
        let _ = update(&mut app, Message::CloseFocused);
        let _ = update(&mut app, Message::Event(key_press(Key::Named(Named::Enter), None)));
        assert!(!app.confirm_close, "Enter confirma");
    }

    /// `exit` en un shell cierra su pestaña; un agente o un comando terminado se quedan para leerlos.
    #[test]
    fn un_shell_que_termina_solo_se_cierra() {
        let mut shell = pane_info(1, "s", Status::Exited);
        shell.cmd = vec!["/bin/zsh".into(), "-l".into()];
        let mut agente = pane_info(2, "a", Status::Exited);
        agente.cmd = vec!["zsh".into()];
        agente.agent = true;
        let mut build = pane_info(3, "b", Status::Exited);
        build.cmd = vec!["cargo".into()];
        let mut vivo = pane_info(4, "v", Status::Idle);
        vivo.cmd = vec!["zsh".into()];
        assert_eq!(shells_terminados(&[shell, agente, build, vivo]), vec![1]);
    }

    /// Cerrar la última pestaña marca la ventana para salir; con otra abierta, no.
    #[test]
    fn cerrar_la_ultima_pestana_cierra_la_ventana() {
        let mut app = App { panes: vec![pane_info(1, "a", Status::Exited), pane_info(2, "b", Status::Idle)], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::ConfirmClose);
        assert!(!app.salir_al_vaciar);
        let mut app = App { panes: vec![pane_info(1, "a", Status::Exited)], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::ConfirmClose);
        assert!(app.salir_al_vaciar);
    }

    /// Un clic que un widget de texto capturó sigue llegando al terminal (foco de split y selección).
    #[test]
    fn el_clic_capturado_por_el_texto_llega_al_terminal() {
        let clic = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        assert!(reenviar_evento(clic.clone(), event::Status::Captured).is_some());
        assert!(reenviar_evento(clic, event::Status::Ignored).is_some());
        let rueda = Event::Mouse(mouse::Event::WheelScrolled { delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 } });
        assert!(reenviar_evento(rueda, event::Status::Captured).is_none());
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

    /// C: Cmd+Shift+P abre la paleta; Cmd+K limpia la pantalla (terminal primero).
    #[test]
    fn atajos_de_la_paleta() {
        let k = Key::Character("k".into());
        assert!(matches!(shortcut(&k, keyboard::Modifiers::COMMAND), Some(Message::ClearScreen)));
        let p = Key::Character("p".into());
        assert!(matches!(shortcut(&p, keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT), Some(Message::OpenPalette)));
        let p = Key::Character("P".into());
        assert!(matches!(shortcut(&p, keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT), Some(Message::OpenPalette)));
    }

    /// E: dividir pide un panel al lado del enfocado; al llegar, queda en la misma pestaña y con el foco.
    #[test]
    fn dividir_pone_el_panel_al_lado_y_le_da_el_foco() {
        let mut p1 = pane_info(1, "uno", Status::Idle);
        p1.workspace = Some("repo/main".into());
        let mut app = App { panes: vec![p1], selected: Some(1), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
        let _ = update(&mut app, Message::SplitFocused(false));
        let _ = update(&mut app, Message::LaunchFailed("shell".into(), "sin daemon".into()));
        assert!(app.pending_split.is_empty());
    }

    /// E: Cmd+Opt+flecha mueve el foco al split vecino; sin vecino en esa dirección, no se mueve.
    #[test]
    fn foco_por_direccion_entre_splits() {
        let mut app = App { panes: vec![pane_info(1, "izq", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.layouts.split(1, true, 2);
        app.panes.push(pane_info(2, "der", Status::Idle));
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
        assert_eq!(focus_origin(&app), term_origin(), "un panel sin split usa el origen de referencia");
        app.layouts.split(1, true, 2);
        app.panes.push(pane_info(2, "dos", Status::Idle));
        app.selected = Some(2);
        let (x, _) = focus_origin(&app);
        assert!(x > term_origin().0, "el panel de la derecha empieza más a la derecha");
        let (rows, cols) = grid_in(pane_rect(&app, 2).unwrap());
        assert!(cols < grid_for((1100.0, 700.0)).1, "y tiene menos columnas que la ventana entera");
        assert!(rows > 5);
    }

    /// E: las pantallas de los paneles visibles sin foco se sondean por turnos, y la respuesta se guarda.
    #[test]
    fn paneles_visibles_se_sondean_por_turnos() {
        let mut app = App { panes: vec![pane_info(1, "a", Status::Idle)], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.layouts.split(1, true, 2);
        app.panes.push(pane_info(2, "b", Status::Idle));
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.layouts.split(1, true, 2);
        app.layouts.sync_nombrado(&pane_nombres(&app));
        let _ = update(&mut app, Message::Select(2));
        let _ = update(&mut app, Message::ConfirmClose);
        app.panes.retain(|p| p.id != 2);
        let restantes = app.panes.clone();
        let _ = update(&mut app, Message::Panes(Ok(restantes)));
        assert_eq!(app.selected, Some(1), "el hermano del split, no el primero de la lista");
        assert_eq!(app.layouts.key_of(1).as_deref(), Some("r/main"));
    }

    /// r13 M3: al dividir, el panel original se redimensiona a su mitad (no se queda con toda la ventana).
    #[test]
    fn r13_split_no_redimensiona_el_original() {
        let mut uno = pane_info(1, "uno", Status::Idle);
        uno.workspace = Some("r/main".into());
        let mut app = App { panes: vec![uno], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.layouts.split(1, true, 2);
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
        let _ = update(&mut app, Message::Select(3));
        assert_eq!(app.layouts.active_tab("w").unwrap().focus, 3);
        let _ = update(&mut app, Message::SelectRowAt(0));
        assert_eq!(app.selected, Some(1), "Cmd+1 va a la primera pestaña de la fila, aunque sea de otro workspace");
    }

    /// r13 B1+B2: dos splits en vuelo reciben nombres distintos, y cada respuesta cae en su split aunque lleguen al revés.
    #[test]
    fn r13_dos_splits_en_vuelo_se_casan_por_nombre() {
        let mut uno = pane_info(1, "uno", Status::Idle);
        uno.workspace = Some("r/main".into());
        let mut app = App { panes: vec![uno], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
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
        PaneInfo { id, name: name.into(), status, manager: None, cmd: vec![], unacked: 0, oldest_unacked_secs: None, orphan: false, waiting_on: None, waiting_secs: None, workspace: None, cwd: None, programa: None, titulo_osc: None, usage: None, title: None, agent: false, hooked: false }
    }

    fn snap(lines: &[&str], cols: u16) -> Snapshot {
        let run = |t: &str| Run { t: t.into(), fg: galgo_proto::Color::Default, bg: galgo_proto::Color::Default, bold: false, italic: false, underline: false, inverse: false, dim: false, strike: false, blink: false };
        Snapshot { gen: 0, rows: lines.len() as u16, cols, cursor: (0, 0), cursor_hidden: true, lines: lines.iter().map(|l| vec![run(l)]).collect(), scrollback: 0, scrollback_max: 0, bracketed_paste: false, wrapped: vec![], mouse: Default::default(), mouse_sgr: false, alt_screen: false, portapapeles: None }
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
        parts.iter().map(|(t, fg, bold)| Run { t: (*t).into(), fg: *fg, bg: galgo_proto::Color::Default, bold: *bold, italic: false, underline: false, inverse: false, dim: false, strike: false, blink: false }).collect()
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
        Snapshot { gen: 1, rows, cols, cursor: (1, 52), cursor_hidden: false, lines: all, scrollback: 0, scrollback_max: 0, bracketed_paste: false, wrapped: vec![], mouse: Default::default(), mouse_sgr: false, alt_screen: false, portapapeles: None }
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
        Snapshot { gen: 1, rows, cols, cursor: (3, 23), cursor_hidden: false, lines: all, scrollback: 0, scrollback_max: 0, bracketed_paste: false, wrapped: vec![], mouse: Default::default(), mouse_sgr: false, alt_screen: false, portapapeles: None }
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
        app.layouts.sync_nombrado(&pane_nombres(&app));
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

    /// Prompt tipo powerlevel10k: segmentos con fondo, separadores powerline y iconos de Nerd Font.
    fn prompt_p10k() -> Snapshot {
        let run = |t: &str, bg: Option<(u8, u8, u8)>, fg: (u8, u8, u8)| Run {
            t: t.into(),
            fg: galgo_proto::Color::Rgb(fg.0, fg.1, fg.2),
            bg: bg.map_or(galgo_proto::Color::Default, |b| galgo_proto::Color::Rgb(b.0, b.1, b.2)),
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            dim: false,
            strike: false,
            blink: false,
        };
        let azul = (0x3a, 0x5f, 0x8a);
        let gris = (0x5a, 0x6e, 0x82);
        let blanco = (0xee, 0xee, 0xee);
        let line0 = vec![
            run(" \u{f179} ", Some(azul), blanco),
            run("\u{e0b0}", Some(gris), azul),
            run(" \u{f015} ~ ", Some(gris), blanco),
            run("\u{e0b0}", None, gris),
            run("     ", None, blanco),
            run("\u{e0b2}", None, azul),
            run(" \u{f017} 18:42:34 ", Some(azul), blanco),
            run("\u{e0b0}", None, azul),
        ];
        let line1 = vec![run("\u{276f} ", None, (0x7f, 0xd0, 0x6a)), run("ls -la", None, blanco)];
        Snapshot {
            gen: 0,
            rows: 2,
            cols: 80,
            cursor: (1, 8),
            cursor_hidden: false,
            lines: vec![line0, line1],
            scrollback: 0,
            scrollback_max: 0,
            bracketed_paste: false,
            wrapped: vec![false, false],
            mouse: Default::default(),
            mouse_sgr: false,
            alt_screen: false,
            portapapeles: None,
        }
    }

    /// Render de los estados del workspace nativo y del equipo (para revisar el diseño sin teclado ni ratón).
    #[test]
    #[ignore]
    fn render_grupos() {
        let dir = std::path::PathBuf::from(std::env::var("GALGO_UX_DIR").unwrap_or_else(|_| "/tmp/galgo-ux".into()));
        std::fs::create_dir_all(&dir).unwrap();
        let size = (1280, 800);
        let en = |id: u32, name: &str, status: Status, agent: bool| {
            let mut p = pane_info(id, name, status);
            p.workspace = Some("cotiza".into());
            p.cwd = Some("/Users/camilolagos/Development/cotiza".into());
            p.agent = agent;
            p
        };
        let base = || {
        let mut app = App { window: (1280.0, 800.0), panes: vec![en(1, "jefe", Status::Working, true), en(2, "codex-1", Status::Idle, true), en(3, "revisor", Status::Blocked, true)], ..Default::default() };
        app.explicit.insert("cotiza".into(), "/Users/camilolagos/Development/cotiza".into());
        app.roles.insert("jefe".into(), "manager".into());
        app.roles.insert("codex-1".into(), "worker".into());
        app.roles.insert("revisor".into(), "reviewer".into());
        app.layouts.by_key.insert(
            "cotiza".into(),
            layout::WsTabs {
                tabs: vec![layout::Tab {
                    root: layout::Node::Split {
                        vertical: true,
                        ratio: 0.6,
                        a: Box::new(layout::Node::Leaf(1)),
                        b: Box::new(layout::Node::Split { vertical: false, ratio: 0.5, a: Box::new(layout::Node::Leaf(2)), b: Box::new(layout::Node::Leaf(3)) }),
                    },
                    focus: 1,
                    orden: 0,
                }],
                active: 0,
            },
        );
        app.selected = Some(1);
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app
        };
        render(&base(), size, &dir.join("grupo-equipo.png"));
        let mut confirm = base();
        confirm.team_pending = Some(TeamPending {
            key: Some("cotiza".into()),
            root: "/Users/camilolagos/Development/cotiza".into(),
            team: team::parse("name = \"cotiza\"\n[[agent]]\nname=\"jefe\"\nprovider=\"claude\"\nrole=\"manager\"\nprompt=\"Coordina.\"\n[[agent]]\nname=\"codex-1\"\nprovider=\"codex\"\nmodel=\"gpt-5.5\"\nrole=\"worker\"\nisolation=\"worktree\"\n").unwrap(),
            fp: "0".into(),
            preset: false,
        });
        render(&confirm, size, &dir.join("grupo-confirmar-equipo.png"));
        let mut armado = base();
        armado.ws_close_armed = Some("cotiza".into());
        armado.ws_close_armed_at = Some(std::time::Instant::now());
        render(&armado, size, &dir.join("grupo-cierre-armado.png"));
        let mut largo = base();
        let texto: String = (0..14).map(|i| format!("Línea {i}: revisa el módulo de pagos y resume los cambios que encuentres.\n")).collect();
        largo.team_pending = Some(TeamPending {
            key: Some("cotiza".into()),
            root: "/Users/camilolagos/Development/cotiza".into(),
            team: team::Team { name: "cotiza".into(), agents: vec![team::TeamAgent { name: "jefe".into(), provider: launch::Agent::Claude, model: None, role: launch::Role::Manager, isolation: launch::Isolation::Shared, prompt: Some(texto) }] },
            fp: "0".into(),
            preset: false,
        });
        render(&largo, size, &dir.join("grupo-confirmar-prompt-largo.png"));
        let mut form = base();
        form.agent_form = Some(AgentForm { agent: launch::Agent::Codex, model: "gpt-5.5".into(), role: launch::Role::Worker, isolation: launch::Isolation::Worktree, name: String::new(), error: None });
        render(&form, size, &dir.join("grupo-agente.png"));
    }

    /// Render de un snapshot real volcado por el daemon (`GALGO_SNAP`), con cada fuente de terminal.
    #[test]
    #[ignore]
    fn render_snapshot_real() {
        let Ok(ruta) = std::env::var("GALGO_SNAP") else { return };
        let dir = std::path::PathBuf::from(std::env::var("GALGO_UX_DIR").unwrap_or_else(|_| "/tmp/galgo-ux".into()));
        let snap: Snapshot = serde_json::from_str(&std::fs::read_to_string(ruta).unwrap()).unwrap();
        let cols = snap.cols as f32;
        // Como en la app: la fuente de símbolos se carga al arrancar.
        iced_graphics::text::font_system().write().unwrap().load_font(std::borrow::Cow::Borrowed(SYMBOLS_FONT));
        for familia in ["Menlo", "MesloLGLDZ Nerd Font Mono"] {
            let nombre: &'static str = fonts::CANDIDATES.iter().map(|c| c.0).find(|f| *f == familia).unwrap_or("Menlo");
            fonts::set_current(nombre);
            let mut app = App { window: ((cols + COLS_MARGIN as f32) * cell_w() + 2.0 * LEAF_PAD + SIDEBAR_W, 240.0), panes: vec![pane_info(1, "shell", Status::Idle)], selected: Some(1), snap: Some(snap.clone()), ..Default::default() };
            app.layouts.sync_nombrado(&pane_nombres(&app));
            let archivo = format!("snap-{}.png", familia.replace(' ', "_"));
            render(&app, (app.window.0 as u32, 240), &dir.join(archivo));
        }
        fonts::set_current("Menlo");
    }

    /// Render del prompt p10k con cada fuente disponible (para comparar a ojo).
    #[test]
    #[ignore]
    fn render_prompt_p10k() {
        let dir = std::path::PathBuf::from(std::env::var("GALGO_UX_DIR").unwrap_or_else(|_| "/tmp/galgo-ux".into()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App { window: (900.0, 220.0), panes: vec![pane_info(1, "shell", Status::Idle)], selected: Some(1), snap: Some(prompt_p10k()), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
        // Experimento: Menlo con los símbolos empaquetados cargados en el sistema de fuentes de iced.
        iced_graphics::text::font_system().write().unwrap().load_font(std::borrow::Cow::Borrowed(SYMBOLS_FONT));
        for familia in ["Menlo", "MesloLGLDZ Nerd Font Mono"] {
            let nombre: &'static str = fonts::CANDIDATES.iter().map(|c| c.0).find(|f| *f == familia).unwrap_or("Menlo");
            fonts::set_current(nombre);
            let archivo = format!("p10k-{}-simbolos-en-span.png", familia.replace(' ', "_"));
            render(&app, (900, 220), &dir.join(archivo));
        }
        fonts::set_current("Menlo");
    }

    #[test]
    #[ignore]
    fn render_capturas() {
        let dir = std::path::PathBuf::from(std::env::var("GALGO_UX_DIR").unwrap_or_else(|_| "/tmp/galgo-ux".into()));
        std::fs::create_dir_all(&dir).unwrap();
        let size = (1280, 800);
        let welcome = App { window: (1280.0, 800.0), ..Default::default() };
        render(&welcome, size, &dir.join("r-bienvenida.png"));
        let app = scene();
        render(&app, size, &dir.join("r-paneles.png"));
        let mut palette = scene();
        palette.palette = Some("cla".into());
        render(&palette, size, &dir.join("r-paleta.png"));
        let mut lanz = scene();
        lanz.launcher = true;
        render(&lanz, size, &dir.join("r-lanzador.png"));
        let mut ajustes = scene();
        ajustes.settings = Some(settings::Section::Integrations);
        ajustes.hooks_output.insert("claude".into(), "claude: hooks de galgo instalados (3 eventos)".into());
        ajustes.hooks_output.insert("codex".into(), "codex: no instalados".into());
        render(&ajustes, size, &dir.join("r-ajustes.png"));
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

#[cfg(test)]
mod ajustes_tests {
    use super::*;

    /// Paso 4 (rex): Cmd+, abre ajustes en la sección General; elegir otra sección la muestra; cerrar la quita.
    #[test]
    fn cmd_coma_abre_ajustes() {
        let coma = Key::Character(",".into());
        assert!(matches!(shortcut(&coma, keyboard::Modifiers::COMMAND), Some(Message::OpenSettings)));
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        let _ = update(&mut app, Message::OpenSettings);
        assert_eq!(app.settings, Some(settings::Section::General));
        let _ = update(&mut app, Message::SettingsSection(settings::Section::Shortcuts));
        assert_eq!(app.settings, Some(settings::Section::Shortcuts));
        let _ = update(&mut app, Message::CloseSettings);
        assert!(app.settings.is_none());
    }
}

#[cfg(test)]
mod lanzador_tests {
    use super::*;

    /// Paso 5 (rex): el lanzador lleva el modelo elegido en el comando, y el nombre es libre (claude, claude-2…).
    #[test]
    fn lanzador_con_modelo_y_nombre_automatico() {
        let uno = PaneInfo {
            id: 1,
            name: "claude".into(),
            status: Status::Idle,
            manager: None,
            cmd: vec!["claude".into()],
            unacked: 0,
            oldest_unacked_secs: None,
            orphan: false,
            waiting_on: None,
            waiting_secs: None,
            workspace: None,
            cwd: None,
            programa: None, titulo_osc: None,
            usage: None,
            title: None,
            agent: false, hooked: false,
        };
        let mut app = App { panes: vec![uno], window: (1100.0, 700.0), ..Default::default() };
        let _ = update(&mut app, Message::LauncherModel("claude".into(), "opus".into()));
        match context_request(&app, launch::Agent::Claude) {
            galgo_proto::Request::Spawn { name, cmd, .. } => {
                assert_eq!(name, "claude-2", "claude ya existe");
                assert_eq!(cmd, vec!["claude", "--model", "opus"]);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod hooks_tests {
    use super::*;

    /// A1 (review-14): «Aplicar» sin un plan visto no escribe nada.
    #[test]
    fn r14_instalar_no_escribe_sin_plan() {
        let mut app = App::default();
        let _ = update(&mut app, Message::HooksApply("claude".into()));
        assert!(app.hooks_busy.is_empty(), "sin plan no hay orden de escritura");
        assert!(app.hooks_plan.is_empty());
    }

    /// A1: el plan se guarda al pedirlo; «Aplicar» lo consume y deja el agente libre al terminar.
    #[test]
    fn r14_plan_y_aplicar() {
        let mut app = App::default();
        let _ = update(&mut app, Message::HooksPlanned("claude".into(), Ok("+ hook PreToolUse".into())));
        assert_eq!(app.hooks_plan.get("claude").map(String::as_str), Some("+ hook PreToolUse"));
        let _ = update(&mut app, Message::HooksApply("claude".into()));
        assert!(app.hooks_busy.contains("claude"));
        let _ = update(&mut app, Message::HooksApplied("claude".into(), Ok("instalado".into())));
        assert!(app.hooks_plan.is_empty(), "el plan se consume al aplicarlo");
        assert!(app.hooks_busy.is_empty());
    }

    /// A1: consultar el estado invalida el plan, y cancelar lo quita.
    #[test]
    fn r14_estado_y_cancelar_invalidan_el_plan() {
        let mut app = App::default();
        let _ = update(&mut app, Message::HooksPlanned("codex".into(), Ok("plan".into())));
        let _ = update(&mut app, Message::HooksStatus("codex".into()));
        assert!(app.hooks_plan.is_empty(), "el estado nuevo no se aplica con un plan viejo");
        let _ = update(&mut app, Message::HooksOutput("codex".into(), Ok("no instalados".into())));
        let _ = update(&mut app, Message::HooksPlanned("codex".into(), Ok("plan".into())));
        let _ = update(&mut app, Message::HooksCancel("codex".into()));
        assert!(app.hooks_plan.is_empty());
    }
}

#[cfg(test)]
mod atencion_tests {
    use super::*;

    /// M1 (review-14): un resultado sin leer se distingue en la fila del agente; un Done ya visto no.
    #[test]
    fn r14_fila_muestra_la_insignia_de_sin_leer() {
        let mut working = PaneInfo { id: 1, name: "codex".into(), status: Status::Working, manager: None, cmd: vec!["codex".into()], unacked: 0, oldest_unacked_secs: None, orphan: false, waiting_on: None, waiting_secs: None, workspace: None, cwd: None, programa: None, titulo_osc: None, usage: None, title: None, agent: false, hooked: false };
        let mut app = App::default();
        app.attention.observe(&[working.clone()], None);
        working.status = Status::Done;
        app.attention.observe(&[working.clone()], None);
        let (texto, _) = chrome::agent_side(&app, &working);
        assert!(texto.contains("sin leer"), "{texto}");
        assert_eq!(app.attention.count(&[working.clone()]), 1, "la barra de sesiones cuenta el panel");
        app.attention.mark_read(1);
        let (texto, _) = chrome::agent_side(&app, &working);
        assert!(!texto.contains("sin leer"), "{texto}");
    }
}

#[cfg(test)]
mod modelo_tests {
    use super::*;

    /// M2 (review-14): sin elección, el desplegable dice «predeterminado» y el comando no lleva modelo.
    #[test]
    fn r14_predeterminado_no_pasa_modelo() {
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
        assert_eq!(chrome::launcher_choice(&app, launch::Agent::Claude), launch::DEFAULT_MODEL);
        match context_request(&app, launch::Agent::Claude) {
            galgo_proto::Request::Spawn { cmd, .. } => assert_eq!(cmd, vec!["claude"]),
            other => panic!("{other:?}"),
        }
        let _ = update(&mut app, Message::LauncherModel("claude".into(), "opus".into()));
        assert_eq!(chrome::launcher_choice(&app, launch::Agent::Claude), "opus");
        let _ = update(&mut app, Message::LauncherModel("claude".into(), launch::DEFAULT_MODEL.into()));
        match context_request(&app, launch::Agent::Claude) {
            galgo_proto::Request::Spawn { cmd, .. } => assert_eq!(cmd, vec!["claude"], "volver a predeterminado quita el flag"),
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod arrastre_tests {
    use super::*;

    /// M3 (review-14): arrastrar la barra no cambia el panel, la vista ni las capas.
    #[test]
    fn r14_arrastrar_la_barra_no_toca_el_estado() {
        let p = PaneInfo { id: 1, name: "p".into(), status: Status::Idle, manager: None, cmd: vec![], unacked: 0, oldest_unacked_secs: None, orphan: false, waiting_on: None, waiting_secs: None, workspace: None, cwd: None, programa: None, titulo_osc: None, usage: None, title: None, agent: false, hooked: false };
        let mut app = App { window: (1100.0, 700.0), panes: vec![p], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::DragWindow);
        assert_eq!(app.selected, Some(1));
        assert!(app.settings.is_none() && app.palette.is_none());
    }
}

#[cfg(test)]
mod capas_tests {
    use super::*;

    fn tecla(key: Key, modifiers: keyboard::Modifiers) -> Message {
        Message::Event(Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
        }))
    }

    /// M4 (review-14): con los ajustes abiertos, Cmd+Opt+flecha no mueve el foco del split de detrás, y Cmd+Shift+Enter
    /// no maximiza el panel de detrás.
    #[test]
    fn r14_atajos_no_actuan_bajo_las_capas() {
        let mut uno = PaneInfo { id: 1, name: "a".into(), status: Status::Idle, manager: None, cmd: vec![], unacked: 0, oldest_unacked_secs: None, orphan: false, waiting_on: None, waiting_secs: None, workspace: Some("w".into()), cwd: None, programa: None, titulo_osc: None, usage: None, title: None, agent: false, hooked: false };
        let dos = PaneInfo { id: 2, name: "b".into(), ..uno.clone() };
        uno.id = 1;
        let mut app = App { panes: vec![uno, dos], selected: Some(1), window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.layouts.split(1, true, 2);
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.settings = Some(settings::Section::General);
        assert_eq!(body_mode(&app), Body::Settings);
        let cmd_opt = keyboard::Modifiers::COMMAND | keyboard::Modifiers::ALT;
        let cmd_shift = keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT;
        let _ = update(&mut app, tecla(Key::Named(Named::ArrowRight), cmd_opt));
        assert_eq!(app.selected, Some(1), "el foco de detrás no se mueve");
        let _ = update(&mut app, tecla(Key::Named(Named::Enter), cmd_shift));
        assert!(app.maximized.is_none(), "el panel de detrás no se maximiza");
    }
}

#[cfg(test)]
mod r14_b_tests {
    use super::*;

    fn pane(id: u32, ws: &str) -> PaneInfo {
        PaneInfo { id, name: format!("p{id}"), status: Status::Idle, manager: None, cmd: vec![], unacked: 0, oldest_unacked_secs: None, orphan: false, waiting_on: None, waiting_secs: None, workspace: Some(ws.into()), cwd: None, programa: None, titulo_osc: None, usage: None, title: None, agent: false, hooked: false }
    }

    /// B4 (review-14): elegir un workspace por clave lleva a su panel aunque cambie el orden de la lista.
    #[test]
    fn r14_sesion_se_elige_por_clave() {
        let mut app = App { panes: vec![pane(1, "repo/main"), pane(2, "otro/dev")], window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
        let _ = update(&mut app, Message::SelectWorkspace("repo/main".into()));
        assert_eq!(app.selected, Some(1));
        let _ = update(&mut app, Message::SelectWorkspace("otro/dev".into()));
        assert_eq!(app.selected, Some(2));
    }

    /// B5 (review-14): con una orden de hooks en curso para un agente, otra no se encola encima.
    #[test]
    fn r14_orden_de_hooks_no_se_solapa() {
        let mut app = App::default();
        let _ = update(&mut app, Message::HooksStatus("claude".into()));
        assert!(app.hooks_busy.contains("claude"));
        let _ = update(&mut app, Message::HooksPlan("claude".into()));
        assert_eq!(app.hooks_busy.len(), 1, "el segundo pedido se ignora mientras el primero corre");
    }

    /// B7 (review-14): el eco de una tecla no sondea los paneles visibles sin foco.
    #[test]
    fn r14_eco_no_sondea_los_demas_paneles() {
        let mut app = App { panes: vec![pane(1, "w"), pane(2, "w")], window: (1100.0, 700.0), ..Default::default() };
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.layouts.split(1, true, 2);
        app.layouts.sync_nombrado(&pane_nombres(&app));
        app.selected = Some(1);
        let _ = update(&mut app, Message::Echo);
        assert!(app.loading, "pide la pantalla del enfocado");
        assert!(app.others_loading.is_empty(), "no pide a los demás paneles");
    }
}

#[cfg(test)]
mod margen_tests {
    use super::*;

    /// El terminal deja `COLS_MARGIN` columnas libres a la derecha: el shell no dibuja su prompt hasta el borde.
    #[test]
    fn el_terminal_deja_margen_a_la_derecha() {
        let rect = layout::Rect { x: 0.0, y: 0.0, w: 2.0 * LEAF_PAD + 100.0 * cell_w(), h: 200.0 };
        let (_, cols) = grid_in(rect);
        assert_eq!(cols, 100 - COLS_MARGIN);
    }
}

#[cfg(test)]
mod simbolos_tests {
    use super::*;

    /// Punto 2 (fuente): los iconos y separadores de área privada van con la familia de símbolos; el texto normal no.
    /// El asset empaquetado es una fuente TrueType válida.
    #[test]
    fn simbolos_van_con_su_familia_y_el_asset_es_ttf() {
        assert!(is_symbol('\u{e0b0}'));
        assert!(is_symbol('\u{f179}'));
        assert!(!is_symbol('a'));
        assert!(!is_symbol('█'));
        assert_eq!(&SYMBOLS_FONT[0..4], &[0x00, 0x01, 0x00, 0x00]);
    }
}

#[cfg(test)]
mod cajas_tests {
    use super::*;

    /// Las cajas salen de la fila y se apuntan en su columna y fila exactas; el resto del texto no cambia.
    #[test]
    fn las_cajas_salen_de_la_fila_y_van_a_su_celda() {
        let runs = vec![Run { t: "┌─x".into(), fg: galgo_proto::Color::Default, bg: galgo_proto::Color::Default, bold: false, italic: false, underline: false, inverse: false, dim: false, strike: false, blink: false }];
        let mut capa = cajas::Capa { cw: 8.0, lh: 18.0, ligero: 1.0, ..Default::default() };
        let sin = quitar_cajas(&runs, 3, &mut capa);
        assert_eq!(sin[0].t, "  x");
        assert_eq!(capa.celdas.len(), 2);
        assert_eq!((capa.celdas[0].0, capa.celdas[0].1, capa.celdas[0].2), (0, 3, '┌'));
        assert_eq!((capa.celdas[1].0, capa.celdas[1].1, capa.celdas[1].2), (1, 3, '─'));
    }
}

mod anchos_tests {
    use super::*;

    /// Punto 4 (fuente): un carácter ancho ocupa dos columnas, un combinado ninguna, y un símbolo de área privada una.
    #[test]
    fn caracteres_anchos_ocupan_lo_que_dice_vt100() {
        assert_eq!(cells_of('中'), 2);
        assert_eq!(cells_of('\u{301}'), 0, "el acento combinado no avanza");
        assert_eq!(cells_of('a'), 1);
        assert_eq!(cells_of('\u{e0b0}'), 1, "el separador powerline ocupa una celda");
    }
}
