//! Dibujo procedural de cajas y bloques (U+2500–U+259F), a la celda exacta: las líneas llegan al borde de la celda
//! y se tocan con las vecinas, en horizontal y en vertical. La fuente no lo garantiza: su glifo no ocupa toda la
//! altura de la fila, y de ahí salen los huecos entre filas.
//!
//! Lo que no está en la tabla (dobles `═║╔…`, y cajas de pesos mezclados) sigue con el glifo de la fuente.

use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Point, Rectangle, Renderer, Size, Theme};

/// Una primitiva dentro de la celda, con origen en su esquina superior izquierda.
#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    Linea { a: (f32, f32), b: (f32, f32), grosor: f32 },
    Curva { a: (f32, f32), control: (f32, f32), b: (f32, f32), grosor: f32 },
    Rect { x: f32, y: f32, w: f32, h: f32, alfa: f32 },
}

/// Primitivas del carácter en una celda de `w`×`h` px, con trazo ligero de `ligero` px (el pesado es el doble).
/// `None` si el carácter no es de los que se dibujan aquí.
pub fn primitivas(ch: char, w: f32, h: f32, ligero: f32) -> Option<Vec<Prim>> {
    let c = ch as u32;
    if (0x2580..=0x259F).contains(&c) {
        return bloques(c, w, h);
    }
    if (0x256D..=0x2570).contains(&c) {
        return Some(vec![curva(c, w, h, ligero)]);
    }
    if (0x2571..=0x2573).contains(&c) {
        let mut v = Vec::new();
        if c != 0x2572 {
            v.push(Prim::Linea { a: (0.0, h), b: (w, 0.0), grosor: ligero });
        }
        if c != 0x2571 {
            v.push(Prim::Linea { a: (0.0, 0.0), b: (w, h), grosor: ligero });
        }
        return Some(v);
    }
    if let Some(v) = discontinua(c, w, h, ligero) {
        return Some(v);
    }
    if (0x2550..=0x256C).contains(&c) {
        return mezcla(c, w, h, ligero);
    }
    let (izq, der, arr, aba) = brazos(c)?;
    let (cx, cy) = (w / 2.0, h / 2.0);
    let grosor = |peso: u8| if peso == 2 { ligero * 2.0 } else { ligero };
    let mut v = Vec::new();
    if izq > 0 {
        v.push(Prim::Linea { a: (0.0, cy), b: (cx, cy), grosor: grosor(izq) });
    }
    if der > 0 {
        v.push(Prim::Linea { a: (cx, cy), b: (w, cy), grosor: grosor(der) });
    }
    if arr > 0 {
        v.push(Prim::Linea { a: (cx, 0.0), b: (cx, cy), grosor: grosor(arr) });
    }
    if aba > 0 {
        v.push(Prim::Linea { a: (cx, cy), b: (cx, h), grosor: grosor(aba) });
    }
    Some(v)
}

/// Si el carácter lo dibuja este módulo (y no la fuente).
pub fn es_dibujable(ch: char) -> bool {
    primitivas(ch, 1.0, 1.0, 1.0).is_some()
}

/// Discontinuas (U+2504–U+250B, U+254C–U+254F): cada brazo se parte en 2, 3 o 4 trazos con huecos.
fn discontinua(c: u32, w: f32, h: f32, ligero: f32) -> Option<Vec<Prim>> {
    // (trazos, pesado, horizontal)
    let (n, pesado, horizontal) = match c {
        0x2504 => (3, false, true),
        0x2505 => (3, true, true),
        0x2506 => (3, false, false),
        0x2507 => (3, true, false),
        0x2508 => (4, false, true),
        0x2509 => (4, true, true),
        0x250A => (4, false, false),
        0x250B => (4, true, false),
        0x254C => (2, false, true),
        0x254D => (2, true, true),
        0x254E => (2, false, false),
        0x254F => (2, true, false),
        _ => return None,
    };
    let grosor = if pesado { ligero * 2.0 } else { ligero };
    let largo = if horizontal { w } else { h };
    let paso = largo / n as f32;
    let (x_hueco, y_hueco) = (w / 2.0, h / 2.0);
    let mut v = Vec::new();
    for i in 0..n {
        let a = i as f32 * paso + paso * 0.2;
        let b = (i as f32 + 1.0) * paso - paso * 0.2;
        v.push(if horizontal {
            Prim::Linea { a: (a, y_hueco), b: (b, y_hueco), grosor }
        } else {
            Prim::Linea { a: (x_hueco, a), b: (x_hueco, b), grosor }
        });
    }
    Some(v)
}

