use galgo_proto::{client, PaneInfo, Request, Status};
use iced::{widget::{button, column, container, row, scrollable, text, Space}, Color, Element, Font, Length, Task, Theme};
use std::time::Duration;

const BG: Color = Color::from_rgb(0.059, 0.067, 0.082);
const SURFACE: Color = Color::from_rgb(0.083, 0.094, 0.113);
const TEXT: Color = Color::from_rgb(0.906, 0.914, 0.937);
const MUTED: Color = Color::from_rgb(0.60, 0.64, 0.70);

mod term;

use galgo_proto::{Run, Snapshot};
use iced::keyboard::{self, Key};
use iced::mouse;
use iced::widget::text::Span;
use iced::widget::{rich_text, span};
use iced::{font, Event, Point, Subscription};
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

/// (fila, columna) dentro de la pantalla mostrada.
type Cell = (u16, u16);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Selection {
    anchor: Cell,
    head: Cell,
}

fn terminal_font() -> Font {
    Font::with_name("Menlo")
}

/// Todas las peticiones de teclado y resize pasan por un solo hilo: el orden de las teclas importa.
fn outbox() -> &'static mpsc::Sender<Request> {
    static TX: OnceLock<mpsc::Sender<Request>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Request>();
        std::thread::spawn(move || {
            for req in rx {
                if let Err(e) = request(req) {
                    eprintln!("galgo-gui: {e}");
                }
            }
        });
        tx
    })
}

#[derive(Default)]
struct App {
    panes: Vec<PaneInfo>,
    selected: Option<u32>,
    snap: Option<Snapshot>,
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
}

