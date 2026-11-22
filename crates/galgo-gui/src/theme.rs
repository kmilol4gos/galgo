//! Tema visual de la GUI: colores, espaciados y estilos en un solo módulo.
//!
//! Reglas: un único color de acento (`ACCENT`), solo para foco, selección y la acción principal. Lo demás son grises
//! sobrios y los colores de estado (un punto por agente). Filas planas: fondo transparente, con un tono sutil al pasar
//! el puntero o al estar seleccionadas. Bordes finos y esquinas poco redondeadas.

use crate::term;
use galgo_proto::Status;
use iced::theme::Palette;
use iced::widget::{button, container};
use iced::{Background, Border, Color, Element, Shadow, Theme};

pub const BG: Color = Color::from_rgb(0.055, 0.059, 0.071);
pub const SIDEBAR: Color = Color::from_rgb(0.071, 0.075, 0.090);
pub const HOVER: Color = Color::from_rgb(0.102, 0.110, 0.129);
pub const SELECTED: Color = Color::from_rgb(0.129, 0.141, 0.169);
pub const BORDER: Color = Color::from_rgb(0.149, 0.165, 0.192);
pub const TEXT: Color = Color::from_rgb(0.894, 0.902, 0.918);
/// Texto de las filas no seleccionadas: un punto más apagado que `TEXT`.
pub const TEXT_SOFT: Color = Color::from_rgb(0.776, 0.788, 0.812);
pub const MUTED: Color = Color::from_rgb(0.541, 0.565, 0.600);
pub const DIM: Color = Color::from_rgb(0.357, 0.380, 0.420);
pub const ACCENT: Color = Color::from_rgb(0.357, 0.553, 0.937);
pub const WORKING: Color = Color::from_rgb(0.424, 0.714, 0.839);
pub const BLOCKED: Color = Color::from_rgb(0.878, 0.643, 0.345);
pub const DONE: Color = Color::from_rgb(0.498, 0.753, 0.541);
pub const FAILED: Color = Color::from_rgb(0.878, 0.424, 0.459);
pub const IDLE: Color = Color::from_rgb(0.357, 0.380, 0.420);

/// Esquina de botones, filas y paneles.
pub const RADIUS: f32 = 3.0;
/// Grosor de los bordes finos (divisores de splits, foco).
pub const LINE: f32 = 1.0;
/// Tamaños de letra: título, cuerpo y texto secundario.
pub const TITLE_SIZE: f32 = 16.0;
pub const BODY_SIZE: f32 = 13.0;
pub const SMALL_SIZE: f32 = 11.0;

/// El tema de iced con la paleta de galgo: el acento es el primario (barras de progreso, desplazamiento).
pub fn app() -> Theme {
    Theme::custom(
        "galgo".into(),
        Palette { background: BG, text: TEXT, primary: ACCENT, success: DONE, danger: FAILED },
    )
}

/// Color del punto de estado de un agente: working, blocked, done, failed o idle.
pub fn status_color(status: Status) -> Color {
    match status {
        Status::Working => WORKING,
        Status::Blocked => BLOCKED,
        Status::Done => DONE,
        Status::Failed | Status::Exited => FAILED,
        Status::Idle => IDLE,
    }
}

/// Botón plano y secundario: sin fondo, con un tono al pasar el puntero.
pub fn btn<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> button::Button<'a, M> {
    button(content).style(flat)
}

/// Botón secundario: superficie con borde fino; la acción que no es la principal de la pantalla.
pub fn btn_secondary<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> button::Button<'a, M> {
    button(content).style(secondary)
}

pub fn secondary(_: &Theme, status: button::Status) -> button::Style {
    let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if hover { HOVER } else { SIDEBAR })),
        text_color: TEXT,
        border: Border { color: BORDER, width: LINE, radius: RADIUS.into() },
        shadow: Shadow::default(),
    }
}

/// Botón primario (la acción principal de una pantalla): el único con el acento de fondo.
pub fn btn_primary<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> button::Button<'a, M> {
    button(content).style(primary)
}

pub fn flat(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Some(Background::Color(HOVER)),
        _ => None,
    };
    button::Style {
        background,
        text_color: TEXT,
        border: Border { color: BORDER, width: 0.0, radius: RADIUS.into() },
        shadow: Shadow::default(),
    }
}

pub fn primary(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Disabled => SELECTED,
        _ => ACCENT,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: TEXT,
        border: Border { color: ACCENT, width: 0.0, radius: RADIUS.into() },
        shadow: Shadow::default(),
    }
}

/// Fila de la barra lateral o de la paleta: plana; seleccionada, con el tono de selección.
pub fn row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = if selected {
            Some(Background::Color(SELECTED))
        } else if hover {
            Some(Background::Color(HOVER))
        } else {
            None
        };
        button::Style {
            background,
            text_color: TEXT,
            border: Border { color: BORDER, width: 0.0, radius: RADIUS.into() },
            shadow: Shadow::default(),
        }
    }
}

/// Fondo sólido de un contenedor (barra lateral, cabecera, ventana).
pub fn surface(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style { background: Some(color.into()), text_color: Some(TEXT), ..Default::default() }
}

