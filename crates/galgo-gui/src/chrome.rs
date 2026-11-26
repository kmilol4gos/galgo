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

/// Cabecera de sección en minúsculas, como Rex («machines», «agents»), con un texto opcional a la derecha.
fn lower_section<'a>(label: &'a str, right: Option<&'a str>) -> Element<'a, Message> {
    let mut r = row![text(label).size(theme::BODY_SIZE).color(theme::MUTED)].align_y(Alignment::Center);
    if let Some(t) = right {
        r = r.push(Space::with_width(Length::Fill)).push(text(t).size(theme::SMALL_SIZE).color(theme::DIM));
    }
    container(r).padding(iced::Padding { top: 10.0, bottom: 4.0, left: 8.0, right: 8.0 }).into()
}

/// Estado más urgente de un grupo de paneles (el que decide el punto del proyecto).
fn worst(panes: &[&PaneInfo]) -> Status {
    panes.iter().map(|p| p.status).min_by_key(|s| sidebar::priority(*s)).unwrap_or(Status::Idle)
}

/// Fila de un agente en «agents»: punto de estado, nombre, título de la tarea debajo y proveedor·modelo a la derecha.
/// Texto del lado derecho de una fila de agente: la insignia de atención si la hay (M1), o proveedor·modelo.
pub fn agent_side(app: &App, pane: &PaneInfo) -> (String, Color) {
    if let Some(b) = attention::badge(pane, app.attention.is_unread(pane.id)) {
        let (label, color) = match b {
            attention::Badge::Blocked => ("necesita decisión", theme::BLOCKED),
            attention::Badge::DoneUnread => ("sin leer", theme::DONE),
            attention::Badge::Orphan => ("sin confirmar", theme::FAILED),
        };
        return (label.to_string(), color);
    }
    let provider = sidebar::provider(pane);
    let who = match sidebar::model(pane) {
        Some(m) => format!("{provider} · {m}"),
        None => provider,
    };
    (who, theme::DIM)
}

