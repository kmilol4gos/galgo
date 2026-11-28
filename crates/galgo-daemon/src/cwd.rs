//! Directorio de trabajo «vivo» de un panel: el del proceso en primer plano del pty, y OSC 7 cuando el shell lo emite.

/// Directorio de trabajo actual de un proceso. macOS: `proc_pidinfo` (PROC_PIDVNODEPATHINFO); Linux: `/proc/<pid>/cwd`.
#[cfg(target_os = "macos")]
pub fn cwd_de_pid(pid: i32) -> Option<String> {
    use std::os::raw::{c_int, c_void};
    extern "C" {
        fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
    }
    // struct proc_vnodepathinfo { vnode_info_path pvi_cdir; vnode_info_path pvi_rdir; }
    // vnode_info_path = vnode_info (152 bytes) + char vip_path[MAXPATHLEN] (1024).
    const PROC_PIDVNODEPATHINFO: c_int = 9;
    const VIP_PATH_OFFSET: usize = 152;
    const MAXPATHLEN: usize = 1024;
    let size = 2 * (VIP_PATH_OFFSET + MAXPATHLEN);
    let mut buf = vec![0u8; size];
    let n = unsafe { proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, buf.as_mut_ptr() as *mut c_void, size as c_int) };
    if n < size as c_int {
        return None;
    }
    let ruta = &buf[VIP_PATH_OFFSET..VIP_PATH_OFFSET + MAXPATHLEN];
    let fin = ruta.iter().position(|&b| b == 0).unwrap_or(MAXPATHLEN);
    let texto = String::from_utf8_lossy(&ruta[..fin]).into_owned();
    (!texto.is_empty()).then_some(texto)
}

