//! Marco de la ventana: barra lateral, barra de pestañas, barra de estado, bienvenida y capas flotantes (paleta,
//! lanzador, nuevo workspace, formulario). El terminal ocupa todo lo demás, sin márgenes de adorno.
//!
//! Geometría (ver `terminal_area`): barra lateral de `SIDEBAR_W` a la izquierda, pestañas de `TABS_H` arriba y estado
//! de `STATUS_H` abajo. Nada que aparezca o desaparezca (errores, uso, confirmaciones) cambia esa geometría: todo eso
//! vive en la barra de estado, así el terminal no salta de tamaño.

use super::*;
use iced::widget::{button, column, container, lazy, mouse_area, opaque, pick_list, progress_bar, row, scrollable, stack, text, text_input, tooltip, Space};
use iced::{Alignment, Background, Border};

/// Punto de estado dibujado (no un glifo de fuente): un círculo de `size` px.
pub fn dot<'a>(color: Color, size: f32) -> Element<'a, Message> {
    container(Space::new(Length::Fixed(size), Length::Fixed(size)))
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: (size / 2.0).into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// Icono de split dibujado con cajas: un rectángulo con su divisor vertical (`vertical`) u horizontal.
fn split_icon<'a>(vertical: bool) -> Element<'a, Message> {
    let line = container(Space::new(
        if vertical { Length::Fixed(1.0) } else { Length::Fill },
        if vertical { Length::Fill } else { Length::Fixed(1.0) },
    ))
    .style(theme::surface(MUTED));
    let inner: Element<'a, Message> = if vertical {
        row![Space::with_width(Length::Fill), line, Space::with_width(Length::Fill)].into()
    } else {
        column![Space::with_height(Length::Fill), line, Space::with_height(Length::Fill)].into()
    };
    container(inner)
        .width(Length::Fixed(15.0))
        .height(Length::Fixed(12.0))
        .style(|_| container::Style { border: Border { color: MUTED, width: 1.0, radius: 2.0.into() }, ..Default::default() })
        .into()
}

/// Botón de icono de la barra de pestañas, con su explicación al pasar el puntero.
fn icon_button<'a>(icon: Element<'a, Message>, tip: &'a str, message: Message) -> Element<'a, Message> {
    let b = theme::btn(container(icon).center_x(Length::Fixed(28.0)).center_y(Length::Fixed(26.0))).padding(0).on_press(message);
    tooltip(b, container(text(tip).size(theme::SMALL_SIZE)).padding([4, 8]).style(theme::overlay), tooltip::Position::Bottom).into()
}

