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

/// Bytes que se envían al PTY para una tecla; `None` si no produce nada.
pub fn encode(key: &Key, mods: Modifiers, text: Option<&str>) -> Option<String> {
    if let Key::Named(n) = key {
        let seq = match n {
            Named::Enter => "\r",
            Named::Backspace => "\x7f",
            Named::Tab if mods.shift() => "\x1b[Z",
            Named::Tab => "\t",
            Named::Escape => "\x1b",
            Named::ArrowUp => "\x1b[A",
            Named::ArrowDown => "\x1b[B",
            Named::ArrowRight => "\x1b[C",
            Named::ArrowLeft => "\x1b[D",
            Named::Home => "\x1b[H",
            Named::End => "\x1b[F",
            Named::PageUp => "\x1b[5~",
            Named::PageDown => "\x1b[6~",
            Named::Delete => "\x1b[3~",
            Named::Insert => "\x1b[2~",
            Named::Space => " ",
            _ => return text.filter(|t| !t.is_empty() && !mods.command()).map(String::from),
        };
        let seq = if mods.alt() && *n != Named::Space { format!("\x1b{seq}") } else { seq.to_string() };
        return Some(seq);
    }
    if mods.command() {
        return None; // atajos de la aplicación (Cmd+V…)
    }
    if mods.control() {
        if let Key::Character(c) = key {
            let ch = c.chars().next()?.to_ascii_lowercase();
            return match ch {
                'a'..='z' => Some(((ch as u8 - b'a' + 1) as char).to_string()),
                '[' => Some("\x1b".into()),
                '\\' => Some("\x1c".into()),
                ']' => Some("\x1d".into()),
                ' ' | '@' | '2' => Some("\0".into()),
                _ => None,
            };
        }
    }
    let t = text.filter(|t| !t.is_empty())?;
    Some(if mods.alt() { format!("\x1b{t}") } else { t.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Key;

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
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
}