/// Línea de 2 px bajo una pestaña: del acento si la pestaña está activa; si no, transparente.
pub fn tab_underline(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(if active { ACCENT } else { Color::TRANSPARENT })),
        ..Default::default()
    }
}

/// Fondo del área de terminales (igual que el del terminal: el panel no se distingue de su marco).
pub const TERM_BG: Color = term::DEFAULT_BG;
/// Superficie elevada: paleta, lanzador y diálogos que flotan sobre el terminal.
pub const ELEVATED: Color = Color::from_rgb(0.090, 0.096, 0.114);
/// Velo bajo una capa flotante.
pub const SCRIM: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.55 };
/// Esquina de las capas flotantes.
pub const RADIUS_LG: f32 = 8.0;

/// Atajo en el formato de macOS: «Cmd+Shift+U» → «⌘⇧U».
pub fn keys(spec: &str) -> String {
    spec.split('+')
        .map(|k| match k {
            "Cmd" => "⌘".to_string(),
            "Shift" => "⇧".to_string(),
            "Opt" | "Alt" => "⌥".to_string(),
            "Ctrl" => "⌃".to_string(),
            "Enter" => "enter".to_string(),
            "Esc" => "esc".to_string(),
            other => other.to_uppercase(),
        })
        .collect()
}

/// Tecla dibujada: texto pequeño en una caja de borde fino.
pub fn keycap<'a, M: 'a>(spec: &str) -> Element<'a, M> {
    container(iced::widget::text(keys(spec)).size(SMALL_SIZE).color(MUTED))
        .padding([1, 6])
        .style(|_| container::Style {
            background: Some(Background::Color(Color { a: 0.04, ..Color::WHITE })),
            border: Border { color: BORDER, width: LINE, radius: RADIUS.into() },
            ..Default::default()
        })
        .into()
}

/// Línea de 1 px (divisor) del color de borde. `vertical` = línea vertical.
pub fn rule<'a, M: 'a>(vertical: bool) -> Element<'a, M> {
    use iced::Length;
    let (w, h) = if vertical { (Length::Fixed(LINE), Length::Fill) } else { (Length::Fill, Length::Fixed(LINE)) };
    container(iced::widget::Space::new(w, h)).style(surface(BORDER)).into()
}

/// Capa flotante: superficie elevada, borde fino, esquinas amplias y sombra.
pub fn overlay(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(ELEVATED)),
        text_color: Some(TEXT),
        border: Border { color: BORDER, width: LINE, radius: RADIUS_LG.into() },
        shadow: Shadow { color: Color { a: 0.5, ..Color::BLACK }, offset: iced::Vector::new(0.0, 12.0), blur_radius: 32.0 },
    }
}

/// Pestaña de la barra superior: la activa toma el fondo del terminal (se funde con él) y el texto claro.
pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = if active {
            Some(Background::Color(TERM_BG))
        } else if hover {
            Some(Background::Color(HOVER))
        } else {
            None
        };
        button::Style {
            background,
            text_color: if active { TEXT } else { MUTED },
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
            shadow: Shadow::default(),
        }
    }
}

/// Campo de búsqueda grande y sin borde (paleta, nuevo workspace): el foco no necesita marco, el cursor basta.
pub fn search_input(_: &Theme, status: iced::widget::text_input::Status) -> iced::widget::text_input::Style {
    let _ = status;
    iced::widget::text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
        icon: MUTED,
        placeholder: DIM,
        value: TEXT,
        selection: Color { a: 0.35, ..ACCENT },
    }
}

/// Campo de formulario: superficie del terminal con borde fino; con el foco, borde de acento.
pub fn field(_: &Theme, status: iced::widget::text_input::Status) -> iced::widget::text_input::Style {
    let focused = matches!(status, iced::widget::text_input::Status::Focused);
    iced::widget::text_input::Style {
        background: Background::Color(TERM_BG),
        border: Border { color: if focused { ACCENT } else { BORDER }, width: LINE, radius: RADIUS.into() },
        icon: MUTED,
        placeholder: DIM,
        value: TEXT,
        selection: Color { a: 0.35, ..ACCENT },
    }
}

/// Desplegable de formulario: como un campo, con la flecha apagada.
pub fn select(_: &Theme, status: iced::widget::pick_list::Status) -> iced::widget::pick_list::Style {
    let open = matches!(status, iced::widget::pick_list::Status::Opened | iced::widget::pick_list::Status::Hovered);
    iced::widget::pick_list::Style {
        text_color: TEXT,
        placeholder_color: DIM,
        handle_color: MUTED,
        background: Background::Color(TERM_BG),
        border: Border { color: if open { MUTED } else { BORDER }, width: LINE, radius: RADIUS.into() },
    }
}

/// Menú de un desplegable: superficie elevada, la opción bajo el puntero con el acento.
pub fn menu(_: &Theme) -> iced::overlay::menu::Style {
    iced::overlay::menu::Style {
        background: Background::Color(ELEVATED),
        border: Border { color: BORDER, width: LINE, radius: RADIUS.into() },
        text_color: TEXT,
        selected_text_color: TEXT,
        selected_background: Background::Color(Color { a: 0.35, ..ACCENT }),
    }
}
