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
use iced::widget::text::Span;
use iced::widget::{rich_text, span};
use iced::{font, Event, Subscription};
use std::sync::{mpsc, OnceLock};

const FONT_SIZE: f32 = 14.0;
const LINE_H: f32 = 18.0;
const CELL_W: f32 = 8.43; // Menlo: 0,602 em
const SIDEBAR_W: f32 = 260.0;

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

fn load_screen(id: u32) -> Task<Message> {
    Task::perform(async move {
        tokio::task::spawn_blocking(move || {
            serde_json::from_value(request(Request::Snapshot { pane: id.to_string() })?).map_err(|e| e.to_string())
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

/// Filas y columnas que caben en el área del terminal para el tamaño de ventana actual.
fn grid_for(window: (f32, f32)) -> (u16, u16) {
    let w = window.0 - SIDEBAR_W - 24.0 - 20.0;
    let h = window.1 - 24.0 - 44.0 - 20.0 - 10.0;
    (((h / LINE_H).floor().max(5.0)) as u16, ((w / CELL_W).floor().max(20.0)) as u16)
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
            Some(id) if !app.loading => { app.loading = true; load_screen(id) }
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
                        app.selected = None; app.snap = None;
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
                    Ok(snap) => { app.error = None; if app.snap.as_ref() != Some(&snap) { app.snap = Some(snap); } }
                    Err(e) => app.error = Some(e),
                }
            }
            Task::none()
        }
        Message::Select(id) => {
            app.selected = Some(id); app.snap = None; fit_size(app);
            if app.loading { Task::none() } else { app.loading = true; load_screen(id) }
        }
        Message::Event(Event::Window(iced::window::Event::Resized(size))) => {
            app.window = (size.width, size.height); fit_size(app); Task::none()
        }
        Message::Event(Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, text, .. })) => {
            if modifiers.command() && matches!(&key, Key::Character(c) if c.as_str() == "v") {
                return iced::clipboard::read().map(Message::Paste);
            }
            if let Some(bytes) = term::encode(&key, modifiers, text.as_deref()) { send_text(app, bytes); }
            Task::none()
        }
        Message::Event(Event::Window(iced::window::Event::Opened { size, .. })) => {
            app.window = (size.width, size.height); fit_size(app); Task::none()
        }
        Message::Event(_) => Task::none(),
        Message::Paste(Some(t)) => { send_text(app, t.replace("\r\n", "\r").replace('\n', "\r")); Task::none() }
        Message::Paste(None) => Task::none(),
    }
}

fn spans_for_line(runs: &[Run], cursor_col: Option<u16>) -> Vec<Span<'static, Message>> {
    let mut out = Vec::new();
    let mut col: u16 = 0;
    let mut push = |text: String, r: &Run, invert: bool| {
        let (mut fg, mut bg) = (term::color(r.fg), term::color(r.bg));
        if r.inverse ^ invert {
            let (f, b) = (bg.unwrap_or(term::DEFAULT_BG), fg.unwrap_or(term::DEFAULT_FG));
            fg = Some(f); bg = Some(b);
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
        out.push(sp);
    };
    for r in runs {
        let n = r.t.chars().count() as u16;
        match cursor_col {
            Some(c) if c >= col && c < col + n => {
                let k = (c - col) as usize;
                let before: String = r.t.chars().take(k).collect();
                let at: String = r.t.chars().skip(k).take(1).collect();
                let after: String = r.t.chars().skip(k + 1).collect();
                if !before.is_empty() { push(before, r, false); }
                push(at, r, true);
                if !after.is_empty() { push(after, r, false); }
            }
            _ => push(r.t.clone(), r, false),
        }
        col += n;
    }
    if out.is_empty() { out.push(span(" ".to_string())); }
    out
}

fn screen_view(snap: &Snapshot) -> Element<'static, Message> {
    let mut lines = column![].spacing(0);
    for (r, runs) in snap.lines.iter().enumerate() {
        let cursor = (!snap.cursor_hidden && snap.cursor.0 as usize == r).then_some(snap.cursor.1);
        let spans = spans_for_line(runs, cursor);
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
        Some(snap) => screen_view(snap),
        None => text(if app.selected.is_none() { "Crea un panel con `galgo spawn`." } else { "Esperando salida…" }).size(14).color(MUTED).into(),
    };
    let terminal = container(screen).width(Length::Fill).height(Length::Fill).padding(10).clip(true)
        .style(|_| container::Style { background: Some(term::DEFAULT_BG.into()), border: iced::border::rounded(8), ..Default::default() });
    let mut body = column![
        row![text(title).size(18), text(state).size(11).color(state_color), Space::with_width(Length::Fill), text("GALGO").size(11).color(MUTED)].spacing(12).align_y(iced::Alignment::Center).height(44),
        terminal,
    ].spacing(10).padding(12).width(Length::Fill).height(Length::Fill);
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
                _ => None,
            }),
        ]))
        .window_size(iced::Size::new(1100.0, 700.0))
        .run_with(|| (App { window: (1100.0, 700.0), ..Default::default() }, load_panes()))
}
