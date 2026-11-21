use galgo_proto::{client, PaneInfo, Request, Status};
use iced::{widget::{button, column, container, lazy, pick_list, progress_bar, row, scrollable, text, text_input, Space}, Color, Element, Font, Length, Task, Theme};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;
use std::time::Duration;

const BG: Color = Color::from_rgb(0.059, 0.067, 0.082);
const SURFACE: Color = Color::from_rgb(0.083, 0.094, 0.113);
const TEXT: Color = Color::from_rgb(0.906, 0.914, 0.937);
const MUTED: Color = Color::from_rgb(0.60, 0.64, 0.70);

mod attention;
mod launch;
mod layout;
mod newpane;
mod palette;
mod recents;
mod sidebar;
mod term;
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
const SIDEBAR_W: f32 = 260.0;
/// Líneas de historial por cada paso de la rueda.
const WHEEL_LINES: i32 = 3;
/// Origen (esquina superior izquierda) de la cuadrícula dentro de la ventana: sidebar, padding del
/// cuerpo (12), padding del contenedor del terminal (10), cabecera (44) y espacios (10).
/// Altura de la barra de pestañas (debajo de la cabecera).
const TABS_H: f32 = 28.0;
/// Altura de la línea de detalle de uso, cuando el panel enfocado tiene datos (con su separación).
const DETAIL_H: f32 = 26.0;
/// Origen de la cuadrícula de un panel único (referencia de las pruebas); un split usa el de su rectángulo.
const TERM_ORIGIN: (f32, f32) = (SIDEBAR_W + 12.0 + 10.0, 12.0 + 44.0 + 10.0 + TABS_H + 10.0 + 10.0);
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
    size_sent: Option<(u32, u16, u16)>,
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
    pending_split: Option<(u32, bool)>,
    /// Últimas pantallas de los paneles visibles que no tienen el foco, y su generación.
    others: std::collections::HashMap<u32, Snapshot>,
    others_gen: std::collections::HashMap<u32, u64>,
    others_loading: std::collections::HashSet<u32>,
    others_cursor: usize,
}

#[derive(Debug, Clone)]
enum Message {
    TickScreen,
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
    let commands: [(&str, &str, Message); 9] = [
        ("Nuevo workspace…", "Cmd+N", Message::OpenNewWorkspace),
        ("Nuevo terminal en este workspace", "Cmd+T", Message::NewTab),
        ("Lanzar agente…", "Cmd+Shift+L", Message::OpenLauncher),
        ("Cerrar panel", "Cmd+W", Message::CloseFocused),
        ("Siguiente que necesita atención", "Cmd+Shift+U", Message::NextAttention),
        ("Lanzar Claude aquí", "", Message::Launch(launch::Agent::Claude)),
        ("Lanzar Codex aquí", "", Message::Launch(launch::Agent::Codex)),
        ("Lanzar PI aquí", "", Message::Launch(launch::Agent::Pi)),
        ("Nuevo panel con formulario", "", Message::OpenForm),
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
        ("t", false) => Some(Message::NewTab),
        ("w", false) => Some(Message::CloseFocused),
        ("[", false) => Some(Message::NextTab(-1)),
        ("]", false) => Some(Message::NextTab(1)),
        ("d", false) => Some(Message::SplitFocused(true)),
        ("d", true) => Some(Message::SplitFocused(false)),
        ("l", true) => Some(Message::OpenLauncher),
        ("u", true) => Some(Message::NextAttention),
        (d, false) => match d.parse::<usize>() {
            Ok(n @ 1..=9) => Some(Message::SelectWorkspaceAt(n - 1)),
            _ => None,
        },
        _ => None,
    }
}

