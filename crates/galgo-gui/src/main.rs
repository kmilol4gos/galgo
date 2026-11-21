use galgo_proto::{client, PaneInfo, Request, Status};
use iced::{widget::{button, column, container, row, scrollable, text, text_input, Space}, Color, Element, Font, Length, Task, Theme};
use std::time::Duration;

const BG: Color = Color::from_rgb(0.059, 0.067, 0.082);
const SURFACE: Color = Color::from_rgb(0.083, 0.094, 0.113);
const TEXT: Color = Color::from_rgb(0.906, 0.914, 0.937);
const MUTED: Color = Color::from_rgb(0.60, 0.64, 0.70);

#[derive(Default)]
struct App {
    panes: Vec<PaneInfo>,
    selected: Option<u32>,
    screen: String,
    input: String,
    error: Option<String>,
    loading: bool,
    sending: bool,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    Loaded(Result<(Vec<PaneInfo>, Option<(u32, String)>), String>),
    Select(u32),
    Input(String),
    Send,
    Sent(String, Result<(), String>),
}

fn request(req: Request) -> Result<serde_json::Value, String> {
    let response = client::request(&req).map_err(|e| format!("{e:#}"))?;
    if response.ok { Ok(response.data) } else { Err(response.error.unwrap_or_else(|| "Error del daemon".into())) }
}

fn load(selected: Option<u32>) -> Task<Message> {
    Task::perform(async move {
        tokio::task::spawn_blocking(move || {
            let panes: Vec<PaneInfo> = serde_json::from_value(request(Request::List)?).map_err(|e| e.to_string())?;
            let screen = match selected.filter(|id| panes.iter().any(|p| p.id == *id)) {
                Some(id) => Some((id, request(Request::Read { pane: id.to_string() })?["text"].as_str().unwrap_or("").to_owned())),
                None => None,
            };
            Ok((panes, screen))
        }).await.map_err(|e| e.to_string())?
    }, Message::Loaded)
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Tick if !app.loading => { app.loading = true; load(app.selected) }
        Message::Tick => Task::none(),
        Message::Loaded(result) => {
            app.loading = false;
            match result {
                Ok((panes, screen)) => {
                    app.error = None;
                    app.panes = panes;
                    if app.selected.is_some_and(|id| !app.panes.iter().any(|p| p.id == id)) {
                        app.selected = None; app.screen.clear();
                    }
                    if let Some((id, contents)) = screen { if app.selected == Some(id) { app.screen = contents; } }
                }
                Err(e) => app.error = Some(e),
            }
            Task::none()
        }
        Message::Select(id) => { app.selected = Some(id); app.screen.clear(); if !app.loading { app.loading = true; load(app.selected) } else { Task::none() } }
        Message::Input(value) => { app.input = value; Task::none() }
        Message::Send if !app.sending && app.selected.is_some() && !app.input.is_empty() => {
            let pane = app.selected.unwrap().to_string();
            let input = std::mem::take(&mut app.input);
            app.sending = true;
            Task::perform(async move {
                let retry = input.clone();
                let result = tokio::task::spawn_blocking(move || request(Request::Send { pane, text: input, enter: true }).map(|_| ()))
                    .await.map_err(|e| e.to_string()).and_then(|r| r);
                (retry, result)
            }, |(input, result)| Message::Sent(input, result))
        }
        Message::Send => Task::none(),
        Message::Sent(input, result) => { app.sending = false; if let Err(e) = result { if app.input.is_empty() { app.input = input; } app.error = Some(e); } Task::none() }
    }
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
    let title = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id)).map(|p| p.name.as_str()).unwrap_or("Selecciona un panel");
    let contents = if app.selected.is_none() { "Elige un panel en la barra lateral para ver su pantalla." } else if app.screen.is_empty() { "Esperando salida…" } else { &app.screen };
    let terminal = container(scrollable(text(contents).font(Font::MONOSPACE).size(14).color(TEXT)).height(Length::Fill)).width(Length::Fill).height(Length::Fill).padding(18).style(|_| container::Style { background: Some(Color::from_rgb(0.04, 0.045, 0.055).into()), border: iced::border::rounded(8), ..Default::default() });
    let mut send = button(text(if app.sending { "Enviando…" } else { "Enviar ↵" }).size(14)).padding([10, 18]);
    if app.selected.is_some() && !app.sending && !app.input.is_empty() { send = send.on_press(Message::Send); }
    let mut body = column![row![column![text(title).size(20), text("PANTALLA DEL PANEL").size(11).color(MUTED)].spacing(5), Space::with_width(Length::Fill), text("GALGO / SPIKE").size(11).color(MUTED)], terminal, text("Enviar texto al panel").size(12).color(MUTED), row![text_input("Escribe un comando o mensaje…", &app.input).on_input(Message::Input).on_submit(Message::Send).padding(12).width(Length::Fill), send].spacing(10)].spacing(14).padding(22).width(Length::Fill).height(Length::Fill);
    if let Some(error) = &app.error { body = body.push(text(format!("Error: {error}")).size(13).color(status_color(Status::Failed))); }
    container(row![sidebar, body]).width(Length::Fill).height(Length::Fill).style(|_| container::Style { background: Some(BG.into()), text_color: Some(TEXT), ..Default::default() }).into()
}

fn main() -> iced::Result {
    iced::application("galgo", update, view)
        .theme(|_| Theme::Dark)
        .subscription(|_: &App| iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick))
        .window_size(iced::Size::new(1100.0, 700.0))
        .run_with(|| (App { loading: true, ..Default::default() }, load(None)))
}