/// Ruta con `~` en lugar del directorio de inicio.
pub fn tilde(path: &str) -> String {
    let home = home_dir();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if home.len() > 1 && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// (carpeta, padre) de una ruta, para mostrar el nombre destacado y el resto en gris.
fn split_path(path: &str) -> (String, String) {
    let p = std::path::Path::new(path);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
    let parent = p.parent().map(|d| tilde(&d.to_string_lossy())).unwrap_or_default();
    (name, parent)
}

// ---------------------------------------------------------------------------------------------------------------
// Barra lateral
// ---------------------------------------------------------------------------------------------------------------

/// Etiqueta corta de la insignia de atención, para la columna derecha de una fila.
fn badge_short(b: Badge) -> (&'static str, Color) {
    match b {
        Badge::Blocked => ("decisión", theme::BLOCKED),
        Badge::DoneUnread => ("sin leer", theme::DONE),
        Badge::Orphan => ("sin confirmar", theme::FAILED),
    }
}

/// Fila de un panel: punto de estado, nombre y, a la derecha, lo más útil (atención, uso o estado). Las relaciones
/// (manager, espera, pendientes) van en una segunda línea solo si existen.
fn pane_row<'a>(app: &'a App, pane: &'a PaneInfo, indent: f32) -> Element<'a, Message> {
    let selected = app.selected == Some(pane.id);
    let right: Element<'a, Message> = if let Some(b) = attention::badge(pane, app.attention.is_unread(pane.id)) {
        let (label, color) = badge_short(b);
        text(label).size(theme::SMALL_SIZE).color(color).into()
    } else if let Some((line, color)) = pane.usage.as_ref().and_then(usage::pane_line) {
        text(line).size(theme::SMALL_SIZE).color(color.unwrap_or(theme::DIM)).into()
    } else if pane.status != Status::Idle {
        text(pane.status.as_str()).size(theme::SMALL_SIZE).color(theme::DIM).into()
    } else {
        Space::with_width(0).into()
    };
    let name_color = if selected { TEXT } else { theme::TEXT_SOFT };
    let line = row![
        dot(theme::status_color(pane.status), 7.0),
        text(&pane.name).size(theme::BODY_SIZE).color(name_color).wrapping(iced::widget::text::Wrapping::None),
        Space::with_width(Length::Fill),
        right,
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let mut notes = Vec::new();
    if pane.orphan {
        notes.push("entrega sin confirmar".to_string());
    } else if pane.unacked > 0 {
        notes.push(format!("{} pendientes", pane.unacked));
    }
    if let Some(manager) = &pane.manager {
        notes.push(format!("→ {manager}"));
    }
    if let Some(waiting) = &pane.waiting_on {
        notes.push(format!("espera a {waiting}"));
    }
    let mut content = column![line].spacing(2);
    if !notes.is_empty() {
        content = content.push(row![Space::with_width(15), text(notes.join(" · ")).size(10).color(theme::DIM)]);
    }
    theme::btn(container(content).width(Length::Fill).padding(iced::Padding { top: 5.0, bottom: 5.0, left: 8.0 + indent, right: 8.0 }).clip(true))
        .padding(0)
        .width(Length::Fill)
        .on_press(Message::Select(pane.id))
        .style(theme::row(selected))
        .into()
}

/// Recuento compacto por estado: un punto de color y el número, solo de los estados presentes.
fn status_counts<'a>(panes: &[&PaneInfo]) -> Element<'a, Message> {
    let mut r = row![].spacing(8).align_y(Alignment::Center);
    for s in [Status::Working, Status::Blocked, Status::Done, Status::Failed] {
        let n = panes.iter().filter(|p| p.status == s || (s == Status::Failed && p.status == Status::Exited)).count();
        if n > 0 {
            r = r.push(row![dot(theme::status_color(s), 6.0), text(n.to_string()).size(theme::SMALL_SIZE).color(theme::DIM)].spacing(4).align_y(Alignment::Center));
        }
    }
    r.into()
}

/// Cabecera de sección de la barra: mayúsculas pequeñas y apagadas, con un contador opcional a la derecha.
fn section<'a>(label: String, color: Color, count: Option<usize>) -> Element<'a, Message> {
    let mut r = row![text(label).size(10).color(color)].align_y(Alignment::Center);
    if let Some(n) = count {
        r = r.push(Space::with_width(Length::Fill)).push(text(n.to_string()).size(10).color(color));
    }
    container(r).padding(iced::Padding { top: 12.0, bottom: 4.0, left: 8.0, right: 8.0 }).into()
}

fn sidebar_list(app: &App) -> Element<'_, Message> {
    let mut list = column![].spacing(1);
    if app.panes.is_empty() {
        list = list.push(container(text("Sin paneles. Abre una carpeta con ⌘N.").size(theme::SMALL_SIZE).color(theme::DIM)).padding([12, 8]));
    }
    let needs = sidebar::needs_attention(&app.panes, |id| app.attention.is_unread(id));
    if !needs.is_empty() {
        list = list.push(section("TE NECESITAN".into(), theme::BLOCKED, Some(needs.len())));
        for pane in needs {
            list = list.push(pane_row(app, pane, 0.0));
        }
    }
    for project in sidebar::tree(&app.panes) {
        list = list.push(section(project.name.to_uppercase(), theme::DIM, None));
        for ws in project.workspaces {
            let key = ws.label.clone().unwrap_or_default();
            let folded = app.collapsed.contains(&key);
            let arrow = text(if folded { "▸" } else { "▾" }).size(12).color(MUTED).width(10);
            let mut right = row![status_counts(&ws.panes)].spacing(8).align_y(Alignment::Center);
            if let Some(total) = usage::group_total(&ws.panes) {
                right = right.push(text(total).size(theme::SMALL_SIZE).color(theme::DIM));
            }
            let current = focus_key(app).is_some_and(|k| k == key);
            let head = row![
                arrow,
                text(ws.name.clone()).size(theme::BODY_SIZE).color(if current { TEXT } else { theme::TEXT_SOFT }).wrapping(iced::widget::text::Wrapping::None),
                Space::with_width(Length::Fill),
                right,
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            list = list.push(
                theme::btn(container(head).width(Length::Fill).padding([5, 8]).clip(true))
                    .padding(0)
                    .width(Length::Fill)
                    .on_press(Message::ToggleGroup(key))
                    .style(theme::row(false)),
            );
            if !folded {
                for pane in ws.panes {
                    list = list.push(pane_row(app, pane, 16.0));
                }
            }
        }
    }
    container(list).padding([0, 6]).into()
}

/// Pie de la barra lateral: cuotas por agente, compactas (etiqueta, barra fina, porcentaje y reinicio).
fn limits_footer(app: &App) -> Element<'_, Message> {
    let now = now_secs();
    let mut col = column![].spacing(4);
    if app.limits.is_empty() {
        let msg = match &app.limits_error {
            Some(e) => format!("cuotas no disponibles: {e}"),
            None => "sin cuotas observadas".to_string(),
        };
        col = col.push(text(msg).size(10).color(theme::DIM));
    }
    for l in &app.limits {
        col = col.push(text(l.agent.clone()).size(theme::SMALL_SIZE).color(theme::TEXT_SOFT));
        for r in usage::limit_rows(l, now) {
            let color = if r.pct >= 90.0 { theme::FAILED } else if r.pct >= 70.0 { theme::BLOCKED } else { theme::ACCENT };
            let bar = row![
                text(r.label).size(10).color(theme::DIM).width(26),
                progress_bar(0.0..=100.0, r.pct).height(4).style(move |_| progress_bar::Style {
                    background: Background::Color(theme::BORDER),
                    bar: Background::Color(color),
                    border: Border { radius: 2.0.into(), ..Default::default() },
                }),
                text(format!("{:.0}%", r.pct)).size(10).color(theme::MUTED).width(30).align_x(iced::alignment::Horizontal::Right),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            col = col.push(bar).push(text(r.detail).size(10).color(theme::DIM));
        }
    }
    column![theme::rule(false), container(col).padding([10, 14])].into()
}

pub fn sidebar_view(app: &App) -> Element<'_, Message> {
    let header = row![
        text("galgo").size(15).font(Font { weight: font::Weight::Bold, ..terminal_font() }).color(TEXT),
        Space::with_width(Length::Fill),
        icon_button(text("+").size(16).color(MUTED).into(), "Nuevo workspace  ⌘N", Message::OpenNewWorkspace),
    ]
    .align_y(Alignment::Center)
    .padding(iced::Padding { top: 0.0, bottom: 0.0, left: 14.0, right: 6.0 })
    .height(TABS_H - 1.0);
    let search = theme::btn(
        row![text("Buscar o ejecutar…").size(theme::SMALL_SIZE + 1.0).color(theme::DIM), Space::with_width(Length::Fill), theme::keycap("Cmd+K")]
            .align_y(Alignment::Center),
    )
    .padding([5, 8])
    .width(Length::Fill)
    .on_press(Message::OpenPalette)
    .style(|_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(Background::Color(if hover { theme::HOVER } else { theme::TERM_BG })),
            text_color: TEXT,
            border: Border { color: theme::BORDER, width: 1.0, radius: theme::RADIUS.into() },
            ..Default::default()
        }
    });
    let body = column![
        header,
        theme::rule(false),
        container(search).padding(iced::Padding { top: 10.0, bottom: 2.0, left: 10.0, right: 10.0 }),
        scrollable(sidebar_list(app)).height(Length::Fill),
        limits_footer(app),
    ];
    row![
        container(body).width(Length::Fixed(SIDEBAR_W - 1.0)).height(Length::Fill).style(theme::surface(SIDEBAR)),
        theme::rule(true),
    ]
    .into()
}

