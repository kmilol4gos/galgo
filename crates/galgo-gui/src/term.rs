//! Paleta ANSI y codificación de teclas: lo que convierte la GUI en un terminal de verdad.
use galgo_proto::Color as C;
use iced::keyboard::{key::Named, Key, Modifiers};
use iced::Color;

/// Texto y fondo del terminal: los del tema cálido (el mismo fondo que el chrome).
pub const DEFAULT_FG: Color = Color::from_rgb(0.925, 0.890, 0.824);
pub const DEFAULT_BG: Color = crate::theme::TERM_BG;

/// Colores ANSI 0-15 en tonos tierra y ámbar, a juego con el tema (p10k en Rex sale en tan/ámbar, no en azul
/// xterm). Negro y blanco quedan legibles sobre el fondo cálido; los colores 16-255 no cambian.
const BASE16: [(u8, u8, u8); 16] = [
    (0x2b, 0x26, 0x20), (0xc4, 0x78, 0x6a), (0x9a, 0xaa, 0x6a), (0xc9, 0xa5, 0x6f),
    (0x7f, 0x9b, 0xb0), (0xb0, 0x8a, 0xa6), (0x86, 0xaa, 0xa0), (0xd8, 0xcf, 0xbd),
    (0x5f, 0x57, 0x4c), (0xd8, 0x9a, 0x8c), (0xb8, 0xc4, 0x8a), (0xe0, 0xbc, 0x86),
    (0x9f, 0xb6, 0xc8), (0xc9, 0xaa, 0xbf), (0x9c, 0xc4, 0xb8), (0xf2, 0xeb, 0xdc),
];

/// Color xterm de 256 índices.
pub fn idx(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => BASE16[i as usize],
        16..=231 => {
            let n = i - 16;
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (lvl(n / 36), lvl((n / 6) % 6), lvl(n % 6))
        }
        232..=255 => {
            let g = 8 + (i - 232) * 10;
            (g, g, g)
        }
    }
}

/// `None` si es el color por defecto.
pub fn color(c: C) -> Option<Color> {
    let (r, g, b) = match c {
        C::Default => return None,
        C::Idx(i) => idx(i),
        C::Rgb(r, g, b) => (r, g, b),
    };
    Some(Color::from_rgb8(r, g, b))
}

/// Parámetro de modificador de las secuencias xterm: 1 + Shift(1) + Alt(2) + Ctrl(4).
/// Sin modificadores vale 1 y la secuencia se escribe sin parámetro.
fn xterm_mod(mods: Modifiers) -> u8 {
    1 + u8::from(mods.shift()) + 2 * u8::from(mods.alt()) + 4 * u8::from(mods.control())
}