#[derive(Debug, Clone)]
enum Message {
    TickScreen,
    TickList,
    Panes(Result<Vec<PaneInfo>, String>),
    Screen(u32, Result<Snapshot, String>),
    Select(u32),
    Event(Event),
    Paste(Option<String>),
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

fn load_screen(id: u32, offset: u16) -> Task<Message> {
    Task::perform(async move {
        tokio::task::spawn_blocking(move || {
            serde_json::from_value(request(Request::Snapshot { pane: id.to_string(), offset })?).map_err(|e| e.to_string())
        }).await.map_err(|e| e.to_string())?
    }, move |r| Message::Screen(id, r))
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

/// Vuelve a la pantalla en vivo y descarta la selección: el contenido mostrado cambia.
fn back_to_live(app: &mut App) {
    if app.scroll != 0 {
        app.scroll = 0;
    }
    app.sel = None;
}

/// Filas y columnas que caben en el área del terminal para el tamaño de ventana actual.
fn grid_for(window: (f32, f32)) -> (u16, u16) {
    let w = window.0 - SIDEBAR_W - 24.0 - 20.0;
    let h = window.1 - 24.0 - 44.0 - 20.0 - 10.0;
    (((h / LINE_H).floor().max(5.0)) as u16, ((w / CELL_W).floor().max(20.0)) as u16)
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

/// Texto de la selección: sin relleno de espacios al final de cada línea.
fn selection_text(snap: &Snapshot, sel: Selection) -> String {
    let (s, e) = ordered(sel);
    let mut lines = Vec::new();
    for row in s.0..=e.0 {
        let Some(runs) = snap.lines.get(row as usize) else { break };
        let Some((c0, c1)) = selected_cols(sel, row, snap.cols) else { continue };
        let chars: Vec<char> = runs.iter().flat_map(|r| r.t.chars()).collect();
        let part: String = chars.iter().skip(c0 as usize).take((c1 - c0 + 1) as usize).collect();
        lines.push(part.trim_end().to_string());
    }
    lines.join("\n")
}

fn grid_cell(app: &App, p: Point) -> Option<Cell> {
    let snap = app.snap.as_ref()?;
    pixel_to_cell(p.x, p.y, (snap.rows, snap.cols))
}

fn fit_size(app: &mut App) {
    let Some(id) = app.selected else { return };
    let (rows, cols) = grid_for(app.window);
    if app.size_sent != Some((id, rows, cols)) {
        app.size_sent = Some((id, rows, cols));
        let _ = outbox().send(Request::Resize { pane: id.to_string(), rows, cols });
    }
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::TickScreen => match app.selected {
            Some(id) if !app.loading => { app.loading = true; load_screen(id, app.scroll) }
            _ => Task::none(),
        },
        Message::TickList if !app.listing => { app.listing = true; load_panes() }
        Message::TickList => Task::none(),
        Message::Panes(result) => {
            app.listing = false;
            match result {
                Ok(panes) => {
                    app.error = None;
                    app.panes = panes;
                    if app.selected.is_some_and(|id| !app.panes.iter().any(|p| p.id == id)) {
                        app.selected = None; app.snap = None; app.sel = None; app.scroll = 0;
                    }
                    if app.selected.is_none() {
                        if let Some(p) = app.panes.first() { app.selected = Some(p.id); fit_size(app); }
                    }
                }
                Err(e) => app.error = Some(e),
            }
            Task::none()
        }
        Message::Screen(id, result) => {
            app.loading = false;
            if app.selected == Some(id) {
                match result {
                    Ok(snap) => {
                        app.error = None;
                        // El daemon puede haber recortado el desplazamiento al historial disponible.
                        app.scroll = app.scroll.min(snap.scrollback_max);
                        if app.snap.as_ref() != Some(&snap) { app.snap = Some(snap); }
                    }
                    Err(e) => app.error = Some(e),
                }
            }
            Task::none()
        }
        Message::Select(id) => {
            app.selected = Some(id); app.snap = None; app.sel = None; app.scroll = 0; fit_size(app);
            if app.loading { Task::none() } else { app.loading = true; load_screen(id, 0) }
        }
        Message::Event(Event::Window(iced::window::Event::Resized(size))) => {
            app.window = (size.width, size.height); fit_size(app); Task::none()
        }
        Message::Event(Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, text, .. })) => {
            if modifiers.command() && matches!(&key, Key::Character(c) if c.as_str() == "v") {
                return iced::clipboard::read().map(Message::Paste);
            }
            if modifiers.command() && matches!(&key, Key::Character(c) if c.as_str() == "c") {
                let copied = match (app.sel, app.snap.as_ref()) {
                    (Some(sel), Some(snap)) => selection_text(snap, sel),
                    _ => String::new(),
                };
                return if copied.is_empty() { Task::none() } else { iced::clipboard::write(copied) };
            }
            if let Some(bytes) = term::encode(&key, modifiers, text.as_deref()) {
                back_to_live(app);
                send_text(app, bytes);
            }
            Task::none()
        }
        Message::Event(Event::Window(iced::window::Event::Opened { size, .. })) => {
            app.window = (size.width, size.height); fit_size(app); Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::WheelScrolled { delta })) => {
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
                    app.sel = None;
                }
            }
            Task::none()
        }
        Message::Event(Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))) => {
            if let Some(cell) = app.pointer.and_then(|p| grid_cell(app, p)) {
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
                app.sel = None;
            }
            Task::none()
        }
        Message::Event(_) => Task::none(),
        Message::Paste(Some(t)) => {
            back_to_live(app);
            send_text(app, t.replace("\r\n", "\r").replace('\n', "\r"));
            Task::none()
        }
        Message::Paste(None) => Task::none(),
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