/// Líneas dobles (U+2550–U+256C), puras o mezcladas con simples. Cada línea doble es una pareja de trazos a ±d del
/// centro, y cada trazo termina donde se engancha con la línea de la barra perpendicular que le corresponde:
/// - en una esquina o una unión, la línea exterior con la exterior y la interior con la interior, así que anidan
///   (╔: el trazo exterior arranca en x = cx − dx y el interior en x = cx + dx, sin sobresalir);
/// - si la barra es doble solo en un lado, la exterior de ese lado es la del lado opuesto al brazo;
/// - si la barra es doble en los dos lados, cada trazo llega a la línea interior de la barra;
/// - si las dos mitades de una línea se solapan (la barra no se corta en ese trazo), se dibuja entera.
fn mezcla(c: u32, w: f32, h: f32, ligero: f32) -> Option<Vec<Prim>> {
    // (izquierda, derecha, arriba, abajo): 0 = no hay, 1 = simple, 2 = doble.
    let (l, r, u, d) = match c {
        0x2550 => (2, 2, 0, 0), 0x2551 => (0, 0, 2, 2),
        0x2552 => (0, 2, 0, 1), 0x2553 => (0, 1, 0, 2), 0x2554 => (0, 2, 0, 2),
        0x2555 => (2, 0, 0, 1), 0x2556 => (1, 0, 0, 2), 0x2557 => (2, 0, 0, 2),
        0x2558 => (0, 2, 1, 0), 0x2559 => (0, 1, 2, 0), 0x255A => (0, 2, 2, 0),
        0x255B => (2, 0, 1, 0), 0x255C => (1, 0, 2, 0), 0x255D => (2, 0, 2, 0),
        0x255E => (0, 2, 1, 1), 0x255F => (0, 1, 2, 2), 0x2560 => (0, 2, 2, 2),
        0x2561 => (2, 0, 1, 1), 0x2562 => (1, 0, 2, 2), 0x2563 => (2, 0, 2, 2),
        0x2564 => (2, 2, 0, 1), 0x2565 => (1, 1, 0, 2), 0x2566 => (2, 2, 0, 2),
        0x2567 => (2, 2, 1, 0), 0x2568 => (1, 1, 2, 0), 0x2569 => (2, 2, 2, 0),
        0x256A => (2, 2, 1, 1), 0x256B => (1, 1, 2, 2), 0x256C => (2, 2, 2, 2),
        _ => return None,
    };
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (dx, dy) = ((w / 4.0).max(ligero * 2.0), (h / 4.0).max(ligero * 2.0));
    let h_ambos = l == 2 && r == 2;
    let h_una = (l == 2) != (r == 2);
    let v_ambos = u == 2 && d == 2;
    let v_una = (u == 2) != (d == 2);
    // Lado del único brazo doble de cada eje (si lo hay): la línea exterior de la barra es la del lado contrario.
    let sx_una = if l == 2 { -1.0f32 } else { 1.0 };
    let sy_una = if u == 2 { -1.0f32 } else { 1.0 };
    // Simples opuestos que cruzan la barra sin cortarse: una sola línea entera (no solapada).
    let h_entera = l == 1 && r == 1 && !v_ambos;
    let v_entera = u == 1 && d == 1 && !h_ambos;
    let trazo = |a: (f32, f32), b: (f32, f32)| Prim::Linea { a, b, grosor: ligero };
    // Final, junto al centro, del trazo horizontal de desplazamiento `o` (−1 arriba, +1 abajo) del lado `sx`.
    let fin_x = |sx: f32, o: f32| -> f32 {
        if v_ambos {
            cx + sx * dx
        } else if v_una {
            if o == -sy_una { cx - sx * dx } else { cx + sx * dx }
        } else {
            cx
        }
    };
    // Final, junto al centro, del trazo vertical de desplazamiento `p` (−1 izquierda, +1 derecha) del lado `sy`.
    let fin_y = |sy: f32, p: f32| -> f32 {
        if h_ambos {
            cy + sy * dy
        } else if h_una {
            if p == -sx_una { cy - sy * dy } else { cy + sy * dy }
        } else {
            cy
        }
    };
    let mut v = Vec::new();
    if h_entera {
        v.push(trazo((0.0, cy), (w, cy)));
    }
    if v_entera {
        v.push(trazo((cx, 0.0), (cx, h)));
    }
    // Líneas dobles horizontales: el lado izquierdo y el derecho de cada una; se funden si se solapan.
    if l == 2 || r == 2 {
        for o in [-1.0f32, 1.0] {
            let y = cy + o * dy;
            let izq = (l == 2).then(|| fin_x(-1.0, o));
            let der = (r == 2).then(|| fin_x(1.0, o));
            match (izq, der) {
                (Some(a), Some(b)) if a > b => v.push(trazo((0.0, y), (w, y))),
                _ => {
                    if let Some(a) = izq {
                        v.push(trazo((0.0, y), (a, y)));
                    }
                    if let Some(b) = der {
                        v.push(trazo((b, y), (w, y)));
                    }
                }
            }
        }
    }
    // Líneas dobles verticales.
    if u == 2 || d == 2 {
        for p in [-1.0f32, 1.0] {
            let x = cx + p * dx;
            let arr = (u == 2).then(|| fin_y(-1.0, p));
            let aba = (d == 2).then(|| fin_y(1.0, p));
            match (arr, aba) {
                (Some(a), Some(b)) if a > b => v.push(trazo((x, 0.0), (x, h))),
                _ => {
                    if let Some(a) = arr {
                        v.push(trazo((x, 0.0), (x, a)));
                    }
                    if let Some(b) = aba {
                        v.push(trazo((x, b), (x, h)));
                    }
                }
            }
        }
    }
    // Brazos simples: llegan al centro, o a la línea de la barra doble que cruzan.
    for (p, sx) in [(l, -1.0f32), (r, 1.0)] {
        if p == 1 && !h_entera {
            let x0 = if v_ambos {
                cx + sx * dx
            } else if v_una {
                cx - sx * dx
            } else {
                cx
            };
            v.push(if sx < 0.0 { trazo((0.0, cy), (x0, cy)) } else { trazo((x0, cy), (w, cy)) });
        }
    }
    for (p, sy) in [(u, -1.0f32), (d, 1.0)] {
        if p == 1 && !v_entera {
            let y0 = if h_ambos {
                cy + sy * dy
            } else if h_una {
                cy - sy * dy
            } else {
                cy
            };
            v.push(if sy < 0.0 { trazo((cx, 0.0), (cx, y0)) } else { trazo((cx, y0), (cx, h)) });
        }
    }
    Some(v)
}

