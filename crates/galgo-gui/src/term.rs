//! Paleta ANSI y codificación de teclas: lo que convierte la GUI en un terminal de verdad.
use galgo_proto::Color as C;
use iced::keyboard::{key::Named, Key, Modifiers};
use iced::Color;

pub const DEFAULT_FG: Color = Color::from_rgb(0.86, 0.87, 0.90);
pub const DEFAULT_BG: Color = Color::from_rgb(0.04, 0.045, 0.055);

const BASE16: [(u8, u8, u8); 16] = [
    (0x1d, 0x1f, 0x21), (0xcc, 0x66, 0x66), (0xb5, 0xbd, 0x68), (0xf0, 0xc6, 0x74),
    (0x81, 0xa2, 0xbe), (0xb2, 0x94, 0xbb), (0x8a, 0xbe, 0xb7), (0xc5, 0xc8, 0xc6),
    (0x66, 0x69, 0x6c), (0xd5, 0x4e, 0x53), (0xb9, 0xca, 0x4a), (0xe7, 0xc5, 0x47),
    (0x7a, 0xa6, 0xda), (0xc3, 0x97, 0xd8), (0x70, 0xc0, 0xb1), (0xea, 0xea, 0xea),
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
        // Alt delante de una tecla con nombre (salvo que la secuencia ya lleve el modificador).
        let alt_prefix = mods.alt() && m == 1 && !matches!(n, Named::Space);
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
        assert_eq!(encode(&named(Named::ArrowRight), Modifiers::ALT, None).as_deref(), Some("\x1b[1;3C"));
        assert_eq!(encode(&named(Named::Home), Modifiers::CTRL | Modifiers::SHIFT, None).as_deref(), Some("\x1b[1;6H"));
        assert_eq!(encode(&named(Named::Delete), Modifiers::CTRL, None).as_deref(), Some("\x1b[3;5~"));
    }
}