fn agent_row<'a>(app: &'a App, pane: &'a PaneInfo) -> Element<'a, Message> {
    let selected = app.selected == Some(pane.id);
    let (side, side_color) = agent_side(app, pane);
    // B3: el título se recorta; así proveedor·modelo siempre cabe a la derecha.
    let title = sidebar::recortar(&pane.title.clone().unwrap_or_else(|| pane.status.as_str().to_string()), 40);
    let body = column![
        text(&pane.name).size(theme::BODY_SIZE).color(if selected { TEXT } else { theme::TEXT_SOFT }).wrapping(iced::widget::text::Wrapping::None),
        text(title).size(theme::SMALL_SIZE).color(MUTED).wrapping(iced::widget::text::Wrapping::None),
    ]
    .spacing(1);
    let line = row![
        dot(theme::status_color(pane.status), 7.0),
        body,
        Space::with_width(Length::Fill),
        text(side).size(theme::SMALL_SIZE).color(side_color).wrapping(iced::widget::text::Wrapping::None),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    theme::btn(container(line).width(Length::Fill).padding([5, 8]).clip(true))
        .padding(0)
        .width(Length::Fill)
        .on_press(Message::Select(pane.id))
        .style(theme::row(selected))
        .into()
}

fn sidebar_list(app: &App) -> Element<'_, Message> {
    let mut list = column![].spacing(1);
    // machines: Local > proyecto > rama. Un clic en la rama selecciona su primer panel.
    list = list.push(lower_section("machines", None));
    list = list.push(container(text("▾ Local").size(theme::BODY_SIZE).color(TEXT)).padding([3, 8]));
    for project in sidebar::tree(&app.panes) {
        list = list.push(container(text(project.name.clone()).size(theme::BODY_SIZE).color(theme::TEXT_SOFT)).padding(iced::Padding { top: 4.0, bottom: 0.0, left: 16.0, right: 8.0 }));
        for ws in project.workspaces {
            let state = worst(&ws.panes);
            let first = ws.panes.first().map(|p| p.id);
            let head = row![
                dot(theme::status_color(state), 7.0),
                text(ws.name.clone()).size(theme::SMALL_SIZE + 1.0).color(MUTED).wrapping(iced::widget::text::Wrapping::None),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            let mut item = theme::btn(container(head).width(Length::Fill).padding(iced::Padding { top: 2.0, bottom: 2.0, left: 30.0, right: 8.0 }).clip(true))
                .padding(0)
                .width(Length::Fill)
                .style(theme::row(false));
            if let Some(id) = first {
                item = item.on_press(Message::Select(id));
            }
            list = list.push(item);
        }
    }
    // Pie de «machines», como Rex: crear y menú.
    list = list.push(
        container(
            row![
                theme::btn(text("new").size(theme::SMALL_SIZE).color(MUTED)).padding([2, 6]).on_press(Message::OpenNewWorkspace),
                text("· Local").size(theme::SMALL_SIZE).color(theme::DIM),
                Space::with_width(Length::Fill),
                theme::btn(text("menu").size(theme::SMALL_SIZE).color(MUTED)).padding([2, 6]).on_press(Message::OpenPalette),
            ]
            .align_y(Alignment::Center),
        )
        .padding([4, 8]),
    );
    // agents · priority: todos los agentes, por prioridad.
    list = list.push(lower_section("agents", Some("priority")));
    if app.panes.is_empty() {
        list = list.push(container(text("Sin agentes. Abre una carpeta con ⌘N.").size(theme::SMALL_SIZE).color(theme::DIM)).padding([8, 8]));
    }
    for pane in sidebar::agents_sorted(&app.panes) {
        list = list.push(agent_row(app, pane));
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

/// Barra superior de sesiones, dentro de la barra de título (Rex): una sesión por workspace, la activa resaltada,
/// y a la derecha las acciones. El hueco de la izquierda deja sitio a los semáforos de macOS.
pub fn session_bar(app: &App) -> Element<'_, Message> {
    let current = focus_key(app);
    // M1: la ventana no muestra «(n)» en la barra de título (título oculto): el contador va aquí.
    let atencion = app.attention.count(&app.panes);
    // M3: los huecos de la barra arrastran la ventana (window::drag); los botones de sesión siguen siendo clics.
    let arrastre = || mouse_area(Space::with_width(Length::Fill)).on_press(Message::DragWindow);
    let mut bar = row![mouse_area(Space::with_width(78.0)).on_press(Message::DragWindow), text("galgo").size(theme::BODY_SIZE).font(Font { weight: font::Weight::Bold, ..terminal_font() }).color(TEXT)]
        .spacing(8)
        .align_y(Alignment::Center)
        .height(TABS_H);
    if atencion > 0 {
        bar = bar.push(text(atencion.to_string()).size(theme::SMALL_SIZE).color(theme::BLOCKED));
    }
    bar = bar.push(Space::with_width(12.0));
    for ws in sidebar::workspaces(&app.panes) {
        let key = ws.label.clone().unwrap_or_default();
        let active = current.as_deref().is_some_and(|k| k == key);
        let state = ws.panes.iter().map(|p| p.status).min_by_key(|s| sidebar::priority(*s)).unwrap_or(Status::Idle);
        let label = row![dot(theme::status_color(state), 6.0), text(ws.label.clone().unwrap_or_else(|| "sin workspace".into())).size(theme::SMALL_SIZE + 1.0).color(if active { TEXT } else { MUTED }).wrapping(iced::widget::text::Wrapping::None)]
            .spacing(7)
            .align_y(Alignment::Center);
        // B4: la sesión se elige por su clave, no por su posición, que puede cambiar entre el dibujo y el clic.
        bar = bar.push(theme::btn(container(label).padding([4, 12])).padding(0).on_press(Message::SelectWorkspace(key.clone())).style(theme::tab(active)));
    }
    bar = bar.push(icon_button(text("+").size(16).color(MUTED).into(), "Nuevo workspace  ⌘N", Message::OpenNewWorkspace));
    bar = bar
        .push(arrastre())
        .push(icon_button(text("⌘").size(14).color(MUTED).into(), "Buscar o ejecutar  ⌘K", Message::OpenPalette))
        .push(Space::with_width(8));
    container(bar).width(Length::Fill).style(theme::surface(SIDEBAR)).into()
}

/// Hora local HH:MM, sin dependencias de fecha: `localtime_r` de libc.
pub fn clock_hm() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as libc::time_t).unwrap_or(0);
    // SAFETY: `tm` es un registro de salida de `localtime_r`, que lo rellena por completo.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

pub fn sidebar_view(app: &App) -> Element<'_, Message> {
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
                let zoomed = active && maximized(app).is_some();
                label = label.push(text(if zoomed { format!("1/{n}") } else { format!("{n}") }).size(10).color(if zoomed { theme::ACCENT } else { theme::DIM }));
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
        .push(text(clock_hm()).size(theme::SMALL_SIZE + 1.0).color(theme::DIM))
        .push(Space::with_width(10));
    column![container(bar).width(Length::Fill).style(theme::surface(SIDEBAR)), theme::rule(false)].into()
}