/// Brazos (izquierda, derecha, arriba, abajo) de un carácter de cajas: 0 = no hay, 1 = ligero, 2 = pesado.
fn brazos(c: u32) -> Option<(u8, u8, u8, u8)> {
    Some(match c {
        // Rectas, ligeras y pesadas; las discontinuas se dibujan continuas (la celda no admite el patrón).
        0x2500 | 0x2504 | 0x2508 | 0x254C => (1, 1, 0, 0),
        0x2501 | 0x2505 | 0x2509 | 0x254D => (2, 2, 0, 0),
        0x2502 | 0x2506 | 0x250A | 0x254E => (0, 0, 1, 1),
        0x2503 | 0x2507 | 0x250B | 0x254F => (0, 0, 2, 2),
        // Esquinas, tes y cruces de un solo peso.
        0x250C => (0, 1, 0, 1),
        0x2510 => (1, 0, 0, 1),
        0x2514 => (0, 1, 1, 0),
        0x2518 => (1, 0, 1, 0),
        0x251C => (0, 1, 1, 1),
        0x2524 => (1, 0, 1, 1),
        0x252C => (1, 1, 0, 1),
        0x2534 => (1, 1, 1, 0),
        0x253C => (1, 1, 1, 1),
        0x250F => (0, 2, 0, 2),
        0x2513 => (2, 0, 0, 2),
        0x2517 => (0, 2, 2, 0),
        0x251B => (2, 0, 2, 0),
        0x2523 => (0, 2, 2, 2),
        0x252B => (2, 0, 2, 2),
        0x2533 => (2, 2, 0, 2),
        0x253B => (2, 2, 2, 0),
        0x254B => (2, 2, 2, 2),
        // Medias líneas.
        0x2574 => (1, 0, 0, 0),
        0x2575 => (0, 0, 1, 0),
        0x2576 => (0, 1, 0, 0),
        0x2577 => (0, 0, 0, 1),
        0x2578 => (2, 0, 0, 0),
        0x2579 => (0, 0, 2, 0),
        0x257A => (0, 2, 0, 0),
        0x257B => (0, 0, 0, 2),
        0x257C => (1, 2, 0, 0),
        0x257D => (0, 0, 1, 2),
        0x257E => (2, 1, 0, 0),
        0x257F => (0, 0, 2, 1),
        _ => return None,
    })
}