/// Workspace n-ésimo (0 = primero) en el orden de la barra lateral; se selecciona su primer panel.
fn workspace_at(panes: &[PaneInfo], n: usize) -> Option<u32> {
    sidebar::workspaces(panes).get(n).and_then(|w| w.panes.first()).map(|p| p.id)
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
    Task::perform(
        async move { tokio::task::spawn_blocking(move || request(req)).await.map_err(|e| e.to_string()).and_then(|r| r) },
        Message::PaneLaunched,
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
    app.panes.iter().map(|p| p.name.as_str()).collect()
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

fn welcome_view(app: &App) -> Element<'_, Message> {
    let mut col = column![
        text("Empieza un workspace").size(26),
        text("Un workspace es una carpeta de trabajo: sus terminales y agentes viven en ella.").size(13).color(MUTED),
        button(text("Abrir carpeta…   (Cmd+N)").size(15)).width(Length::Fill).padding(12).on_press(Message::PickFolder),
        button(text("Nuevo terminal en el inicio").size(15)).width(Length::Fill).padding(12).on_press(Message::OpenFolder(home_dir())),
    ]
    .spacing(12)
    .max_width(480);
    if let Some(last) = app.recents.first() {
        col = col.push(
            button(text(format!("Reabrir lo último: {last}")).size(13)).width(Length::Fill).padding(10).on_press(Message::OpenFolder(last.clone())),
        );
    }
    for p in app.recents.iter().skip(1).take(4) {
        col = col.push(button(text(p.as_str()).size(12).color(MUTED)).width(Length::Fill).padding(8).on_press(Message::OpenFolder(p.clone())));
    }
    container(col).width(Length::Fill).height(Length::Fill).align_x(iced::alignment::Horizontal::Center).align_y(iced::alignment::Vertical::Center).into()
}

/// Barra lateral: «Te necesitan» arriba, luego Proyecto > Workspace > paneles. Cada workspace se pliega.
fn sidebar_tree_view<'a>(app: &'a App, mut list: iced::widget::Column<'a, Message>) -> iced::widget::Column<'a, Message> {
    let needs = sidebar::needs_attention(&app.panes, |id| app.attention.is_unread(id));
    if !needs.is_empty() {
        list = list.push(text(format!("TE NECESITAN · {}", needs.len())).size(11).color(status_color(Status::Blocked)));
        for pane in needs {
            list = list.push(pane_button(app, pane));
        }
    }
    for project in sidebar::tree(&app.panes) {
        list = list.push(text(project.name.clone()).size(14).color(MUTED));
        for ws in project.workspaces {
            let key = ws.label.clone().unwrap_or_default();
            let folded = app.collapsed.contains(&key);
            let arrow = if folded { "▸" } else { "▾" };
            // Estados y atención del workspace; luego el coste total, si hay uso medido.
            let mut detail = sidebar::status_summary(&ws.panes);
            let attention_note = attention::group_summary(&ws.panes, &app.attention);
            if !attention_note.is_empty() {
                detail = if detail.is_empty() { attention_note } else { format!("{detail} · {attention_note}") };
            }
            if let Some(total) = usage::group_total(&ws.panes) {
                detail = if detail.is_empty() { total } else { format!("{detail} · {total}") };
            }
            let head = column![text(format!("{arrow} {}", ws.name)).size(13), text(detail).size(11).color(MUTED)].spacing(2);
            list = list.push(
                button(container(head).width(Length::Fill).padding([6, 8]))
                    .width(Length::Fill)
                    .on_press(Message::ToggleGroup(key))
                    .style(|_, _| button::Style { background: None, text_color: TEXT, ..Default::default() }),
            );
            if !folded {
                for pane in ws.panes {
                    list = list.push(row![Space::with_width(12), pane_button(app, pane)]);
                }
            }
        }
    }
    list
}

/// Barra de pestañas del workspace enfocado, con «+», y los botones de dividir.
fn tabs_bar(app: &App) -> Element<'_, Message> {
    let mut bar = row![].spacing(6).align_y(iced::Alignment::Center).height(TABS_H);
    if let Some(ws) = focus_key(app).and_then(|k| app.layouts.by_key.get(&k)) {
        for (i, tab) in ws.tabs.iter().enumerate() {
            let name = app.panes.iter().find(|p| p.id == tab.focus).map_or("?", |p| p.name.as_str());
            let marca = if i == ws.active { "▸ " } else { "" };
            bar = bar.push(button(text(format!("{marca}{}  {name}", i + 1)).size(12)).padding([4, 10]).on_press(Message::SelectTab(i)));
        }
    }
    bar = bar.push(button(text("+").size(12)).padding([4, 10]).on_press(Message::NewTab));
    bar = bar.push(button(text("Dividir →").size(12)).padding([4, 10]).on_press(Message::SplitFocused(true)));
    bar = bar.push(button(text("Dividir ↓").size(12)).padding([4, 10]).on_press(Message::SplitFocused(false)));
    bar.into()
}

/// El área de paneles: la pestaña visible del workspace enfocado, con sus splits.
fn layout_view(app: &App) -> Element<'_, Message> {
    match focus_key(app).and_then(|k| app.layouts.active_tab(&k)) {
        Some(tab) => node_view(app, &tab.root),
        None => text("Esperando salida…").size(14).color(MUTED).into(),
    }
}