fn view(app: &App) -> Element<'_, Message> {
    let mut list = column![row![text("PANELES").size(12).color(MUTED), Space::with_width(Length::Fill), text(app.panes.len()).size(12).color(MUTED)].padding([0, 4])].spacing(8);
    if app.panes.is_empty() { list = list.push(text("No hay paneles todavía.").size(14).color(MUTED)); }
    for pane in &app.panes {
        let mut notes = Vec::new();
        if pane.orphan { notes.push("ENTREGA SIN CONFIRMAR".to_string()); }
        else if pane.unacked > 0 { notes.push(format!("{} pendientes", pane.unacked)); }
        if let Some(manager) = &pane.manager { notes.push(format!("reporta a {manager}")); }
        if let Some(waiting) = &pane.waiting_on { notes.push(format!("espera a {waiting}")); }
        let mut info = column![text(&pane.name).size(15), text(pane.status.as_str().to_uppercase()).size(11).color(status_color(pane.status))].spacing(5);
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
    let screen: Element<'_, Message> = match &app.snap {
        Some(snap) => screen_view(snap, app.sel),
        None => text(if app.selected.is_none() { "Crea un panel con `galgo spawn`." } else { "Esperando salida…" }).size(14).color(MUTED).into(),
    };
    let terminal = container(screen).width(Length::Fill).height(Length::Fill).padding(10).clip(true)
        .style(|_| container::Style { background: Some(term::DEFAULT_BG.into()), border: iced::border::rounded(8), ..Default::default() });
    let mut header = row![text(title).size(18), text(state).size(11).color(state_color), Space::with_width(Length::Fill)].spacing(12).align_y(iced::Alignment::Center).height(44);
    if let Some(n) = app.snap.as_ref().map(|s| s.scrollback).filter(|&n| n > 0) {
        header = header.push(text(format!("↑ {n}")).size(12).color(status_color(Status::Blocked)));
    }
    header = header.push(text("GALGO").size(11).color(MUTED));
    let mut body = column![header, terminal].spacing(10).padding(12).width(Length::Fill).height(Length::Fill);
    if let Some(error) = &app.error { body = body.push(text(format!("Error: {error}")).size(12).color(status_color(Status::Failed))); }
    container(row![sidebar, body]).width(Length::Fill).height(Length::Fill).style(|_| container::Style { background: Some(BG.into()), text_color: Some(TEXT), ..Default::default() }).into()
}

fn main() -> iced::Result {
    iced::application("galgo", update, view)
        .theme(|_| Theme::Dark)
        .subscription(|_: &App| Subscription::batch([
            iced::time::every(Duration::from_millis(50)).map(|_| Message::TickScreen),
            iced::time::every(Duration::from_secs(1)).map(|_| Message::TickList),
            iced::event::listen_with(|event, _status, _id| match event {
                Event::Keyboard(keyboard::Event::KeyPressed { .. }) => Some(Message::Event(event)),
                Event::Window(iced::window::Event::Resized(_)) | Event::Window(iced::window::Event::Opened { .. }) => Some(Message::Event(event)),
                Event::Mouse(mouse::Event::WheelScrolled { .. })
                | Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                | Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                | Event::Mouse(mouse::Event::CursorMoved { .. }) => Some(Message::Event(event)),
                _ => None,
            }),
        ]))
        .window_size(iced::Size::new(1100.0, 700.0))
        .run_with(|| (App { window: (1100.0, 700.0), ..Default::default() }, load_panes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &str) -> Run {
        Run { t: t.into(), fg: galgo_proto::Color::Default, bg: galgo_proto::Color::Default, bold: false, italic: false, underline: false, inverse: false }
    }

    fn snap(lines: &[&str], cols: u16) -> Snapshot {
        Snapshot {
            rows: lines.len() as u16,
            cols,
            cursor: (0, 0),
            cursor_hidden: true,
            lines: lines.iter().map(|l| vec![run(l)]).collect(),
            scrollback: 0,
            scrollback_max: 0,
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

    #[test]
    fn celdas_seleccionadas_por_fila() {
        let sel = Selection { anchor: (1, 4), head: (3, 2) };
        assert_eq!(selected_cols(sel, 0, 10), None);
        assert_eq!(selected_cols(sel, 1, 10), Some((4, 9)));
        assert_eq!(selected_cols(sel, 2, 10), Some((0, 9)));
        assert_eq!(selected_cols(sel, 3, 10), Some((0, 2)));
        assert_eq!(selected_cols(sel, 4, 10), None);
    }
}