// ---------------------------------------------------------------------------------------------------------------
// Pestañas y paneles
// ---------------------------------------------------------------------------------------------------------------

/// Barra superior del área de terminales: dónde estás (proyecto / rama), las pestañas del workspace y las acciones.
pub fn tabs_bar(app: &App) -> Element<'_, Message> {
    let key = focus_key(app);
    let (project, branch) = sidebar::split_label(key.as_deref().filter(|k| !k.is_empty()));
    let crumb = row![text(project).size(theme::SMALL_SIZE + 1.0).color(theme::DIM), text("/").size(theme::SMALL_SIZE + 1.0).color(theme::DIM), text(branch).size(theme::SMALL_SIZE + 1.0).color(MUTED)]
        .spacing(4)
        .align_y(Alignment::Center);
    let mut bar = row![container(crumb).padding([0, 14])].align_y(Alignment::Center).height(TABS_H - 1.0);
    if let Some(ws) = key.as_ref().and_then(|k| app.layouts.by_key.get(k)) {
        for (i, tab) in ws.tabs.iter().enumerate() {
            let pane = app.panes.iter().find(|p| p.id == tab.focus);
            let name = pane.map_or("?", |p| p.name.as_str());
            let color = pane.map_or(theme::IDLE, |p| theme::status_color(p.status));
            let active = i == ws.active;
            let n = tab.root.ids().len();
            let mut label = row![dot(color, 6.0), text(name).size(theme::BODY_SIZE)].spacing(7).align_y(Alignment::Center);
            if n > 1 {
                label = label.push(text(format!("{n}")).size(10).color(theme::DIM));
            }
            if active {
                // Cerrar el panel enfocado de la pestaña (con confirmación en la barra de estado si sigue vivo).
                label = label.push(theme::btn(text("×").size(13).color(theme::DIM)).padding([0, 4]).on_press(Message::AskClose));
            }
            let top = container(Space::new(Length::Fill, Length::Fixed(2.0))).style(theme::tab_underline(active));
            let b = theme::btn(container(label).padding([0, 14]).center_y(Length::Fixed(TABS_H - 3.0)))
                .padding(0)
                .on_press(Message::SelectTab(i))
                .style(theme::tab(active));
            bar = bar.push(row![theme::rule(true), column![top, b].width(Length::Shrink)].height(TABS_H - 1.0));
        }
        bar = bar.push(theme::rule(true));
    }
    bar = bar
        .push(icon_button(text("+").size(16).color(MUTED).into(), "Nueva pestaña  ⌘T", Message::NewTab))
        .push(Space::with_width(Length::Fill))
        .push(
            tooltip(
                theme::btn(text("Agente").size(theme::SMALL_SIZE + 1.0).color(MUTED)).padding([4, 10]).on_press(Message::OpenLauncher),
                container(text("Lanzar agente  ⌘⇧L").size(theme::SMALL_SIZE)).padding([4, 8]).style(theme::overlay),
                tooltip::Position::Bottom,
            ),
        )
        .push(icon_button(split_icon(true), "Dividir a la derecha  ⌘D", Message::SplitFocused(true)))
        .push(icon_button(split_icon(false), "Dividir hacia abajo  ⌘⇧D", Message::SplitFocused(false)))
        .push(Space::with_width(6));
    column![container(bar).width(Length::Fill).style(theme::surface(SIDEBAR)), theme::rule(false)].into()
}

