use galgo_proto::{client, PaneInfo, Request, Status};
use iced::{widget::{button, column, container, lazy, pick_list, row, scrollable, text, text_input, Space}, Color, Element, Font, Length, Task, Theme};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;
use std::time::Duration;

const BG: Color = Color::from_rgb(0.059, 0.067, 0.082);
const SURFACE: Color = Color::from_rgb(0.083, 0.094, 0.113);
const TEXT: Color = Color::from_rgb(0.906, 0.914, 0.937);
const MUTED: Color = Color::from_rgb(0.60, 0.64, 0.70);

mod attention;
mod newpane;
mod term;

use attention::{Alert, Attention, Badge};
use galgo_proto::{Run, Snapshot};
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
const TERM_ORIGIN: (f32, f32) = (SIDEBAR_W + 12.0 + 10.0, 12.0 + 44.0 + 10.0 + 10.0);
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
    attention: Attention,
    /// Formulario de "Nuevo panel", abierto mientras existe.
    form: Option<NewPane>,
    form_error: Option<String>,
    /// Pide confirmación antes de cerrar el panel seleccionado.
    confirm_close: bool,
    /// La ventana tiene el foco: si no, las alertas van a notificación de macOS.
    focused: bool,
}

#[derive(Debug, Clone)]
enum Message {
    TickScreen,
    TickList,
    Panes(Result<Vec<PaneInfo>, String>),
    /// Respuesta de una petición de pantalla: id, offset pedido, `since` enviado y el resultado.
    Screen(u32, u16, Option<u64>, Result<ScreenReply, String>),
    Select(u32),
    Event(Event),
    Paste(Option<String>),
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
fn grid_for(window: (f32, f32)) -> (u16, u16) {
    let w = window.0 - SIDEBAR_W - 24.0 - 20.0;
    let h = window.1 - 24.0 - 44.0 - 20.0 - 10.0;
    (((h / LINE_H).floor().max(5.0)) as u16, ((w / CELL_W).floor().max(20.0)) as u16)
}

/// Pantalla que se muestra: la congelada si hay selección, si no la última recibida.
fn shown(app: &App) -> Option<&Snapshot> {
    app.sel_snap.as_ref().or(app.snap.as_ref())
}

/// Celda de la cuadrícula bajo el píxel (x, y), con el origen del terminal restado.
/// Fuera de la cuadrícula por la derecha o por abajo se recorta al borde; por la izquierda o
/// por arriba devuelve `None`.
fn pixel_to_cell(x: f32, y: f32, grid: (u16, u16)) -> Option<Cell> {
    let (rows, cols) = grid;
    if rows == 0 || cols == 0 {
        return None;
    }
    let (ox, oy) = TERM_ORIGIN;
    if x < ox || y < oy {
        return None;
    }
    let col = ((x - ox) / CELL_W).floor() as u16;
    let row = ((y - oy) / LINE_H).floor() as u16;
    Some((row.min(rows - 1), col.min(cols - 1)))
}

/// Si el píxel cae dentro de la cuadrícula visible (no en el recorte de los bordes).
fn inside_grid(x: f32, y: f32, grid: (u16, u16)) -> bool {
    let (ox, oy) = TERM_ORIGIN;
    x >= ox && y >= oy && x < ox + grid.1 as f32 * CELL_W && y < oy + grid.0 as f32 * LINE_H
}

fn pointer_in_grid(app: &App) -> bool {
    match (app.pointer, shown(app)) {
        (Some(p), Some(s)) => inside_grid(p.x, p.y, (s.rows, s.cols)),
        _ => false,
    }
}

fn grid_cell(app: &App, p: Point) -> Option<Cell> {
    let snap = shown(app)?;
    pixel_to_cell(p.x, p.y, (snap.rows, snap.cols))
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
    let (rows, cols) = grid_for(app.window);
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
            match app.selected {
                Some(id) if !app.loading => {
                    app.loading = true;
                    // Solo se pide "lo que cambió" si la última respuesta completa es del mismo desplazamiento.
                    let since = if app.snap_offset == app.scroll { app.snap_gen } else { None };
                    load_screen(id, app.scroll, since)
                }
                _ => Task::none(),
            }
        }
        Message::TickList if !app.listing => { app.listing = true; load_panes() }
        Message::TickList => Task::none(),
        Message::Panes(result) => {
            app.listing = false;
            match result {
                Ok(panes) => {
                    app.error = None;
                    app.panes = panes;
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
            // Con el formulario abierto, el teclado es del formulario, no del panel.
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
        Message::OpenForm => {
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

fn view(app: &App) -> Element<'_, Message> {
    let mut list = column![
        row![text("PANELES").size(12).color(MUTED), Space::with_width(Length::Fill), text(app.panes.len()).size(12).color(MUTED)].padding([0, 4]),
        button(text("+ Nuevo panel").size(13)).width(Length::Fill).on_press(Message::OpenForm),
    ].spacing(8);
    if app.panes.is_empty() { list = list.push(text("No hay paneles todavía.").size(14).color(MUTED)); }
    for pane in &app.panes {
        let mut notes = Vec::new();
        if pane.orphan { notes.push("ENTREGA SIN CONFIRMAR".to_string()); }
        else if pane.unacked > 0 { notes.push(format!("{} pendientes", pane.unacked)); }
        if let Some(manager) = &pane.manager { notes.push(format!("reporta a {manager}")); }
        if let Some(waiting) = &pane.waiting_on { notes.push(format!("espera a {waiting}")); }
        let mut info = column![text(&pane.name).size(15), text(pane.status.as_str().to_uppercase()).size(11).color(status_color(pane.status))].spacing(5);
        if let Some(b) = attention::badge(pane, app.attention.is_unread(pane.id)) {
            let (label, color) = badge_look(b);
            info = info.push(row![text("●").size(11).color(color), text(label).size(11).color(color)].spacing(6));
        }
        if !notes.is_empty() { info = info.push(text(notes.join(" · ")).size(11).color(if pane.orphan { status_color(Status::Blocked) } else { MUTED })); }
        let selected = app.selected == Some(pane.id);
        list = list.push(button(container(info).width(Length::Fill).padding(10)).width(Length::Fill).on_press(Message::Select(pane.id)).style(move |_, status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style { background: Some(if selected { Color::from_rgb(0.14, 0.17, 0.20) } else if hover { Color::from_rgb(0.11, 0.12, 0.15) } else { SURFACE }.into()), text_color: TEXT, border: iced::border::rounded(6), ..Default::default() }
        }));
    }
    let sidebar = container(scrollable(list).height(Length::Fill)).width(260).height(Length::Fill).padding(16).style(|_| container::Style { background: Some(SURFACE.into()), ..Default::default() });
    let selected_pane = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id));
    let title = selected_pane.map(|p| p.name.as_str()).unwrap_or("Sin paneles");
    let state = selected_pane.map(|p| p.status.as_str().to_uppercase()).unwrap_or_default();
    let state_color = selected_pane.map(|p| status_color(p.status)).unwrap_or(MUTED);

    let body_content: Element<'_, Message> = if let Some(f) = &app.form {
        form_view(app, f)
    } else {
        let screen: Element<'_, Message> = match shown(app) {
            // Con `lazy`, el árbol de la pantalla solo se reconstruye cuando cambia lo que se muestra.
            Some(snap) => {
                let key = (app.selected, app.snap_gen, app.snap_offset, app.sel, app.sel_snap.as_ref().map(|s| s.gen));
                let sel = app.sel;
                lazy(key, move |_| screen_view(snap, sel)).into()
            }
            None => text(if app.selected.is_none() { "Crea un panel con «+ Nuevo panel» o con `galgo spawn`." } else { "Esperando salida…" }).size(14).color(MUTED).into(),
        };
        container(screen).width(Length::Fill).height(Length::Fill).padding(10).clip(true)
            .style(|_| container::Style { background: Some(term::DEFAULT_BG.into()), border: iced::border::rounded(8), ..Default::default() })
            .into()
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
    let mut body = column![header, body_content].spacing(10).padding(12).width(Length::Fill).height(Length::Fill);
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
        .run_with(|| (App { window: (1100.0, 700.0), focused: true, ..Default::default() }, load_panes()))
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
        assert_eq!(pixel_to_cell(x, y, (10, 80)), Some((2, 3)));
        // Esquina superior izquierda del área del terminal.
        assert_eq!(pixel_to_cell(TERM_ORIGIN.0, TERM_ORIGIN.1, (10, 80)), Some((0, 0)));
    }

    #[test]
    fn pixel_a_celda_recorta_al_borde_y_rechaza_fuera_por_la_izquierda() {
        assert_eq!(pixel_to_cell(TERM_ORIGIN.0 + 10_000.0, TERM_ORIGIN.1 + 10_000.0, (10, 80)), Some((9, 79)));
        assert_eq!(pixel_to_cell(TERM_ORIGIN.0 - 1.0, TERM_ORIGIN.1 + 5.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(TERM_ORIGIN.0 + 5.0, TERM_ORIGIN.1 - 1.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(50.0, 50.0, (10, 80)), None);
        assert_eq!(pixel_to_cell(TERM_ORIGIN.0, TERM_ORIGIN.1, (0, 80)), None);
    }

    /// B3: la rueda y el clic solo cuentan dentro de la cuadrícula.
    #[test]
    fn dentro_de_la_cuadricula_solo() {
        assert!(inside_grid(TERM_ORIGIN.0, TERM_ORIGIN.1, (10, 80)));
        assert!(!inside_grid(TERM_ORIGIN.0 + 10_000.0, TERM_ORIGIN.1, (10, 80)));
        assert!(!inside_grid(TERM_ORIGIN.0, TERM_ORIGIN.1 + 10.0 * LINE_H, (10, 80)));
        assert!(!inside_grid(50.0, 300.0, (10, 80)));
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
}