/// El área de paneles: la pestaña visible del workspace enfocado, con sus splits. Sobre un divisor (o arrastrándolo)
/// el puntero muestra la flecha de redimensionar.
pub fn layout_view(app: &App) -> Element<'_, Message> {
    let inner: Element<'_, Message> = match focus_key(app).and_then(|k| app.layouts.active_tab(&k)) {
        Some(tab) => match maximized(app) {
            // Maximizado: solo ese panel, sin marco de split.
            Some(id) => leaf_view(app, id, false),
            None => {
                let split = matches!(tab.root, layout::Node::Split { .. });
                node_view(app, &tab.root, split, &mut Vec::new())
            }
        },
        None => placeholder("Esperando salida…"),
    };
    let active = app.drag_divider.as_ref().map(|d| d.vertical).or(app.hover_divider.as_ref().map(|(_, v)| *v));
    match active {
        Some(vertical) => mouse_area(inner)
            .interaction(if vertical { iced::mouse::Interaction::ResizingHorizontally } else { iced::mouse::Interaction::ResizingVertically })
            .into(),
        None => inner,
    }
}

fn placeholder<'a>(msg: &'a str) -> Element<'a, Message> {
    container(text(msg).size(theme::BODY_SIZE).color(theme::DIM)).center(Length::Fill).into()
}

/// Divisor de 1 px; con el puntero encima o arrastrándolo, del color de acento.
fn divider<'a>(app: &App, path: &layout::Path, vertical: bool) -> Element<'a, Message> {
    let hot = app.drag_divider.as_ref().is_some_and(|d| &d.path == path) || app.hover_divider.as_ref().is_some_and(|(p, _)| p == path);
    let (w, h) = if vertical { (Length::Fixed(1.0), Length::Fill) } else { (Length::Fill, Length::Fixed(1.0)) };
    container(Space::new(w, h)).style(theme::surface(if hot { theme::ACCENT } else { theme::BORDER })).into()
}