/// Un nodo del layout: una hoja es un panel; un split reparte su área en dos mitades iguales.
fn node_view<'a>(app: &'a App, node: &'a layout::Node) -> Element<'a, Message> {
    match node {
        layout::Node::Leaf(id) => leaf_view(app, *id),
        layout::Node::Split { vertical, a, b } => {
            let first = container(node_view(app, a)).width(Length::FillPortion(1)).height(Length::Fill);
            let second = container(node_view(app, b)).width(Length::FillPortion(1)).height(Length::Fill);
            if *vertical {
                row![first, second].width(Length::Fill).height(Length::Fill).into()
            } else {
                column![first, second].width(Length::Fill).height(Length::Fill).into()
            }
        }
    }
}

/// Un panel: el enfocado con su vista congelada y su cursor; los demás, con la última pantalla que llegó.
fn leaf_view(app: &App, id: u32) -> Element<'_, Message> {
    let focused = app.selected == Some(id);
    let inner: Element<'_, Message> = if focused {
        match shown(app) {
            // Con `lazy`, el árbol de la pantalla solo se reconstruye cuando cambia lo que se muestra.
            Some(snap) => {
                let key = (app.selected, app.snap_gen, app.snap_offset, app.sel, app.sel_snap.as_ref().map(|s| s.gen));
                let sel = app.sel;
                lazy(key, move |_| screen_view(snap, sel)).into()
            }
            None => text(if app.selected.is_none() { "Crea un panel con «+ Nuevo panel» o con `galgo spawn`." } else { "Esperando salida…" }).size(14).color(MUTED).into(),
        }
    } else {
        match app.others.get(&id) {
            Some(snap) => screen_view(snap, None),
            None => text("Esperando salida…").size(14).color(MUTED).into(),
        }
    };
    let borde = if focused { Color::from_rgb(0.30, 0.45, 0.80) } else { Color::TRANSPARENT };
    container(inner)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(10)
        .clip(true)
        .style(move |_| container::Style {
            background: Some(term::DEFAULT_BG.into()),
            border: iced::Border { color: borde, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
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

/// Identificador del campo de búsqueda de la paleta: para darle el foco al abrirla.
fn palette_input_id() -> iced::widget::text_input::Id {
    iced::widget::text_input::Id::new("paleta")
}

/// Paleta: búsqueda arriba y resultados debajo; el resaltado se mueve con las flechas y se ejecuta con Enter.
fn palette_view(app: &App) -> Element<'_, Message> {
    let query = app.palette.as_deref().unwrap_or("");
    let results = palette_results(app);
    let mut list = column![].spacing(4);
    for (i, item) in results.iter().enumerate().take(12) {
        let marca = if i == app.palette_cursor { "▸ " } else { "   " };
        let line = row![text(format!("{marca}{}", item.label)).size(14), Space::with_width(Length::Fill), text(item.detail.clone()).size(11).color(MUTED)];
        list = list.push(button(line).width(Length::Fill).padding(8).on_press(Message::PaletteRun(i)));
    }
    if results.is_empty() {
        list = list.push(text("Sin resultados.").size(12).color(MUTED));
    }
    column![
        text_input("Buscar workspace, panel o acción…", query)
            .id(palette_input_id())
            .on_input(Message::PaletteQuery)
            .on_submit(Message::PaletteRun(app.palette_cursor))
            .padding(10),
        scrollable(list).height(Length::Fill),
        text("↑ ↓ mover · Enter ejecutar · Esc cerrar").size(11).color(MUTED),
    ]
    .spacing(12)
    .max_width(640)
    .into()
}

/// Lanzador: un botón por agente, en el workspace y la carpeta del panel enfocado.
fn launcher_view(app: &App) -> Element<'_, Message> {
    let (workspace, cwd) = focus_context(app);
    let target = format!(
        "Workspace: {} · carpeta: {}",
        workspace.as_deref().unwrap_or("sin workspace"),
        cwd.as_deref().unwrap_or("inicio")
    );
    let mut col = column![text("Lanzar agente").size(20), text(target).size(12).color(MUTED)].spacing(10).max_width(480);
    for agent in launch::Agent::ALL {
        col = col.push(button(text(agent.label()).size(15)).width(Length::Fill).padding(12).on_press(Message::Launch(agent)));
    }
    col = col.push(button(text("Cancelar").size(13)).on_press(Message::CloseLauncher));
    container(col).width(Length::Fill).height(Length::Fill).align_x(iced::alignment::Horizontal::Center).align_y(iced::alignment::Vertical::Center).into()
}

fn new_workspace_view(app: &App) -> Element<'_, Message> {
    let query = app.new_ws.as_deref().unwrap_or("");
    let matches = recents::matching(&app.recents, query);
    let mut list = column![].spacing(6);
    for p in matches.iter().take(10) {
        list = list.push(button(text(p.as_str()).size(13)).width(Length::Fill).padding(8).on_press(Message::OpenFolder((*p).clone())));
    }
    if matches.is_empty() {
        list = list.push(text("Sin coincidencias entre las carpetas recientes.").size(12).color(MUTED));
    }
    column![
        text("Nuevo workspace").size(20),
        text("Elige una carpeta. Se abre un terminal en ella; el daemon deduce el proyecto y la rama del repo git.").size(12).color(MUTED),
        text_input("Buscar en recientes…", query).on_input(Message::NewWsQuery).on_submit(Message::NewWsSubmit).padding(8),
        scrollable(list).height(Length::Fill),
        row![
            button(text("Elegir carpeta…").size(13)).on_press(Message::PickFolder),
            button(text("Cancelar").size(13)).on_press(Message::CloseNewWorkspace),
        ]
        .spacing(8),
    ]
    .spacing(12)
    .max_width(560)
    .into()
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
    app.selected = Some(id);
    app.layouts.focus(id);
    app.snap = None;
    app.snap_gen = None;
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

/// Filas y columnas que caben en el área del terminal para el tamaño de ventana actual.
/// Área donde viven los paneles: a la derecha de la barra lateral, debajo de la cabecera, las pestañas y, si lo hay,
/// la línea de detalle de uso (`extra_top`).
fn terminal_area(window: (f32, f32), extra_top: f32) -> layout::Rect {
    let x = SIDEBAR_W + 12.0;
    let y = 12.0 + 44.0 + 10.0 + TABS_H + 10.0 + extra_top;
    layout::Rect { x, y, w: (window.0 - x - 12.0).max(0.0), h: (window.1 - y - 12.0).max(0.0) }
}

/// Cuadrícula de texto que cabe en un rectángulo de panel (con el margen de 10 px del terminal).
fn grid_in(rect: layout::Rect) -> (u16, u16) {
    (((rect.h - 20.0) / LINE_H).floor().max(5.0) as u16, ((rect.w - 20.0) / CELL_W).floor().max(20.0) as u16)
}

/// Cuadrícula de la ventana entera (un panel nuevo sin split).
fn grid_for(window: (f32, f32)) -> (u16, u16) {
    grid_in(terminal_area(window, 0.0))
}

/// Altura extra por la línea de detalle de uso, si el panel enfocado la muestra.
fn detail_extra(app: &App) -> f32 {
    let has = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id)).is_some_and(|p| p.usage.is_some());
    if has { DETAIL_H } else { 0.0 }
}