/// El área de paneles: la pestaña visible del workspace enfocado, con sus splits.
pub fn layout_view(app: &App) -> Element<'_, Message> {
    match focus_key(app).and_then(|k| app.layouts.active_tab(&k)) {
        Some(tab) => {
            let split = matches!(tab.root, layout::Node::Split { .. });
            node_view(app, &tab.root, split)
        }
        None => placeholder("Esperando salida…"),
    }
}

fn placeholder<'a>(msg: &'a str) -> Element<'a, Message> {
    container(text(msg).size(theme::BODY_SIZE).color(theme::DIM)).center(Length::Fill).into()
}

/// Un nodo del layout: una hoja es un panel; un split reparte su área en dos mitades, con un divisor de 1 px.
fn node_view<'a>(app: &'a App, node: &'a layout::Node, split: bool) -> Element<'a, Message> {
    match node {
        layout::Node::Leaf(id) => leaf_view(app, *id, split),
        layout::Node::Split { vertical, a, b } => {
            let first = container(node_view(app, a, split)).width(Length::FillPortion(1)).height(Length::FillPortion(1));
            let second = container(node_view(app, b, split)).width(Length::FillPortion(1)).height(Length::FillPortion(1));
            if *vertical {
                row![first, theme::rule(true), second].width(Length::Fill).height(Length::Fill).into()
            } else {
                column![first, theme::rule(false), second].width(Length::Fill).height(Length::Fill).into()
            }
        }
    }
}