/// Un nodo del layout: una hoja es un panel; un split reparte su área según su fracción, con un divisor de 1 px.
fn node_view<'a>(app: &'a App, node: &'a layout::Node, split: bool, path: &mut layout::Path) -> Element<'a, Message> {
    match node {
        layout::Node::Leaf(id) => leaf_view(app, *id, split),
        layout::Node::Split { vertical, ratio, a, b } => {
            let pa = (ratio * 1000.0).round().clamp(1.0, 999.0) as u16;
            let pb = 1000 - pa;
            path.push(false);
            let first = node_view(app, a, split, path);
            path.pop();
            path.push(true);
            let second = node_view(app, b, split, path);
            path.pop();
            let line = divider(app, path, *vertical);
            if *vertical {
                row![
                    container(first).width(Length::FillPortion(pa)).height(Length::Fill),
                    line,
                    container(second).width(Length::FillPortion(pb)).height(Length::Fill),
                ]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
            } else {
                column![
                    container(first).height(Length::FillPortion(pa)).width(Length::Fill),
                    line,
                    container(second).height(Length::FillPortion(pb)).width(Length::Fill),
                ]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
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

/// Modelo que muestra y lanza el desplegable de un agente: el elegido, o «predeterminado» si no hay elección (M2).
pub fn launcher_choice(app: &App, agent: launch::Agent) -> String {
    app.launch_model.get(agent.label()).cloned().unwrap_or_else(|| launch::DEFAULT_MODEL.to_string())
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
        let launch_row = theme::btn(line).width(Length::Fill).padding([8, 12]).on_press(Message::Launch(*agent)).style(theme::row(false));
        // Proveedor y modelo: el desplegable va fuera del botón de lanzar, para que su clic no lance el agente.
        let models = agent.models();
        let mut fila = row![launch_row].spacing(6).align_y(Alignment::Center);
        if !models.is_empty() {
            let label = agent.label().to_string();
            // M2: la primera opción es «predeterminado»; lo que se muestra es lo que se lanza.
            let options: Vec<String> = std::iter::once(launch::DEFAULT_MODEL.to_string()).chain(models.iter().map(|m| m.to_string())).collect();
            let elegido = Some(launcher_choice(app, *agent));
            fila = fila.push(
                pick_list(options, elegido, move |m| Message::LauncherModel(label.clone(), m))
                    .text_size(theme::SMALL_SIZE + 1.0)
                    .padding([3, 8]),
            );
        }
        list = list.push(fila);
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

// ---------------------------------------------------------------------------------------------------------------
// Ajustes (Cmd+,)
// ---------------------------------------------------------------------------------------------------------------

/// Una fila «etiqueta: valor» de la sección de ajustes.
fn kv<'a>(label: String, value: String) -> Element<'a, Message> {
    row![
        container(text(label).size(theme::SMALL_SIZE + 1.0).color(theme::DIM)).width(Length::Fixed(150.0)),
        text(value).size(theme::SMALL_SIZE + 1.0).color(TEXT),
    ]
    .spacing(8)
    .into()
}

/// Contenido de una sección de ajustes.
fn settings_content(app: &App, section: settings::Section) -> Element<'_, Message> {
    let mut col = column![text(section.label()).size(theme::BODY_SIZE + 1.0).color(TEXT)].spacing(8.0);
    match section {
        settings::Section::General => {
            let estado = match &app.error {
                None => "conectado al daemon".to_string(),
                Some(e) => format!("sin conexión: {e}"),
            };
            col = col
                .push(kv("Servidor".into(), estado))
                .push(kv("Socket".into(), galgo_proto::socket_path().display().to_string()))
                .push(kv("Recientes".into(), "junto al socket (gui-recientes.json)".into()));
        }
        settings::Section::Interface => {
            col = col
                .push(kv("Tema".into(), "cálido, oscuro".into()))
                .push(kv("Fuente".into(), "monoespaciada en toda la interfaz".into()))
                .push(kv("Barra de título".into(), "transparente; sesiones arriba".into()));
        }
        settings::Section::Terminal => {
            col = col
                .push(kv("Fuente del terminal".into(), "Menlo".into()))
                .push(kv("Historial".into(), "el del daemon (scrollback)".into()))
                .push(kv("Selección".into(), "por celda, con copia en el portapapeles".into()));
        }
        settings::Section::Shortcuts => {
            for (keys, what) in settings::SHORTCUTS {
                col = col.push(row![
                    container(text(keys).size(theme::SMALL_SIZE + 1.0).color(theme::DIM)).width(Length::Fixed(190.0)),
                    text(what).size(theme::SMALL_SIZE + 1.0).color(TEXT),
                ]
                .spacing(8));
            }
        }
        settings::Section::Integrations => {
            let destino = match settings::hooks_binary() {
                Ok(bin) => format!("Los hooks apuntarán a: {bin}"),
                Err(e) => format!("No se pueden instalar hooks: {e}"),
            };
            col = col.push(text(destino).size(theme::SMALL_SIZE).color(MUTED));
            col = col.push(text("Instalar muestra el plan (sin escribir nada). Solo «Aplicar» escribe la configuración del agente.").size(theme::SMALL_SIZE).color(MUTED));
            for agent in settings::AGENTS {
                let busy = app.hooks_busy.contains(agent);
                let salida = app.hooks_output.get(agent).cloned().unwrap_or_else(|| "consultando…".into());
                let mut acciones = row![
                    enabled_btn("Estado", !busy, Message::HooksStatus(agent.into())),
                    enabled_btn("Instalar…", !busy, Message::HooksPlan(agent.into())),
                ]
                .spacing(8.0);
                let mut bloque = column![text(agent).size(theme::BODY_SIZE).color(TEXT), text(salida).size(theme::SMALL_SIZE).color(MUTED)].spacing(4);
                if let Some(plan) = app.hooks_plan.get(agent) {
                    bloque = bloque.push(text(plan.clone()).size(theme::SMALL_SIZE).color(theme::TEXT_SOFT));
                    acciones = acciones
                        .push(enabled_btn("Aplicar", !busy, Message::HooksApply(agent.into())))
                        .push(enabled_btn("Cancelar", true, Message::HooksCancel(agent.into())));
                }
                col = col.push(bloque.push(acciones));
            }
        }
        settings::Section::Notifications => {
            col = col.push(text("Aviso de macOS cuando un agente se bloquea o termina y la ventana no tiene el foco.").size(theme::SMALL_SIZE).color(MUTED));
        }
        settings::Section::Usage => {
            let now = now_secs();
            if app.limits.is_empty() {
                col = col.push(text("Sin cuotas observadas. Claude las informa con su statusline.").size(theme::SMALL_SIZE).color(MUTED));
            }
            for l in &app.limits {
                col = col.push(text(l.agent.clone()).size(theme::BODY_SIZE).color(TEXT));
                for r in usage::limit_rows(l, now) {
                    col = col.push(kv(r.label.to_string(), format!("{:.0} % · {}", r.pct, r.detail)));
                }
            }
            col = col.push(text("Coste por proveedor (equivalente de lista)").size(theme::SMALL_SIZE).color(MUTED));
            for proveedor in ["claude", "codex", "pi", "opencode", "shell"] {
                let grupo: Vec<&PaneInfo> = app.panes.iter().filter(|p| sidebar::provider(p) == proveedor).collect();
                if grupo.is_empty() {
                    continue;
                }
                let total = usage::group_total(&grupo).unwrap_or_else(|| "sin uso medido".into());
                col = col.push(kv(proveedor.to_string(), format!("{} panel(es) · {total}", grupo.len())));
            }
        }
        settings::Section::Developer => {
            col = col
                .push(kv("Versión de la GUI".into(), env!("CARGO_PKG_VERSION").into()))
                .push(kv("Socket".into(), galgo_proto::socket_path().display().to_string()))
                .push(kv("CLI usada".into(), std::env::var("GALGO_BIN").unwrap_or_else(|_| "galgo (PATH)".into())));
        }
    }
    col.into()
}

/// Botón secundario que solo responde si `enabled` (B5: sin órdenes solapadas para el mismo agente).
fn enabled_btn<'a>(label: &'a str, enabled: bool, message: Message) -> Element<'a, Message> {
    let b = theme::btn_secondary(text(label).size(theme::SMALL_SIZE)).padding([4, 10]);
    if enabled { b.on_press(message).into() } else { b.into() }
}

/// Panel de ajustes: barra de secciones a la izquierda, contenido a la derecha. Cmd+, lo abre; Esc lo cierra.
pub fn settings_panel(app: &App) -> Element<'_, Message> {
    let current = app.settings.unwrap_or(settings::Section::General);
    let mut nav = column![].spacing(2);
    for s in settings::Section::ALL {
        let on = s == current;
        nav = nav.push(
            theme::btn(container(text(s.label()).size(theme::BODY_SIZE).color(if on { TEXT } else { MUTED })).padding([6, 10]))
                .width(Length::Fill)
                .padding(0)
                .on_press(Message::SettingsSection(s))
                .style(theme::row(on)),
        );
    }
    let header = row![
        text("Ajustes").size(theme::TITLE_SIZE).color(TEXT),
        Space::with_width(Length::Fill),
        theme::btn(text("×").size(16).color(MUTED)).padding([0, 6]).on_press(Message::CloseSettings),
    ]
    .align_y(Alignment::Center);
    let body = row![
        container(nav).width(Length::Fixed(170.0)),
        theme::rule(true),
        scrollable(container(settings_content(app, current)).padding([0, 14])).width(Length::Fill).height(Length::Fill),
    ]
    .height(Length::Fill);
    container(column![header, theme::rule(false), body].spacing(10).padding(14))
        .width(Length::Fixed(820.0))
        .height(Length::Fixed(540.0))
        .style(theme::surface(SIDEBAR))
        .into()
}