/// Área de los paneles con la altura real de la ventana y de la columna del cuerpo.
fn terminal_area_for(app: &App) -> layout::Rect {
    terminal_area(app.window, detail_extra(app))
}

/// Rectángulo del panel `id` dentro de la pestaña visible de su workspace.
fn pane_rect(app: &App, id: u32) -> Option<layout::Rect> {
    let key = app.layouts.key_of(id)?;
    let tab = app.layouts.active_tab(&key)?;
    tab.root.rects(terminal_area_for(app)).into_iter().find(|(p, _)| *p == id).map(|(_, r)| r)
}

/// Origen de la cuadrícula de un rectángulo de panel (su margen de 10 px).
fn origin_in(rect: layout::Rect) -> (f32, f32) {
    (rect.x + 10.0, rect.y + 10.0)
}

/// Origen de la cuadrícula del panel enfocado; sin rectángulo conocido, el de referencia.
fn focus_origin(app: &App) -> (f32, f32) {
    app.selected.and_then(|id| pane_rect(app, id)).map_or(TERM_ORIGIN, origin_in)
}

/// Panel visible bajo el puntero, si hay alguno.
fn leaf_at(app: &App, p: Point) -> Option<u32> {
    let key = app.selected.and_then(|id| app.layouts.key_of(id))?;
    let tab = app.layouts.active_tab(&key)?;
    tab.root.rects(terminal_area_for(app)).into_iter().find(|(_, r)| p.x >= r.x && p.y >= r.y && p.x < r.x + r.w && p.y < r.y + r.h).map(|(id, _)| id)
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
    let col = ((x - ox) / CELL_W).floor() as u16;
    let row = ((y - oy) / LINE_H).floor() as u16;
    Some((row.min(rows - 1), col.min(cols - 1)))
}

