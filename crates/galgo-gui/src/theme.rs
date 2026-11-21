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
/// Espaciado entre elementos de una columna o fila.
pub const GAP: f32 = 8.0;
/// Margen interior de paneles y barras.
pub const PAD: f32 = 12.0;
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

/// Panel de terminal: borde fino; con el foco, del color de acento. Así también se ven los divisores de los splits.
pub fn leaf(focused: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(term::DEFAULT_BG.into()),
        border: Border { color: if focused { ACCENT } else { BORDER }, width: LINE, radius: RADIUS.into() },
        ..Default::default()
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