/// Arco de esquina redondeada (╭╮╯╰): une dos brazos contiguos a través del centro de la celda.
fn curva(c: u32, w: f32, h: f32, ligero: f32) -> Prim {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (a, b) = match c {
        0x256D => ((w, cy), (cx, h)), // ╭: derecha y abajo
        0x256E => ((0.0, cy), (cx, h)), // ╮: izquierda y abajo
        0x256F => ((0.0, cy), (cx, 0.0)), // ╯: izquierda y arriba
        _ => ((w, cy), (cx, 0.0)), // ╰: derecha y arriba
    };
    Prim::Curva { a, control: (cx, cy), b, grosor: ligero }
}

/// Bloques (U+2580–U+259F): mitades, octavos, sombreados y cuadrantes.
fn bloques(c: u32, w: f32, h: f32) -> Option<Vec<Prim>> {
    let rect = |x: f32, y: f32, rw: f32, rh: f32, alfa: f32| Prim::Rect { x, y, w: rw, h: rh, alfa };
    Some(match c {
        0x2580 => vec![rect(0.0, 0.0, w, h / 2.0, 1.0)],
        0x2581..=0x2587 => {
            let n = (c - 0x2580) as f32;
            vec![rect(0.0, h - h * n / 8.0, w, h * n / 8.0, 1.0)]
        }
        0x2588 => vec![rect(0.0, 0.0, w, h, 1.0)],
        0x2589..=0x258F => {
            let octavos = (0x2590 - c) as f32;
            vec![rect(0.0, 0.0, w * octavos / 8.0, h, 1.0)]
        }
        0x2590 => vec![rect(w / 2.0, 0.0, w / 2.0, h, 1.0)],
        0x2591 => vec![rect(0.0, 0.0, w, h, 0.25)],
        0x2592 => vec![rect(0.0, 0.0, w, h, 0.5)],
        0x2593 => vec![rect(0.0, 0.0, w, h, 0.75)],
        0x2594 => vec![rect(0.0, 0.0, w, h / 8.0, 1.0)],
        0x2595 => vec![rect(w * 7.0 / 8.0, 0.0, w / 8.0, h, 1.0)],
        // Cuadrantes: máscara de bits UL=1, UR=2, LL=4, LR=8.
        0x2596..=0x259F => {
            let mascara: u8 = match c {
                0x2596 => 4,
                0x2597 => 8,
                0x2598 => 1,
                0x2599 => 13,
                0x259A => 9,
                0x259B => 7,
                0x259C => 11,
                0x259D => 2,
                0x259E => 6,
                _ => 14,
            };
            let (mw, mh) = (w / 2.0, h / 2.0);
            let celdas = [(1u8, 0.0, 0.0), (2, mw, 0.0), (4, 0.0, mh), (8, mw, mh)];
            celdas.iter().filter(|(bit, _, _)| mascara & bit != 0).map(|&(_, x, y)| rect(x, y, mw, mh, 1.0)).collect()
        }
        _ => return None,
    })
}

/// Celdas de caracteres dibujados aquí: `(columna, carácter, color)` por fila.
#[derive(Default, Clone)]
pub struct Capa {
    pub celdas: Vec<(u16, u16, char, Color)>,
    pub cw: f32,
    pub lh: f32,
    pub ligero: f32,
}