/// Un panel: el enfocado con su vista congelada y su cursor; los demás, con la última pantalla que llegó, sin cursor
/// y atenuados (como Ghostty), para que se vea dónde va el teclado. En un split, el enfocado lleva un borde de acento.
fn leaf_view(app: &App, id: u32, split: bool) -> Element<'_, Message> {
    let focused = app.selected == Some(id);
    let inner: Element<'_, Message> = if focused {
        match shown(app) {
            // Con `lazy`, el árbol de la pantalla solo se reconstruye cuando cambia lo que se muestra.
            Some(snap) => {
                let key = (app.selected, app.snap_gen, app.snap_offset, app.sel, app.sel_snap.as_ref().map(|s| s.gen), app.focused, zoom());
                let sel = app.sel;
                let cursor = app.focused;
                lazy(key, move |_| screen_view(snap, sel, cursor)).into()
            }
            None => placeholder("Esperando salida…"),
        }
    } else {
        match app.others.get(&id) {
            Some(snap) => screen_view(snap, None, false),
            None => placeholder("Esperando salida…"),
        }
    };
    let body = container(inner).width(Length::Fill).height(Length::Fill).padding(LEAF_PAD).clip(true);
    let framed = container(body).width(Length::Fill).height(Length::Fill).style(move |_| container::Style {
        background: Some(theme::TERM_BG.into()),
        border: Border { color: if split && focused { Color { a: 0.55, ..theme::ACCENT } } else { Color::TRANSPARENT }, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    });
    if split && !focused {
        let veil = container(Space::new(Length::Fill, Length::Fill)).style(theme::surface(Color { a: 0.28, ..theme::TERM_BG }));
        stack![framed, veil].into()
    } else {
        framed.into()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Barra de estado
// ---------------------------------------------------------------------------------------------------------------

fn sep<'a>() -> Element<'a, Message> {
    text("·").size(theme::SMALL_SIZE).color(theme::DIM).into()
}

/// Barra inferior: el panel enfocado (estado, carpeta, tamaño), y a la derecha lo pasajero: confirmación de cierre,
/// error, historial, vista congelada y uso. Tiene altura fija: nada de esto mueve el terminal.
pub fn status_bar(app: &App) -> Element<'_, Message> {
    let pane = app.selected.and_then(|id| app.panes.iter().find(|p| p.id == id));
    let mut left = row![].spacing(8).align_y(Alignment::Center);
    if let Some(p) = pane {
        left = left
            .push(dot(theme::status_color(p.status), 6.0))
            .push(text(&p.name).size(theme::SMALL_SIZE).color(theme::TEXT_SOFT))
            .push(text(p.status.as_str()).size(theme::SMALL_SIZE).color(theme::status_color(p.status)));
        if let Some(cwd) = &p.cwd {
            left = left.push(sep()).push(text(tilde(cwd)).size(theme::SMALL_SIZE).color(theme::DIM).wrapping(iced::widget::text::Wrapping::None));
        }
        if let Some(s) = shown(app) {
            left = left.push(sep()).push(text(format!("{}×{}", s.cols, s.rows)).size(theme::SMALL_SIZE).color(theme::DIM));
        }
    }
    let mut right = row![].spacing(8).align_y(Alignment::Center);
    if app.confirm_close && pane.is_some() {
        let name = pane.map_or("", |p| p.name.as_str());
        right = right
            .push(text(format!("¿Cerrar «{name}»? El proceso termina.")).size(theme::SMALL_SIZE).color(theme::BLOCKED))
            .push(theme::btn(text("Cerrar").size(theme::SMALL_SIZE).color(theme::FAILED)).padding([1, 8]).on_press(Message::ConfirmClose))
            .push(theme::btn(text("Cancelar").size(theme::SMALL_SIZE).color(MUTED)).padding([1, 8]).on_press(Message::CancelClose));
    } else {
        if let Some(e) = &app.error {
            right = right.push(text(format!("⚠ {e}")).size(theme::SMALL_SIZE).color(theme::FAILED).wrapping(iced::widget::text::Wrapping::None));
        }
        if let Some(n) = shown(app).map(|s| s.scrollback).filter(|&n| n > 0) {
            right = right.push(text(format!("↑ {n} líneas")).size(theme::SMALL_SIZE).color(theme::BLOCKED));
        }
        if app.sel_snap.is_some() {
            right = right.push(text("vista congelada · esc").size(theme::SMALL_SIZE).color(MUTED));
        }
        if let Some(u) = pane.and_then(|p| p.usage.as_ref()) {
            right = right.push(text(usage::detail_line(u)).size(theme::SMALL_SIZE).color(theme::DIM).wrapping(iced::widget::text::Wrapping::None));
        }
    }
    let bar = row![container(left).clip(true), Space::with_width(Length::Fill), right]
        .spacing(12)
        .align_y(Alignment::Center)
        .padding([0, 12])
        .height(STATUS_H - 1.0);
    column![theme::rule(false), container(bar).width(Length::Fill).style(theme::surface(SIDEBAR))].into()
}

// ---------------------------------------------------------------------------------------------------------------
// Bienvenida
// ---------------------------------------------------------------------------------------------------------------

/// Fila de acción de la bienvenida: texto a la izquierda y su atajo a la derecha.
fn action_row<'a>(label: &'a str, keys: Option<&'a str>, message: Message, primary: bool) -> Element<'a, Message> {
    let mut r = row![text(label).size(theme::BODY_SIZE).color(if primary { TEXT } else { theme::TEXT_SOFT })].align_y(Alignment::Center);
    r = r.push(Space::with_width(Length::Fill));
    if let Some(k) = keys {
        r = r.push(theme::keycap(k));
    }
    theme::btn(r).padding([9, 12]).width(Length::Fill).on_press(message).style(theme::row(false)).into()
}

pub fn welcome_view(app: &App) -> Element<'_, Message> {
    let mut col = column![
        text("galgo").size(30).font(Font { weight: font::Weight::Bold, ..terminal_font() }).color(TEXT),
        text("Terminales y agentes, ordenados por proyecto y rama.").size(theme::BODY_SIZE).color(MUTED),
        Space::with_height(18),
        action_row("Abrir carpeta…", Some("Cmd+O"), Message::PickFolder, true),
        action_row("Nuevo workspace desde recientes", Some("Cmd+N"), Message::OpenNewWorkspace, false),
        action_row("Terminal en ~", None, Message::OpenFolder(home_dir()), false),
        action_row("Paleta de comandos", Some("Cmd+K"), Message::OpenPalette, false),
    ]
    .spacing(2)
    .max_width(460);
    if !app.recents.is_empty() {
        col = col.push(Space::with_height(14)).push(container(text("RECIENTES").size(10).color(theme::DIM)).padding([4, 12]));
        for p in app.recents.iter().take(6) {
            let (name, parent) = split_path(p);
            let r = row![text(name).size(theme::BODY_SIZE).color(theme::TEXT_SOFT), text(parent).size(theme::SMALL_SIZE).color(theme::DIM).wrapping(iced::widget::text::Wrapping::None)]
                .spacing(10)
                .align_y(Alignment::Center);
            col = col.push(theme::btn(container(r).clip(true)).padding([7, 12]).width(Length::Fill).on_press(Message::OpenFolder(p.clone())).style(theme::row(false)));
        }
    }
    container(col).center(Length::Fill).style(theme::surface(theme::TERM_BG)).into()
}