/// Si el píxel cae dentro de la cuadrícula visible (no en el recorte de los bordes).
fn inside_grid(origin: (f32, f32), x: f32, y: f32, grid: (u16, u16)) -> bool {
    let (ox, oy) = origin;
    x >= ox && y >= oy && x < ox + grid.1 as f32 * CELL_W && y < oy + grid.0 as f32 * LINE_H
}

fn pointer_in_grid(app: &App) -> bool {
    match (app.pointer, shown(app)) {
        (Some(p), Some(s)) => inside_grid(focus_origin(app), p.x, p.y, (s.rows, s.cols)),
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
fn size_request(app: &mut App) -> Option<Request> {
    let id = app.selected?;
    let (rows, cols) = pane_rect(app, id).map(grid_in).unwrap_or_else(|| grid_for(app.window));
    if app.size_sent == Some((id, rows, cols)) {
        return None;
    }
    app.size_sent = Some((id, rows, cols));
    Some(Request::Resize { pane: id.to_string(), rows, cols })
}

fn resize_if_needed(app: &mut App) {
    if let Some(req) = size_request(app) {
        let _ = outbox().send(req);
    }
}

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

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::TickScreen => {
            if let Some(e) = outbox_error().lock().unwrap().take() {
                app.error = Some(format!("no se envió: {e}"));
            }
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
            app.pending_split = Some((focus, vertical));
            spawn_task_sized(app, req, (rows, cols))
        }
        Message::FocusDir(dir) => {
            let (Some(from), Some(key)) = (app.selected, focus_key(app)) else { return Task::none() };
            let rects = match app.layouts.active_tab(&key) {
                Some(tab) => tab.root.rects(terminal_area_for(app)),
                None => return Task::none(),
            };
            match layout::neighbor(&rects, from, dir) {
                Some(id) => select(app, id),
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
                    app.size_sent = Some((p.id, rows, cols));
                    app.panes.push(p.clone());
                    // Un split coloca el panel junto al enfocado; si no, nace en su pestaña.
                    if let Some((focus, vertical)) = app.pending_split.take() {
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
            app.pending_split = None;
            app.error = Some(e);
            Task::none()
        }
        Message::SelectWorkspaceAt(n) => match workspace_at(&app.panes, n) {
            Some(id) => select(app, id),
            None => Task::none(),
        },
        Message::NextAttention => {
            let next = next_attention(&app.panes, app.selected, |id| app.attention.is_unread(id));
            match next {
                Some(id) => select(app, id),
                None => Task::none(),
            }
        }
        Message::CloseFocused => {
            let working = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id)).is_some_and(|p| p.status == Status::Working);
            if working {
                // Un agente trabajando: se pide confirmación. Un panel parado se cierra con Cmd+W sin preguntar.
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
            let n = palette_results(app).len();
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
            Task::none()
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
                    .map(|folder| folder.path().to_string_lossy().into_owned())
            },
            Message::FolderPicked,
        ),
        Message::FolderPicked(Some(path)) => open_folder(app, path),
        Message::FolderPicked(None) => Task::none(),
        Message::OpenFolder(path) => open_folder(app, path),
        Message::WorkspaceOpened(path, Ok(data)) => {
            app.new_ws = None;
            app.error = None;
            recents::push(&mut app.recents, &path);
            if let Some(file) = &app.recents_path {
                let _ = recents::save(file, &app.recents);
            }
            match serde_json::from_value::<PaneInfo>(data) {
                Ok(p) => {
                    let (rows, cols) = app.spawn_size.take().unwrap_or_else(|| grid_for(app.window));
                    app.size_sent = Some((p.id, rows, cols));
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
            // Atajos de la ventana (Cmd+…), salvo mientras «Nuevo workspace» está abierto.
            if modifiers.command() && app.new_ws.is_none() && !app.launcher {
                if let Some(message) = shortcut(&key, modifiers) {
                    return update(app, message);
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
            if let Some(bytes) = term::encode(&key, modifiers, text.as_deref()) {
                back_to_live(app);
                // Escribir en un panel lo redimensiona a la ventana, si hace falta.
                resize_if_needed(app);
                if let Some(bytes) = route_input(app, bytes) {
                    send_text(app, bytes);
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
                mouse::ScrollDelta::Pixels { y, .. } => y / LINE_H,
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
        Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))) => {
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
            if app.dragging {
                if let (Some(cell), Some(sel)) = (grid_cell(app, position), app.sel.as_mut()) {
                    sel.head = cell;
                }
            }
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
                    app.size_sent = Some((p.id, rows, cols));
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
fn styled(text: String, r: &Run, invert: bool) -> Span<'static, Message> {
    let (mut fg, mut bg) = (term::color(r.fg), term::color(r.bg));
    if r.inverse ^ invert {
        let (f, b) = (bg.unwrap_or(term::DEFAULT_BG), fg.unwrap_or(term::DEFAULT_FG));
        fg = Some(f);
        bg = Some(b);
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
        let mut chunk_inv = false;
        for ch in r.t.chars() {
            let inv = cursor_col == Some(col) || sel.is_some_and(|(a, b)| (a..=b).contains(&col));
            if !chunk.is_empty() && inv != chunk_inv {
                out.push(styled(std::mem::take(&mut chunk), r, chunk_inv));
            }
            chunk_inv = inv;
            chunk.push(ch);
            col += 1;
        }
        if !chunk.is_empty() {
            out.push(styled(chunk, r, chunk_inv));
        }
    }
    if out.is_empty() { out.push(span(" ".to_string())); }
    out
}

fn screen_view(snap: &Snapshot, sel: Option<Selection>) -> Element<'static, Message> {
    let mut lines = column![].spacing(0);
    // El cursor solo se dibuja en la pantalla en vivo.
    let live = snap.scrollback == 0;
    for (r, runs) in snap.lines.iter().enumerate() {
        let cursor = (live && !snap.cursor_hidden && snap.cursor.0 as usize == r).then_some(snap.cursor.1);
        let selected = sel.and_then(|s| selected_cols(s, r as u16, snap.cols));
        let spans = spans_for_line(runs, cursor, selected);
        lines = lines.push(
            rich_text(spans).size(FONT_SIZE).font(terminal_font()).line_height(iced::widget::text::LineHeight::Absolute(LINE_H.into())).wrapping(iced::widget::text::Wrapping::None),
        );
    }
    lines.into()
}

fn status_color(status: Status) -> Color {
    match status {
        Status::Done => Color::from_rgb(0.43, 0.83, 0.61),
        Status::Blocked => Color::from_rgb(0.94, 0.70, 0.33),
        Status::Failed | Status::Exited => Color::from_rgb(0.91, 0.43, 0.43),
        Status::Working => Color::from_rgb(0.48, 0.69, 0.96),
        Status::Idle => MUTED,
    }
}

/// Texto y color de la insignia de atención de un panel.
fn badge_look(b: Badge) -> (&'static str, Color) {
    match b {
        Badge::Blocked => ("necesita una decisión", status_color(Status::Blocked)),
        Badge::DoneUnread => ("terminó · sin leer", status_color(Status::Done)),
        Badge::Orphan => ("entrega sin confirmar", status_color(Status::Failed)),
    }
}

fn form_view<'a>(app: &'a App, f: &'a NewPane) -> Element<'a, Message> {
    let presets: Vec<String> = Preset::ALL.iter().map(|p| p.label().to_string()).collect();
    let mut managers: Vec<String> = vec![NO_MANAGER.to_string()];
    managers.extend(app.panes.iter().map(|p| p.name.clone()));
    let manager_sel = f.manager.clone().unwrap_or_else(|| NO_MANAGER.to_string());

    let mut form = column![
        text("Nuevo panel").size(18),
        text("Nombre").size(12).color(MUTED),
        text_input("p. ej. worker-tests", &f.name).on_input(Message::FormName).padding(8),
        text("Workspace").size(12).color(MUTED),
        text_input("p. ej. inventia/cotizacion (vacío = el del repo git)", &f.workspace).on_input(Message::FormWorkspace).padding(8),
        text("Comando").size(12).color(MUTED),
        pick_list(presets, Some(f.preset.label().to_string()), Message::FormPreset),
    ]
    .spacing(8)
    .width(Length::Fixed(420.0));
    if f.preset == Preset::Custom {
        form = form.push(text_input("p. ej. bash --norc", &f.custom).on_input(Message::FormCustom).padding(8));
    }
    form = form
        .push(text("Directorio").size(12).color(MUTED))
        .push(text_input(&home_dir(), &f.cwd).on_input(Message::FormCwd).padding(8))
        .push(text("Manager (recibe el resultado)").size(12).color(MUTED))
        .push(pick_list(managers, Some(manager_sel), Message::FormManager))
        .push(row![
            button(text("Crear").size(14)).on_press(Message::SubmitForm),
            button(text("Cancelar").size(14)).on_press(Message::CancelForm),
        ].spacing(10));
    if let Some(e) = &app.form_error {
        form = form.push(text(e.clone()).size(12).color(status_color(Status::Failed)));
    }
    container(form).padding(16).into()
}

/// Botón de un panel en la barra lateral: nombre, estado, insignia de atención y notas.
fn pane_button<'a>(app: &'a App, pane: &'a PaneInfo) -> Element<'a, Message> {
    let mut notes = Vec::new();
    if pane.orphan { notes.push("ENTREGA SIN CONFIRMAR".to_string()); }
    else if pane.unacked > 0 { notes.push(format!("{} pendientes", pane.unacked)); }
    if let Some(manager) = &pane.manager { notes.push(format!("reporta a {manager}")); }
    if let Some(waiting) = &pane.waiting_on { notes.push(format!("espera a {waiting}")); }
    let mut info = column![text(&pane.name).size(15), text(pane.status.as_str().to_uppercase()).size(11).color(status_color(pane.status))].spacing(5);
    if let Some((line, color)) = pane.usage.as_ref().and_then(usage::pane_line) {
        info = info.push(text(line).size(11).color(color.unwrap_or(MUTED)));
    }
    if let Some(b) = attention::badge(pane, app.attention.is_unread(pane.id)) {
        let (label, color) = badge_look(b);
        info = info.push(row![text("●").size(11).color(color), text(label).size(11).color(color)].spacing(6));
    }
    if !notes.is_empty() { info = info.push(text(notes.join(" · ")).size(11).color(if pane.orphan { status_color(Status::Blocked) } else { MUTED })); }
    let selected = app.selected == Some(pane.id);
    button(container(info).width(Length::Fill).padding(10)).width(Length::Fill).on_press(Message::Select(pane.id)).style(move |_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style { background: Some(if selected { Color::from_rgb(0.14, 0.17, 0.20) } else if hover { Color::from_rgb(0.11, 0.12, 0.15) } else { SURFACE }.into()), text_color: TEXT, border: iced::border::rounded(6), ..Default::default() }
    }).into()
}

/// Pie de la barra lateral: cuotas por agente, con barras de 5 h y 7 d, reinicio y edad del dato.
fn limits_footer(app: &App) -> Element<'_, Message> {
    let now = now_secs();
    let mut col = column![text("CUOTAS").size(12).color(MUTED)].spacing(6);
    if app.limits.is_empty() {
        let msg = match &app.limits_error {
            Some(e) => format!("cuotas no disponibles: {e}"),
            None => "sin cuotas observadas".to_string(),
        };
        col = col.push(text(msg).size(11).color(MUTED));
    }
    for l in &app.limits {
        col = col.push(text(l.agent.clone()).size(12));
        for r in usage::limit_rows(l, now) {
            let bar = row![
                text(r.label).size(11).width(40),
                progress_bar(0.0..=100.0, r.pct).height(6),
                text(format!("{:.0} %", r.pct)).size(11).width(44),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            col = col.push(bar).push(text(r.detail).size(10).color(MUTED));
        }
    }
    col.into()
}

fn view(app: &App) -> Element<'_, Message> {
    let mut list = column![
        row![text("PANELES").size(12).color(MUTED), Space::with_width(Length::Fill), text(app.panes.len()).size(12).color(MUTED)].padding([0, 4]),
        button(text("+ Nuevo workspace").size(13)).width(Length::Fill).on_press(Message::OpenNewWorkspace),
        button(text("+ Nuevo panel").size(13)).width(Length::Fill).on_press(Message::OpenForm),
    ].spacing(8);
    if app.panes.is_empty() { list = list.push(text("No hay paneles todavía.").size(14).color(MUTED)); }
    let list = sidebar_tree_view(app, list);
    let sidebar = container(column![scrollable(list).height(Length::Fill), limits_footer(app)].spacing(12)).width(260).height(Length::Fill).padding(16).style(|_| container::Style { background: Some(SURFACE.into()), ..Default::default() });
    let selected_pane = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id));
    let title = selected_pane.map(|p| p.name.as_str()).unwrap_or("Sin paneles");
    let state = selected_pane.map(|p| p.status.as_str().to_uppercase()).unwrap_or_default();
    let state_color = selected_pane.map(|p| status_color(p.status)).unwrap_or(MUTED);
    let body_content: Element<'_, Message> = if body_mode(app) == Body::NewWorkspace {
        new_workspace_view(app)
    } else if body_mode(app) == Body::Palette {
        palette_view(app)
    } else if body_mode(app) == Body::Launcher {
        launcher_view(app)
    } else if body_mode(app) == Body::Welcome {
        welcome_view(app)
    } else if let Some(f) = &app.form {
        form_view(app, f)
    } else {
        layout_view(app)
    };

    let mut header = row![text(title).size(18), text(state).size(11).color(state_color), Space::with_width(Length::Fill)].spacing(12).align_y(iced::Alignment::Center).height(44);
    if let Some(n) = shown(app).map(|s| s.scrollback).filter(|&n| n > 0) {
        header = header.push(text(format!("↑ {n}")).size(12).color(status_color(Status::Blocked)));
    }
    if app.form.is_none() && app.selected.is_some() {
        if app.confirm_close {
            header = header.push(text(format!("¿Cerrar «{title}»?")).size(12).color(status_color(Status::Blocked)))
                .push(button(text("Sí, cerrar").size(12)).on_press(Message::ConfirmClose))
                .push(button(text("No").size(12)).on_press(Message::CancelClose));
        } else {
            header = header.push(button(text("Cerrar panel").size(12)).on_press(Message::AskClose));
        }
    }
    if app.sel_snap.is_some() {
        header = header.push(text("vista congelada · Esc para volver").size(11).color(MUTED));
    }
    header = header.push(text("GALGO").size(11).color(MUTED));
    let mut body = column![header].spacing(10).padding(12).width(Length::Fill).height(Length::Fill);
    if body_mode(app) == Body::Screen {
        body = body.push(tabs_bar(app));
    }
    if let Some(u) = selected_pane.and_then(|p| p.usage.as_ref()) {
        body = body.push(text(usage::detail_line(u)).size(11).color(MUTED));
    }
    body = body.push(body_content);
    if let Some(error) = &app.error { body = body.push(text(format!("Error: {error}")).size(12).color(status_color(Status::Failed))); }
    container(row![sidebar, body]).width(Length::Fill).height(Length::Fill).style(|_| container::Style { background: Some(BG.into()), text_color: Some(TEXT), ..Default::default() }).into()
}