#[cfg(not(target_os = "macos"))]
pub fn cwd_de_pid(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Ruta completa del ejecutable de un proceso. macOS: `proc_pidpath`; Linux: `/proc/<pid>/exe`.
#[cfg(target_os = "macos")]
pub fn ruta_de_pid(pid: i32) -> Option<String> {
    use std::os::raw::{c_char, c_int, c_void};
    extern "C" {
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }
    // PROC_PIDPATHINFO_MAXSIZE = 4 * MAXPATHLEN.
    let mut buf = vec![0 as c_char; 4096];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let bytes: Vec<u8> = buf[..n as usize].iter().map(|&c| c as u8).collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn ruta_de_pid(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Nombre del ejecutable de un proceso (`vim`, `claude`, `zsh`…).
/// Nombre con el que se muestra un programa. Claude instala su binario como `…/claude/versions/2.1.332`: el nombre
/// de archivo es el número de versión, así que se muestra el agente de la ruta. Otras rutas: su nombre de archivo.
pub fn nombre_visible(ruta: &str) -> Option<String> {
    let base = std::path::Path::new(ruta).file_name()?.to_string_lossy().into_owned();
    let es_version = !base.is_empty() && base.chars().all(|c| c.is_ascii_digit() || c == '.');
    if es_version {
        if let Some(agente) = ruta.split('/').find(|c| matches!(*c, "claude" | "codex")) {
            return Some(agente.to_string());
        }
    }
    Some(base)
}

/// `nombre_visible` del proceso `pid`, o None si no existe.
pub fn nombre_visible_de_pid(pid: i32) -> Option<String> {
    nombre_visible(&ruta_de_pid(pid)?)
}

/// Ruta de una URI `file://host/ruta` (OSC 7), con los `%XX` decodificados. El host se ignora.
pub fn ruta_de_osc7(uri: &str) -> Option<String> {
    let resto = uri.strip_prefix("file://")?;
    let ruta = &resto[resto.find('/')?..];
    let bytes = ruta.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let texto = String::from_utf8_lossy(&out).into_owned();
    (texto.len() > 1).then_some(texto)
}

/// Lo que anuncia el programa por OSC: su directorio (OSC 7), su título (OSC 0 y 2) o un texto para el
/// portapapeles (OSC 52, solo escritura).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tipo {
    Cwd,
    Titulo,
    Portapapeles,
}

/// Busca `ESC ] 7 ; <uri>` (cwd) y `ESC ] 0|2 ; <texto>` (título) en el flujo de salida del pty, terminados en BEL o
/// `ESC \`. Guarda el estado entre lecturas, así que una secuencia partida entre dos trozos también sale.
#[derive(Default)]
pub struct Osc {
    st: Estado,
}

#[derive(Default)]
enum Estado {
    #[default]
    Normal,
    Esc,
    /// Tras `ESC ]`, leyendo el número antes de `;`.
    Numero(Vec<u8>),
    /// OSC 7 o 0/2: leyendo el contenido.
    Carga(Tipo, Vec<u8>),
    /// Otra OSC: se descarta hasta su terminador.
    Otra,
    /// Dentro de una OSC acaba de llegar un ESC: si sigue `\`, termina. Lleva el contenido si era una que importa.
    FinEsc(Option<(Tipo, Vec<u8>)>),
}

/// Lo último que anunció el programa en un trozo de salida.
#[derive(Debug, Default, PartialEq)]
pub struct Anuncios {
    pub cwd: Option<String>,
    pub titulo: Option<String>,
    /// Texto que el programa pide copiar al portapapeles (OSC 52 con datos, nunca una lectura `?`).
    pub portapapeles: Option<String>,
}

const MAX_CARGA: usize = 4096;
/// Tope del base64 de OSC 52 (unos 100 KB de texto). Lo que pase de ahí se descarta entero.
pub const MAX_PORTAPAPELES: usize = 140_000;

/// Decodifica base64 estándar con relleno. `None` si hay un carácter fuera del alfabeto o el largo no cuadra.
pub fn decodificar_base64(entrada: &[u8]) -> Option<Vec<u8>> {
    fn valor(b: u8) -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a') as u32 + 26,
            b'0'..=b'9' => (b - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let sin_relleno = entrada.iter().rposition(|&b| b != b'=').map_or(&entrada[..0], |i| &entrada[..=i]);
    if entrada.len() % 4 != 0 || entrada.len() - sin_relleno.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(sin_relleno.len() * 3 / 4);
    for trozo in sin_relleno.chunks(4) {
        let mut acc = 0u32;
        for &b in trozo {
            acc = (acc << 6) | valor(b)?;
        }
        acc <<= 6 * (4 - trozo.len() as u32);
        let bytes = acc.to_be_bytes();
        match trozo.len() {
            4 => out.extend_from_slice(&bytes[1..4]),
            3 => out.extend_from_slice(&bytes[1..3]),
            2 => out.push(bytes[1]),
            _ => return None,
        }
    }
    Some(out)
}

/// OSC 52: `c;<base64>` (o selección vacía). Una lectura (`?`) o un base64 roto no hacen nada.
fn portapapeles_de(carga: &[u8]) -> Option<String> {
    if carga.len() > MAX_PORTAPAPELES {
        return None;
    }
    let corte = carga.iter().position(|&b| b == b';')?;
    let (seleccion, datos) = (&carga[..corte], &carga[corte + 1..]);
    if !(seleccion.is_empty() || seleccion.contains(&b'c')) || datos == b"?" {
        return None;
    }
    let bytes = decodificar_base64(datos)?;
    String::from_utf8(bytes).ok().filter(|t| !t.is_empty())
}

/// B4: un título no lleva controles ni caracteres de formato que alteren la pantalla (bidi, ancho cero).
pub fn limpiar_titulo(t: &str) -> String {
    t.chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'))
        .collect()
}

/// B4: una ruta anunciada por OSC 7 se guarda solo si es absoluta, sin controles y existe como carpeta.
pub fn ruta_segura(p: &str) -> bool {
    p.starts_with('/') && !p.chars().any(char::is_control) && std::path::Path::new(p).is_dir()
}

fn cierra(tipo: Tipo, carga: &[u8], out: &mut Anuncios) {
    let texto = String::from_utf8_lossy(carga).into_owned();
    match tipo {
        Tipo::Cwd => {
            if let Some(d) = ruta_de_osc7(&texto) {
                out.cwd = Some(d);
            }
        }
        Tipo::Titulo => {
            let t = limpiar_titulo(texto.trim());
            if !t.trim().is_empty() {
                out.titulo = Some(t.trim().to_string());
            }
        }
        Tipo::Portapapeles => {
            if let Some(t) = portapapeles_de(carga) {
                out.portapapeles = Some(t);
            }
        }
    }
}

impl Osc {
    /// Devuelve lo último que se anunció en `bytes`.
    pub fn feed(&mut self, bytes: &[u8]) -> Anuncios {
        let mut out = Anuncios::default();
        for &b in bytes {
            let anterior = std::mem::take(&mut self.st);
            self.st = match anterior {
                Estado::Normal => if b == 0x1b { Estado::Esc } else { Estado::Normal },
                Estado::Esc => match b {
                    b']' => Estado::Numero(Vec::new()),
                    0x1b => Estado::Esc,
                    _ => Estado::Normal,
                },
                Estado::Numero(mut n) => match b {
                    b';' if n == b"7" => Estado::Carga(Tipo::Cwd, Vec::new()),
                    b';' if n == b"0" || n == b"2" => Estado::Carga(Tipo::Titulo, Vec::new()),
                    b';' if n == b"52" => Estado::Carga(Tipo::Portapapeles, Vec::new()),
                    b';' => Estado::Otra,
                    0x07 => Estado::Normal,
                    0x1b => Estado::FinEsc(None),
                    d if d.is_ascii_digit() && n.len() < 4 => {
                        n.push(d);
                        Estado::Numero(n)
                    }
                    _ => Estado::Otra,
                },
                Estado::Carga(tipo, mut c) => match b {
                    0x07 => {
                        cierra(tipo, &c, &mut out);
                        Estado::Normal
                    }
                    0x1b => Estado::FinEsc(Some((tipo, c))),
                    _ => {
                        // OSC 52 admite un poco más de un tope: así se sabe que se pasó y se descarta entero.
                        let tope = if tipo == Tipo::Portapapeles { MAX_PORTAPAPELES + 1 } else { MAX_CARGA };
                        if c.len() < tope {
                            c.push(b);
                        }
                        Estado::Carga(tipo, c)
                    }
                },
                Estado::Otra => match b {
                    0x07 => Estado::Normal,
                    0x1b => Estado::FinEsc(None),
                    _ => Estado::Otra,
                },
                Estado::FinEsc(carga) => {
                    if b == b'\\' {
                        if let Some((tipo, c)) = carga {
                            cierra(tipo, &c, &mut out);
                        }
                        Estado::Normal
                    } else if b == 0x1b {
                        Estado::FinEsc(carga)
                    } else {
                        Estado::Normal
                    }
                }
            };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd(a: &Anuncios) -> Option<&str> {
        a.cwd.as_deref()
    }

    #[test]
    fn osc7_decodifica_la_ruta_e_ignora_el_host() {
        assert_eq!(ruta_de_osc7("file://mac.local/Users/x/Development/nexo").as_deref(), Some("/Users/x/Development/nexo"));
        assert_eq!(ruta_de_osc7("file:///tmp/con%20espacio").as_deref(), Some("/tmp/con espacio"));
        assert_eq!(ruta_de_osc7("http://no/es/osc7"), None);
        assert_eq!(ruta_de_osc7("file://host"), None);
    }

    #[test]
    fn osc7_con_bel_y_con_st() {
        let mut o = Osc::default();
        assert_eq!(cwd(&o.feed(b"hola\x1b]7;file://h/a/b\x07mundo")), Some("/a/b"));
        assert_eq!(cwd(&o.feed(b"\x1b]7;file://h/c\x1b\\")), Some("/c"));
        assert_eq!(o.feed(b"sin secuencias"), Anuncios::default());
    }

    #[test]
    fn osc7_partida_entre_lecturas_tambien_sale() {
        let mut o = Osc::default();
        assert_eq!(o.feed(b"\x1b]7;file://h/Users/x"), Anuncios::default());
        assert_eq!(cwd(&o.feed(b"/nexo\x07")), Some("/Users/x/nexo"));
        assert_eq!(o.feed(b"\x1b"), Anuncios::default());
        assert_eq!(o.feed(b"]7;file://h/tmp\x1b"), Anuncios::default());
        assert_eq!(cwd(&o.feed(b"\\")), Some("/tmp"));
    }

    #[test]
    fn otras_osc_no_cuentan_como_cwd_ni_titulo() {
        let mut o = Osc::default();
        // OSC 8 no cuenta; OSC 52 sí, pero solo como texto para el portapapeles, no como cwd ni título.
        let a = o.feed(b"\x1b]8;;file://h/no\x07\x1b]52;c;aGk=\x07");
        assert_eq!((a.cwd, a.titulo, a.portapapeles.as_deref()), (None, None, Some("hi")));
        assert_eq!(cwd(&o.feed(b"\x1b]7;file://h/si\x07")), Some("/si"));
    }

    #[test]
    fn osc_0_y_2_dan_el_titulo_del_programa() {
        let mut o = Osc::default();
        assert_eq!(o.feed(b"\x1b]0;vim: main.rs\x07").titulo.as_deref(), Some("vim: main.rs"));
        assert_eq!(o.feed(b"\x1b]2;\xe2\x9c\xb3 Claude Code\x1b\\").titulo.as_deref(), Some("✳ Claude Code"));
        assert_eq!(o.feed(b"\x1b]2;   \x07").titulo, None, "un título vacío no cuenta");
    }

    #[test]
    fn titulos_y_rutas_se_sanean() {
        assert_eq!(limpiar_titulo("ab\u{202E}cd\u{7}\u{1b}e\u{200B}f"), "abcdef");
        assert!(ruta_segura(&std::env::temp_dir().display().to_string()));
        assert!(!ruta_segura("relativa/ruta"));
        assert!(!ruta_segura("/no/existe/jamas/xyz"));
        assert!(!ruta_segura("/tmp/con\u{7}control"));
        let mut o = Osc::default();
        assert_eq!(o.feed(b"\x1b]2;vim\xe2\x80\xae\x07").titulo.as_deref(), Some("vim"));
    }

    #[test]
    fn el_cwd_de_un_proceso_es_el_suyo() {
        let esperado = std::env::current_dir().unwrap().canonicalize().unwrap();
        let leido = cwd_de_pid(std::process::id() as i32).expect("cwd del propio proceso");
        assert_eq!(std::path::PathBuf::from(leido).canonicalize().unwrap(), esperado);
        assert_eq!(cwd_de_pid(-1), None);
    }

    #[test]
    fn el_nombre_del_proceso_propio_es_el_del_binario() {
        let nombre = nombre_visible_de_pid(std::process::id() as i32).expect("nombre del propio proceso");
        let esperado = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(nombre, esperado);
    }

    fn portapapeles(a: &Anuncios) -> Option<&str> {
        a.portapapeles.as_deref()
    }

    #[test]
    fn base64_decodifica_con_y_sin_relleno() {
        assert_eq!(decodificar_base64(b"aG9sYQ==").as_deref(), Some(&b"hola"[..]));
        assert_eq!(decodificar_base64(b"aG9sYQ==").map(|v| String::from_utf8(v).unwrap()).as_deref(), Some("hola"));
        assert_eq!(decodificar_base64(b"aG9sYQ").as_deref(), None);
        assert_eq!(decodificar_base64(b"a===").as_deref(), None);
        assert_eq!(decodificar_base64(b"aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decodificar_base64(b"aG9s!w==").as_deref(), None);
        assert_eq!(decodificar_base64(b"").as_deref(), Some(&b""[..]));
    }

    #[test]
    fn osc52_escribe_el_portapapeles_con_bel_y_con_st() {
        let mut o = Osc::default();
        assert_eq!(portapapeles(&o.feed(b"\x1b]52;c;aG9sYQ==\x07")), Some("hola"));
        assert_eq!(portapapeles(&o.feed(b"\x1b]52;c;bXVuZG8=\x1b\\")), Some("mundo"));
        assert_eq!(portapapeles(&o.feed(b"\x1b]52;;aG9sYQ==\x07")), Some("hola"));
    }

    #[test]
    fn osc52_nunca_lee_y_descarta_lo_roto() {
        let mut o = Osc::default();
        assert_eq!(o.feed(b"\x1b]52;c;?\x07"), Anuncios::default());
        assert_eq!(o.feed(b"\x1b]52;c;@@@@\x07"), Anuncios::default());
        assert_eq!(o.feed(b"\x1b]52;p;aG9sYQ==\x07"), Anuncios::default());
        let grande = "A".repeat(MAX_PORTAPAPELES + 8);
        assert_eq!(o.feed(format!("\x1b]52;c;{grande}\x07").as_bytes()), Anuncios::default());
    }

    #[test]
    fn osc52_partida_entre_lecturas() {
        let mut o = Osc::default();
        assert_eq!(o.feed(b"\x1b]52;c;aG9s"), Anuncios::default());
        assert_eq!(portapapeles(&o.feed(b"YQ==\x07")), Some("hola"));
    }

    #[test]
    fn claude_se_muestra_como_claude_y_no_como_su_version() {
        assert_eq!(nombre_visible("/Users/a/.local/share/claude/versions/2.1.332").as_deref(), Some("claude"));
        assert_eq!(nombre_visible("/usr/bin/vim").as_deref(), Some("vim"));
        assert_eq!(nombre_visible("/opt/homebrew/bin/zsh").as_deref(), Some("zsh"));
        // Un programa con un número en el nombre y sin agente en la ruta conserva su nombre.
        assert_eq!(nombre_visible("/opt/tools/2.0").as_deref(), Some("2.0"));
        // Una carpeta llamada claude no cambia el nombre de un programa cualquiera.
        assert_eq!(nombre_visible("/Users/a/claude/bin/htop").as_deref(), Some("htop"));
    }
}
