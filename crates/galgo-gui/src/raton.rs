//! Ratón hacia los programas del panel (xterm): cuando el programa pide seguimiento (DECSET 1000/1002/1003),
//! los clics, arrastres y la rueda se codifican y van al PTY en vez de a la selección propia de galgo.
//! Shift siempre lo devuelve a galgo: quien llama lo comprueba antes de pedir una secuencia.

use galgo_proto::MouseTracking;

/// Botón de ratón tal como lo codifica xterm (0 izquierdo, 1 central, 2 derecho).
pub const IZQ: u8 = 0;
pub const MEDIO: u8 = 1;
pub const DER: u8 = 2;

/// Lo que ha hecho el ratón sobre la cuadrícula.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesto {
    Pulsa(u8),
    Suelta(u8),
    Arrastra(u8),
    /// Movimiento sin botón (solo con modo 1003).
    Mueve,
    Rueda { arriba: bool },
}

/// Modificadores que xterm suma al código del botón.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Secuencia que el programa espera para `gesto` en la celda (`fila`, `col`), base 0. `None` si su modo no lo pide
/// o si la codificación por defecto no cabe (sin SGR, coordenadas por encima de 94: el protocolo pide bytes >127,
/// que aquí no se pueden mandar como texto).
pub fn secuencia(modo: MouseTracking, sgr: bool, gesto: Gesto, mods: Mods, fila: u16, col: u16) -> Option<String> {
    let (base, suelta) = match gesto {
        Gesto::Pulsa(b) if modo != MouseTracking::None => (b as u16, false),
        Gesto::Suelta(b) if matches!(modo, MouseTracking::PressRelease | MouseTracking::ButtonMotion | MouseTracking::AnyMotion) => {
            // Con SGR se sabe qué botón se suelta; sin SGR el protocolo antiguo lo anuncia como «3».
            (if sgr { b as u16 } else { 3 }, true)
        }
        Gesto::Arrastra(b) if matches!(modo, MouseTracking::ButtonMotion | MouseTracking::AnyMotion) => (b as u16 + 32, false),
        Gesto::Mueve if modo == MouseTracking::AnyMotion => (3 + 32, false),
        Gesto::Rueda { arriba } if modo != MouseTracking::None && modo != MouseTracking::Press => (if arriba { 64 } else { 65 }, false),
        _ => return None,
    };
    // En X10 (modo 9) el protocolo no lleva modificadores.
    let mut codigo = base;
    if modo != MouseTracking::Press {
        codigo += 4 * u16::from(mods.shift) + 8 * u16::from(mods.alt) + 16 * u16::from(mods.ctrl);
    }
    let (f, c) = (fila as u32 + 1, col as u32 + 1);
    if sgr {
        let fin = if suelta { 'm' } else { 'M' };
        return Some(format!("\x1b[<{codigo};{c};{f}{fin}"));
    }
    // Codificación por defecto: `ESC [ M` seguido de tres bytes con +32. Solo cabe en ASCII (<128).
    if codigo + 32 > 127 || c + 32 > 127 || f + 32 > 127 {
        return None;
    }
    let bytes = [b'\x1b', b'[', b'M', (codigo + 32) as u8, (c + 32) as u8, (f + 32) as u8];
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NADA: Mods = Mods { shift: false, alt: false, ctrl: false };

    #[test]
    fn sin_modo_no_se_envia_nada() {
        assert_eq!(secuencia(MouseTracking::None, true, Gesto::Pulsa(IZQ), NADA, 0, 0), None);
        assert_eq!(secuencia(MouseTracking::None, true, Gesto::Rueda { arriba: true }, NADA, 0, 0), None);
    }

    #[test]
    fn clic_izquierdo_y_su_suelta_en_sgr_con_coordenadas_de_uno() {
        let p = secuencia(MouseTracking::PressRelease, true, Gesto::Pulsa(IZQ), NADA, 4, 9);
        assert_eq!(p.as_deref(), Some("\x1b[<0;10;5M"));
        let s = secuencia(MouseTracking::PressRelease, true, Gesto::Suelta(IZQ), NADA, 4, 9);
        assert_eq!(s.as_deref(), Some("\x1b[<0;10;5m"));
    }

    #[test]
    fn modo_press_no_manda_soltar_y_no_lleva_modificadores() {
        assert_eq!(secuencia(MouseTracking::Press, true, Gesto::Suelta(IZQ), NADA, 0, 0), None);
        let con_shift = Mods { shift: true, alt: false, ctrl: false };
        assert_eq!(secuencia(MouseTracking::Press, true, Gesto::Pulsa(IZQ), con_shift, 0, 0).as_deref(), Some("\x1b[<0;1;1M"));
    }

    #[test]
    fn los_botones_centrales_y_derechos_van_con_su_codigo() {
        assert_eq!(secuencia(MouseTracking::PressRelease, true, Gesto::Pulsa(MEDIO), NADA, 0, 0).as_deref(), Some("\x1b[<1;1;1M"));
        assert_eq!(secuencia(MouseTracking::PressRelease, true, Gesto::Pulsa(DER), NADA, 0, 0).as_deref(), Some("\x1b[<2;1;1M"));
    }

    #[test]
    fn arrastre_solo_con_modo_1002_o_1003_y_movimiento_solo_con_1003() {
        let arrastre = Gesto::Arrastra(IZQ);
        assert_eq!(secuencia(MouseTracking::PressRelease, true, arrastre, NADA, 1, 1), None);
        assert_eq!(secuencia(MouseTracking::ButtonMotion, true, arrastre, NADA, 1, 1).as_deref(), Some("\x1b[<32;2;2M"));
        assert_eq!(secuencia(MouseTracking::ButtonMotion, true, Gesto::Mueve, NADA, 1, 1), None);
        assert_eq!(secuencia(MouseTracking::AnyMotion, true, Gesto::Mueve, NADA, 1, 1).as_deref(), Some("\x1b[<35;2;2M"));
    }

    #[test]
    fn rueda_arriba_y_abajo_con_modificadores() {
        let ctrl = Mods { shift: false, alt: false, ctrl: true };
        assert_eq!(secuencia(MouseTracking::AnyMotion, true, Gesto::Rueda { arriba: true }, NADA, 0, 0).as_deref(), Some("\x1b[<64;1;1M"));
        assert_eq!(secuencia(MouseTracking::AnyMotion, true, Gesto::Rueda { arriba: false }, ctrl, 0, 0).as_deref(), Some("\x1b[<81;1;1M"));
    }

    #[test]
    fn codificacion_por_defecto_solo_si_cabe_en_ascii() {
        // Clic izquierdo en fila 0, columna 0: ESC [ M, código 32, columna 33, fila 33.
        assert_eq!(secuencia(MouseTracking::PressRelease, false, Gesto::Pulsa(IZQ), NADA, 0, 0).as_deref(), Some("\x1b[M !!"));
        // Suelta sin SGR: botón 3.
        assert_eq!(secuencia(MouseTracking::PressRelease, false, Gesto::Suelta(IZQ), NADA, 0, 0).as_deref(), Some("\x1b[M#!!"));
        // Columna 95 (base 0) o más: no cabe sin SGR.
        assert_eq!(secuencia(MouseTracking::PressRelease, false, Gesto::Pulsa(IZQ), NADA, 0, 95), None);
    }
}