/// Bytes que se envían al PTY para una tecla; `None` si no produce nada.
pub fn encode(key: &Key, mods: Modifiers, text: Option<&str>) -> Option<String> {
    // Cmd+tecla es de la aplicación (Cmd+V, Cmd+C…) o no tiene significado en un terminal:
    // nunca llega al panel.
    if mods.command() {
        return None;
    }
    if let Key::Named(n) = key {
        let m = xterm_mod(mods);
        // Flechas, inicio, fin: `ESC [ X` sin modificadores, `ESC [ 1 ; m X` con ellos.
        let letter = |c: char| if m > 1 { format!("\x1b[1;{m}{c}") } else { format!("\x1b[{c}") };
        // Teclas `ESC [ n ~`, con el mismo parámetro de modificador cuando lo hay.
        let tilde = |n: u8| if m > 1 { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") };
        let seq: String = match n {
            Named::Enter => "\r".into(),
            Named::Backspace => "\x7f".into(),
            Named::Tab if mods.shift() => "\x1b[Z".into(),
            Named::Tab => "\t".into(),
            Named::Escape => "\x1b".into(),
            Named::Space if mods.control() => "\0".into(),
            Named::Space => " ".into(),
            Named::ArrowUp => letter('A'),
            Named::ArrowDown => letter('B'),
            Named::ArrowRight => letter('C'),
            Named::ArrowLeft => letter('D'),
            Named::Home => letter('H'),
            Named::End => letter('F'),
            Named::PageUp => tilde(5),
            Named::PageDown => tilde(6),
            Named::Delete => tilde(3),
            Named::Insert => tilde(2),
            Named::F1 => "\x1bOP".into(),
            Named::F2 => "\x1bOQ".into(),
            Named::F3 => "\x1bOR".into(),
            Named::F4 => "\x1bOS".into(),
            Named::F5 => tilde(15),
            Named::F6 => tilde(17),
            Named::F7 => tilde(18),
            Named::F8 => tilde(19),
            Named::F9 => tilde(20),
            Named::F10 => tilde(21),
            Named::F11 => tilde(23),
            Named::F12 => tilde(24),
            _ => return None,
        };
        // Edición de texto al estilo macOS: Option+flechas salta palabras (Meta-b / Meta-f, como iTerm2 y
        // Terminal.app), Option+Borrar borra la palabra anterior y Shift+Enter es un salto de línea sin enviar
        // (ESC CR: Claude Code y zsh lo entienden así).
        let only_alt = mods.alt() && !mods.shift() && !mods.control();
        match n {
            Named::ArrowLeft if only_alt => return Some("\x1bb".into()),
            Named::ArrowRight if only_alt => return Some("\x1bf".into()),
            Named::Enter if mods.shift() && !mods.alt() && !mods.control() => return Some("\x1b\r".into()),
            _ => {}
        }
        // Alt delante de una tecla con nombre (salvo que la secuencia ya lleve el modificador). Enter, Borrar,
        // Tab y Esc no llevan parámetro xterm: con Alt van siempre con ESC delante.
        let plain = matches!(n, Named::Enter | Named::Backspace | Named::Tab | Named::Escape);
        let alt_prefix = mods.alt() && (m == 1 || plain) && !matches!(n, Named::Space);
        return Some(if alt_prefix { format!("\x1b{seq}") } else { seq });
    }
    if mods.control() {
        if let Key::Character(c) = key {
            let ch = c.chars().next()?.to_ascii_lowercase();
            return match ch {
                'a'..='z' => Some(((ch as u8 - b'a' + 1) as char).to_string()),
                '[' => Some("\x1b".into()),
                '\\' => Some("\x1c".into()),
                ']' => Some("\x1d".into()),
                // Ctrl+^ (en US, Ctrl+Shift+6) y Ctrl+_ / Ctrl+- / Ctrl+/ (Ctrl+Shift+- en US).
                '^' | '6' => Some("\x1e".into()),
                '_' | '-' | '/' => Some("\x1f".into()),
                ' ' | '@' | '2' => Some("\0".into()),
                _ => None,
            };
        }
    }
    let t = text.filter(|t| !t.is_empty())?;
    // Option en macOS: `text` ya trae el carácter compuesto (Option+2 en teclado español es "@").
    // Ese carácter se envía tal cual. ESC solo antepuesto cuando `text` es la tecla base (Meta).
    if mods.alt() {
        if let Key::Character(base) = key {
            if base.as_str() == t {
                return Some(format!("\x1b{t}"));
            }
        }
    }
    Some(t.to_string())
}

/// Cmd+tecla con significado de edición en los terminales de macOS: Cmd+← / Cmd+→ van al inicio y al final de la
/// línea (Ctrl-A / Ctrl-E) y Cmd+Borrar borra hasta el inicio (Ctrl-U). El resto de Cmd es de la aplicación.
pub fn encode_cmd(key: &Key, mods: Modifiers) -> Option<String> {
    if !mods.command() || mods.alt() || mods.control() || mods.shift() {
        return None;
    }
    match key {
        Key::Named(Named::ArrowLeft) => Some("\x01".into()),
        Key::Named(Named::ArrowRight) => Some("\x05".into()),
        Key::Named(Named::Backspace) => Some("\x15".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Key;

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    fn named(n: Named) -> Key {
        Key::Named(n)
    }

    #[test]
    fn palette() {
        assert_eq!(idx(16), (0, 0, 0));
        assert_eq!(idx(231), (255, 255, 255));
        assert_eq!(idx(196), (255, 0, 0));
        assert_eq!(idx(232), (8, 8, 8));
        assert!(color(C::Default).is_none());
    }

    #[test]
    fn keys() {
        let none = Modifiers::empty();
        assert_eq!(encode(&Key::Named(Named::Enter), none, None).as_deref(), Some("\r"));
        assert_eq!(encode(&Key::Named(Named::ArrowUp), none, None).as_deref(), Some("\x1b[A"));
        assert_eq!(encode(&ch("c"), Modifiers::CTRL, Some("c")).as_deref(), Some("\x03"));
        assert_eq!(encode(&ch("d"), Modifiers::CTRL, None).as_deref(), Some("\x04"));
        assert_eq!(encode(&ch("a"), none, Some("a")).as_deref(), Some("a"));
        assert_eq!(encode(&ch("A"), Modifiers::SHIFT, Some("A")).as_deref(), Some("A"));
        assert_eq!(encode(&ch("b"), Modifiers::ALT, Some("b")).as_deref(), Some("\x1bb"));
        assert_eq!(encode(&ch("v"), Modifiers::LOGO, Some("v")), None);
        assert_eq!(encode(&ch("é"), none, Some("é")).as_deref(), Some("é"));
        assert_eq!(encode(&Key::Named(Named::Space), none, Some(" ")).as_deref(), Some(" "));
        assert_eq!(encode(&Key::Named(Named::Shift), Modifiers::SHIFT, None), None);
    }

    /// M1: en teclado español Option+2 llega como key "2" y text "@": va "@" sin ESC.
    #[test]
    fn option_envia_el_caracter_compuesto_sin_esc() {
        assert_eq!(encode(&ch("2"), Modifiers::ALT, Some("@")).as_deref(), Some("@"));
        // Option+´ en español: `text` es "[" y la tecla base no coincide: se envía "[" tal cual.
        assert_eq!(encode(&ch("´"), Modifiers::ALT, Some("[")).as_deref(), Some("["));
        // Option+b en US: text "∫" no es la tecla base: sin ESC.
        assert_eq!(encode(&ch("b"), Modifiers::ALT, Some("∫")).as_deref(), Some("∫"));
        // Alt con la tecla base (Meta): ESC delante.
        assert_eq!(encode(&ch("2"), Modifiers::ALT, Some("2")).as_deref(), Some("\x1b2"));
    }

    /// B5: Ctrl+Espacio es NUL; Ctrl+/ y Ctrl+_ son \x1f; Ctrl+^ es \x1e.
    #[test]
    fn control_con_simbolos() {
        assert_eq!(encode(&named(Named::Space), Modifiers::CTRL, Some(" ")).as_deref(), Some("\0"));
        assert_eq!(encode(&ch("/"), Modifiers::CTRL, Some("/")).as_deref(), Some("\x1f"));
        assert_eq!(encode(&ch("_"), Modifiers::CTRL | Modifiers::SHIFT, Some("_")).as_deref(), Some("\x1f"));
        assert_eq!(encode(&ch("-"), Modifiers::CTRL | Modifiers::SHIFT, Some("_")).as_deref(), Some("\x1f"));
        assert_eq!(encode(&ch("^"), Modifiers::CTRL | Modifiers::SHIFT, Some("^")).as_deref(), Some("\x1e"));
    }

    /// B5: F1-F12 con sus secuencias xterm.
    #[test]
    fn teclas_de_funcion() {
        let none = Modifiers::empty();
        let esperadas = [
            (Named::F1, "\x1bOP"), (Named::F2, "\x1bOQ"), (Named::F3, "\x1bOR"), (Named::F4, "\x1bOS"),
            (Named::F5, "\x1b[15~"), (Named::F6, "\x1b[17~"), (Named::F7, "\x1b[18~"), (Named::F8, "\x1b[19~"),
            (Named::F9, "\x1b[20~"), (Named::F10, "\x1b[21~"), (Named::F11, "\x1b[23~"), (Named::F12, "\x1b[24~"),
        ];
        for (k, seq) in esperadas {
            assert_eq!(encode(&named(k), none, None).as_deref(), Some(seq), "{k:?}");
        }
    }

    /// B5: Cmd con teclas con nombre nunca llega al panel.
    #[test]
    fn cmd_con_tecla_con_nombre_es_none() {
        for k in [Named::Enter, Named::Backspace, Named::ArrowUp, Named::Tab, Named::Escape] {
            assert_eq!(encode(&named(k), Modifiers::LOGO, None), None, "{k:?}");
        }
    }

    /// B5: flechas con modificadores en formato xterm `ESC [ 1 ; m X`.
    #[test]
    fn flechas_con_modificadores() {
        assert_eq!(encode(&named(Named::ArrowUp), Modifiers::CTRL, None).as_deref(), Some("\x1b[1;5A"));
        assert_eq!(encode(&named(Named::ArrowLeft), Modifiers::SHIFT, None).as_deref(), Some("\x1b[1;2D"));
        assert_eq!(encode(&named(Named::ArrowRight), Modifiers::ALT | Modifiers::SHIFT, None).as_deref(), Some("\x1b[1;4C"));
        assert_eq!(encode(&named(Named::Home), Modifiers::CTRL | Modifiers::SHIFT, None).as_deref(), Some("\x1b[1;6H"));
        assert_eq!(encode(&named(Named::Delete), Modifiers::CTRL, None).as_deref(), Some("\x1b[3;5~"));
    }

    /// Edición al estilo macOS: Option salta y borra palabras, Shift+Enter es salto de línea, Cmd va a los extremos.
    #[test]
    fn edicion_al_estilo_macos() {
        assert_eq!(encode(&named(Named::ArrowLeft), Modifiers::ALT, None).as_deref(), Some("\x1bb"));
        assert_eq!(encode(&named(Named::ArrowRight), Modifiers::ALT, None).as_deref(), Some("\x1bf"));
        assert_eq!(encode(&named(Named::Backspace), Modifiers::ALT, None).as_deref(), Some("\x1b\x7f"));
        assert_eq!(encode(&named(Named::Enter), Modifiers::SHIFT, None).as_deref(), Some("\x1b\r"));
        assert_eq!(encode_cmd(&named(Named::ArrowLeft), Modifiers::LOGO).as_deref(), Some("\x01"));
        assert_eq!(encode_cmd(&named(Named::ArrowRight), Modifiers::LOGO).as_deref(), Some("\x05"));
        assert_eq!(encode_cmd(&named(Named::Backspace), Modifiers::LOGO).as_deref(), Some("\x15"));
        assert_eq!(encode_cmd(&named(Named::ArrowLeft), Modifiers::LOGO | Modifiers::ALT), None, "Cmd+Opt es foco de split");
        assert_eq!(encode_cmd(&ch("v"), Modifiers::LOGO), None);
    }
}