// ---------------------------------------------------------------------------------------------------------------
// Capas flotantes
// ---------------------------------------------------------------------------------------------------------------

/// Pone `panel` flotando sobre `base`, arriba y centrado, con un velo que cierra la capa al hacer clic.
pub fn floating<'a>(base: Element<'a, Message>, panel: Element<'a, Message>, width: f32, dismiss: Message) -> Element<'a, Message> {
    let scrim = mouse_area(container(Space::new(Length::Fill, Length::Fill)).style(theme::surface(theme::SCRIM))).on_press(dismiss);
    let boxed = container(panel).width(Length::Fixed(width)).style(theme::overlay);
    let placed = container(column![Space::with_height(Length::FillPortion(1)), opaque(boxed), Space::with_height(Length::FillPortion(4))])
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center);
    stack![base, scrim, placed].into()
}

/// Pie de una capa: las teclas que la manejan.
fn hints<'a>(items: &[(&'a str, &'a str)]) -> Element<'a, Message> {
    let mut r = row![].spacing(14).align_y(Alignment::Center);
    for (k, label) in items {
        r = r.push(row![theme::keycap(k), text(*label).size(theme::SMALL_SIZE).color(theme::DIM)].spacing(6).align_y(Alignment::Center));
    }
    container(r).padding([8, 14]).into()
}