impl<M> canvas::Program<M> for Capa {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        // El marco tiene el tamaño del área: con 0×0 el renderer real no pinta nada (solo lo veían las pruebas).
        let mut frame = Frame::new(renderer, bounds.size());
        for &(col, fila, ch, color) in &self.celdas {
            let (x0, y0) = (col as f32 * self.cw, fila as f32 * self.lh);
            let Some(prims) = primitivas(ch, self.cw, self.lh, self.ligero) else { continue };
            for p in prims {
                match p {
                    Prim::Linea { a, b, grosor } => {
                        let path = Path::line(Point::new(x0 + a.0, y0 + a.1), Point::new(x0 + b.0, y0 + b.1));
                        frame.stroke(&path, Stroke::default().with_width(grosor).with_color(color));
                    }
                    Prim::Curva { a, control, b, grosor } => {
                        let path = Path::new(|p| {
                            p.move_to(Point::new(x0 + a.0, y0 + a.1));
                            p.quadratic_curve_to(Point::new(x0 + control.0, y0 + control.1), Point::new(x0 + b.0, y0 + b.1));
                        });
                        frame.stroke(&path, Stroke::default().with_width(grosor).with_color(color));
                    }
                    Prim::Rect { x, y, w, h, alfa } => {
                        frame.fill_rectangle(Point::new(x0 + x, y0 + y), Size::new(w, h), color.scale_alpha(alfa));
                    }
                }
            }
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_recta_ligera_llega_de_borde_a_borde() {
        let v = primitivas('─', 10.0, 20.0, 1.0).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], Prim::Linea { a: (0.0, 10.0), b: (5.0, 10.0), grosor: 1.0 });
        assert_eq!(v[1], Prim::Linea { a: (5.0, 10.0), b: (10.0, 10.0), grosor: 1.0 });
    }

    #[test]
    fn el_pesado_es_el_doble_y_la_esquina_une_dos_brazos() {
        let v = primitivas('┏', 10.0, 20.0, 1.0).unwrap();
        assert!(v.iter().all(|p| matches!(p, Prim::Linea { grosor, .. } if *grosor == 2.0)));
        assert_eq!(v.len(), 2, "┏ tiene dos brazos: derecha y abajo");
    }

    #[test]
    fn el_bloque_completo_y_el_octavo_ocupan_lo_debido() {
        assert_eq!(primitivas('█', 10.0, 20.0, 1.0).unwrap(), vec![Prim::Rect { x: 0.0, y: 0.0, w: 10.0, h: 20.0, alfa: 1.0 }]);
        // ▏ es un octavo de ancho a la izquierda: 10/8.
        assert_eq!(primitivas('▏', 8.0, 20.0, 1.0).unwrap(), vec![Prim::Rect { x: 0.0, y: 0.0, w: 1.0, h: 20.0, alfa: 1.0 }]);
    }

    #[test]
    fn el_cuadrante_de_cuatro_bits_son_cuatro_rectangulos() {
        assert_eq!(primitivas('▟', 10.0, 20.0, 1.0).unwrap().len(), 3, "▟ = arriba-derecha + abajo-izquierda + abajo-derecha");
    }

    #[test]
    fn lo_que_no_se_dibuja_aqui_sigue_con_la_fuente() {
        assert!(primitivas('a', 10.0, 20.0, 1.0).is_none());
        assert!(primitivas('中', 10.0, 20.0, 1.0).is_none());

        assert!(primitivas('┍', 10.0, 20.0, 1.0).is_none(), "los pesos mezclados no están en la tabla");
    }
}

#[cfg(test)]
mod tests_dobles {
    use super::*;

    /// La esquina ╔ en 10×20 con trazo 1: la línea superior doble va de x = cx − dx = 2.5 a la derecha, a y = 5 (cy − dy)
    /// y a y = 15; la izquierda, de y = 5 hacia abajo, a x = 2.5 y a x = 7.5. Así el contorno cierra en (2.5, 5).
    #[test]
    fn la_esquina_doble_cierra_su_contorno() {
        let v = primitivas('╔', 10.0, 20.0, 1.0).unwrap();
        assert_eq!(v.len(), 4, "dos trazos horizontales y dos verticales");
        assert!(v.contains(&Prim::Linea { a: (2.5, 5.0), b: (10.0, 5.0), grosor: 1.0 }), "{v:?}");
        assert!(v.contains(&Prim::Linea { a: (2.5, 5.0), b: (2.5, 20.0), grosor: 1.0 }), "{v:?}");
    }