fn main() -> iced::Result {
    iced::application(app_title, update, view)
        .theme(|_| Theme::Dark)
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
        let mut app = App { window: (1100.0, 700.0), ..Default::default() };
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
        assert!(app.size_sent.is_none());
        // Primera escritura en el panel 1: se redimensiona una vez.
        assert!(matches!(size_request(&mut app), Some(Request::Resize { ref pane, .. }) if pane == "1"));
        assert!(size_request(&mut app).is_none());
        // Cambiar de panel sin escribir no envía nada; escribir en el 2 sí.
        app.selected = Some(2);
        assert!(app.size_sent.is_some());
        assert!(matches!(size_request(&mut app), Some(Request::Resize { ref pane, .. }) if pane == "2"));
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
        assert_eq!(app.size_sent, Some((7, pedido.0, pedido.1)));
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
        assert!(shortcut(&k("0"), cmd).is_none());
        assert!(shortcut(&k("9"), cmd).is_some());
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
        assert_eq!(workspace_at(&panes, 0), Some(2), "otro/dev va primero");
        assert_eq!(workspace_at(&panes, 1), Some(1), "repo/main, con su primer panel");
        assert_eq!(workspace_at(&panes, 2), None);
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
        let mut app = App { panes: vec![pane_info(1, "p", Status::Idle)], selected: Some(1), ..Default::default() };
        let _ = update(&mut app, Message::CloseFocused);
        assert!(!app.confirm_close, "parado: no pregunta");
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
        let total = palette_results(&app).len();
        assert!(total >= 9, "al menos los comandos");
        let _ = update(&mut app, Message::PaletteMove(-1));
        assert_eq!(app.palette_cursor, total - 1);
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
        assert_eq!(app.pending_split, Some((1, true)));
        let mut p2 = pane_info(2, "dos", Status::Idle);
        p2.workspace = Some("repo/main".into());
        let json = serde_json::to_value(p2).unwrap();
        let _ = update(&mut app, Message::PaneLaunched(Ok(json)));
        assert!(app.pending_split.is_none());
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
        let _ = update(&mut app, Message::PaneLaunched(Err("sin daemon".into())));
        assert!(app.pending_split.is_none());
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
}