pub fn palette_panel(app: &App) -> Element<'_, Message> {
    let query = app.palette.as_deref().unwrap_or("");
    let results = palette_results(app);
    let mut list = column![].spacing(1);
    for (i, item) in results.iter().enumerate().take(PALETTE_VISIBLE) {
        let cursor = i == app.palette_cursor;
        let (kind, name) = match item.label.split_once(" · ") {
            Some((k, n)) => (k.to_string(), n.to_string()),
            None => ("Acción".to_string(), item.label.clone()),
        };
        let right: Element<'_, Message> = if item.detail.starts_with("Cmd") {
            theme::keycap(&item.detail)
        } else {
            text(item.detail.clone()).size(theme::SMALL_SIZE).color(theme::DIM).into()
        };
        let line = row![
            text(kind).size(theme::SMALL_SIZE).color(if cursor { theme::ACCENT } else { theme::DIM }).width(76),
            text(name).size(theme::BODY_SIZE).color(if cursor { TEXT } else { theme::TEXT_SOFT }),
            Space::with_width(Length::Fill),
            right,
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        list = list.push(theme::btn(line).width(Length::Fill).padding([7, 12]).on_press(Message::PaletteRun(i)).style(theme::row(cursor)));
    }
    if results.is_empty() {
        list = list.push(container(text("Sin resultados.").size(theme::SMALL_SIZE).color(theme::DIM)).padding([10, 12]));
    }
    column![
        text_input("Buscar workspace, panel o acción…", query)
            .id(palette_input_id())
            .on_input(Message::PaletteQuery)
            .on_submit(Message::PaletteRun(app.palette_cursor))
            .size(15)
            .padding([14, 16])
            .style(theme::search_input),
        theme::rule(false),
        container(scrollable(list).height(Length::Shrink)).padding(6).max_height(440),
        theme::rule(false),
        hints(&[("↑↓", "mover"), ("Enter", "ejecutar"), ("Esc", "cerrar")]),
    ]
    .into()
}

pub fn launcher_panel(app: &App) -> Element<'_, Message> {
    let (workspace, cwd) = focus_context(app);
    let target = format!("{} · {}", workspace.as_deref().unwrap_or("sin workspace"), cwd.as_deref().map(tilde).unwrap_or_else(|| "~".into()));
    let mut list = column![].spacing(1);
    for (i, agent) in launch::Agent::ALL.iter().enumerate() {
        let desc = match agent {
            launch::Agent::Claude => "Claude Code",
            launch::Agent::Codex => "OpenAI Codex",
            launch::Agent::Pi => "PI",
            launch::Agent::OpenCode => "opencode",
            launch::Agent::Shell => "tu shell",
        };
        let line = row![
            theme::keycap(&(i + 1).to_string()),
            text(agent.label()).size(theme::BODY_SIZE).color(TEXT),
            text(desc).size(theme::SMALL_SIZE).color(theme::DIM),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        list = list.push(theme::btn(line).width(Length::Fill).padding([8, 12]).on_press(Message::Launch(*agent)).style(theme::row(false)));
    }
    column![
        container(column![text("Lanzar agente").size(theme::TITLE_SIZE).color(TEXT), text(target).size(theme::SMALL_SIZE).color(theme::DIM)].spacing(4)).padding([14, 16]),
        theme::rule(false),
        container(list).padding(6),
        theme::rule(false),
        hints(&[("1-5", "lanzar"), ("Esc", "cerrar")]),
    ]
    .into()
}