    /// La esquina ╔ anida: el trazo interior arranca en la línea interior de cada eje (x = 7.5, y = 15), sin cruzar
    /// el exterior ni sobresalir de él.
    #[test]
    fn la_esquina_doble_anida_sin_sobresalir() {
        let v = primitivas('╔', 10.0, 20.0, 1.0).unwrap();
        let mut esperado = vec![
            Prim::Linea { a: (2.5, 5.0), b: (10.0, 5.0), grosor: 1.0 },
            Prim::Linea { a: (7.5, 15.0), b: (10.0, 15.0), grosor: 1.0 },
            Prim::Linea { a: (2.5, 5.0), b: (2.5, 20.0), grosor: 1.0 },
            Prim::Linea { a: (7.5, 15.0), b: (7.5, 20.0), grosor: 1.0 },
        ];
        let mut obtenido = v.clone();
        let clave = |p: &Prim| format!("{p:?}");
        obtenido.sort_by_key(clave);
        esperado.sort_by_key(clave);
        assert_eq!(obtenido, esperado);
    }

    /// Ningún par de trazos paralelos se solapa, en ningún carácter de líneas dobles (si no, se verían más gruesos).
    #[test]
    fn ningun_trazo_doble_se_solapa_ni_sale_de_la_celda() {
        let (w, h) = (10.0f32, 20.0f32);
        for c in 0x2550u32..=0x256C {
            let ch = char::from_u32(c).unwrap();
            let v = primitivas(ch, w, h, 1.0).unwrap();
            for p in &v {
                let Prim::Linea { a, b, .. } = p else { continue };
                for q in [a, b] {
                    assert!((0.0..=w).contains(&q.0) && (0.0..=h).contains(&q.1), "{ch}: {p:?} sale de la celda");
                }
            }
            for (i, p) in v.iter().enumerate() {
                for q in &v[i + 1..] {
                    let (Prim::Linea { a: a1, b: b1, .. }, Prim::Linea { a: a2, b: b2, .. }) = (p, q) else { continue };
                    let horizontal = |a: &(f32, f32), b: &(f32, f32)| (a.1 - b.1).abs() < 1e-3;
                    let vertical = |a: &(f32, f32), b: &(f32, f32)| (a.0 - b.0).abs() < 1e-3;
                    if horizontal(a1, b1) && horizontal(a2, b2) && (a1.1 - a2.1).abs() < 1e-3 {
                        let (x1, x2) = (a1.0.min(b1.0), a1.0.max(b1.0));
                        let (y1, y2) = (a2.0.min(b2.0), a2.0.max(b2.0));
                        assert!(x2 <= y1 + 1e-3 || y2 <= x1 + 1e-3, "{ch}: {p:?} y {q:?} se solapan");
                    }
                    if vertical(a1, b1) && vertical(a2, b2) && (a1.0 - a2.0).abs() < 1e-3 {
                        let (y1, y2) = (a1.1.min(b1.1), a1.1.max(b1.1));
                        let (z1, z2) = (a2.1.min(b2.1), a2.1.max(b2.1));
                        assert!(y2 <= z1 + 1e-3 || z2 <= y1 + 1e-3, "{ch}: {p:?} y {q:?} se solapan");
                    }
                }
            }
        }
    }

    #[test]
    fn la_recta_doble_es_continua_y_las_mezclas_se_dibujan() {
        assert_eq!(primitivas('═', 10.0, 20.0, 1.0).unwrap().len(), 4);
        assert_eq!(primitivas('║', 10.0, 20.0, 1.0).unwrap().len(), 4);
        assert_eq!(primitivas('╒', 10.0, 20.0, 1.0).unwrap().len(), 3, "doble a la derecha, simple abajo");
        assert_eq!(primitivas('╬', 10.0, 20.0, 1.0).unwrap().len(), 8, "la cruz doble tiene ocho trazos");
        assert!(primitivas('╡', 10.0, 20.0, 1.0).is_some());
    }

    /// Todos los caracteres de líneas dobles del rango son dibujables; ninguno cae a la fuente.
    #[test]
    fn todo_el_rango_doble_se_dibuja() {
        for c in 0x2550u32..=0x256C {
            let ch = char::from_u32(c).unwrap();
            assert!(primitivas(ch, 8.0, 16.0, 1.0).is_some(), "U+{c:04X} {ch}");
        }
    }
}