pub fn new_workspace_panel(app: &App) -> Element<'_, Message> {
    let query = app.new_ws.as_deref().unwrap_or("");
    let matches = recents::matching(&app.recents, query);
    let mut list = column![].spacing(1);
    for (i, p) in matches.iter().take(10).enumerate() {
        let (name, parent) = split_path(p);
        let r = row![text(name).size(theme::BODY_SIZE).color(TEXT), text(parent).size(theme::SMALL_SIZE).color(theme::DIM).wrapping(iced::widget::text::Wrapping::None)]
            .spacing(10)
            .align_y(Alignment::Center);
        list = list.push(theme::btn(container(r).clip(true)).width(Length::Fill).padding([7, 12]).on_press(Message::OpenFolder((*p).clone())).style(theme::row(i == 0)));
    }
    if matches.is_empty() {
        let msg = if app.recents.is_empty() { "Aún no hay carpetas recientes." } else { "Sin coincidencias entre las carpetas recientes." };
        list = list.push(container(text(msg).size(theme::SMALL_SIZE).color(theme::DIM)).padding([10, 12]));
    }
    let pick = theme::btn(row![text("Elegir otra carpeta…").size(theme::BODY_SIZE).color(theme::ACCENT), Space::with_width(Length::Fill), theme::keycap("Cmd+O")].align_y(Alignment::Center))
        .width(Length::Fill)
        .padding([8, 12])
        .on_press(Message::PickFolder)
        .style(theme::row(false));
    column![
        text_input("Abrir carpeta reciente…", query)
            .id(new_ws_input_id())
            .on_input(Message::NewWsQuery)
            .on_submit(Message::NewWsSubmit)
            .size(15)
            .padding([14, 16])
            .style(theme::search_input),
        theme::rule(false),
        container(column![scrollable(list).height(Length::Shrink), pick].spacing(1)).padding(6).max_height(440),
        theme::rule(false),
        hints(&[("Enter", "abrir la primera"), ("Esc", "cerrar")]),
    ]
    .into()
}

/// Etiqueta de un campo del formulario.
fn label<'a>(s: &'a str) -> Element<'a, Message> {
    text(s).size(theme::SMALL_SIZE).color(MUTED).into()
}

pub fn form_panel<'a>(app: &'a App, f: &'a NewPane) -> Element<'a, Message> {
    let presets: Vec<String> = Preset::ALL.iter().map(|p| p.label().to_string()).collect();
    let mut managers: Vec<String> = vec![NO_MANAGER.to_string()];
    managers.extend(app.panes.iter().map(|p| p.name.clone()));
    let manager_sel = f.manager.clone().unwrap_or_else(|| NO_MANAGER.to_string());
    let mut form = column![
        column![label("Nombre"), text_input("p. ej. worker-tests", &f.name).on_input(Message::FormName).padding(8).style(theme::field)].spacing(4),
        column![label("Workspace"), text_input("vacío = el del repo git", &f.workspace).on_input(Message::FormWorkspace).padding(8).style(theme::field)].spacing(4),
        column![label("Comando"), pick_list(presets, Some(f.preset.label().to_string()), Message::FormPreset).width(Length::Fill).padding(8).style(theme::select).menu_style(theme::menu)].spacing(4),
    ]
    .spacing(12);
    if f.preset == Preset::Custom {
        form = form.push(text_input("p. ej. bash --norc", &f.custom).on_input(Message::FormCustom).padding(8).style(theme::field));
    }
    form = form
        .push(column![label("Directorio"), text_input(&tilde(&home_dir()), &f.cwd).on_input(Message::FormCwd).padding(8).style(theme::field)].spacing(4))
        .push(column![label("Manager (recibe el resultado)"), pick_list(managers, Some(manager_sel), Message::FormManager).width(Length::Fill).padding(8).style(theme::select).menu_style(theme::menu)].spacing(4));
    if let Some(e) = &app.form_error {
        form = form.push(text(e.clone()).size(theme::SMALL_SIZE).color(theme::FAILED));
    }
    let actions = row![
        Space::with_width(Length::Fill),
        theme::btn_secondary(text("Cancelar").size(theme::BODY_SIZE)).padding([6, 14]).on_press(Message::CancelForm),
        theme::btn_primary(text("Crear panel").size(theme::BODY_SIZE)).padding([6, 14]).on_press(Message::SubmitForm),
    ]
    .spacing(8);
    column![
        container(text("Nuevo panel").size(theme::TITLE_SIZE).color(TEXT)).padding([14, 16]),
        theme::rule(false),
        container(form).padding(16),
        theme::rule(false),
        container(actions).padding([10, 16]),
    ]
    .into()
}
