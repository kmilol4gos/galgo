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

/// Busca `ESC ] 7 ; <uri> (BEL | ESC \)` en el flujo de salida del pty. Guarda el estado entre lecturas, así
/// que una secuencia partida entre dos trozos también sale.
#[derive(Default)]
pub struct Osc7 {
    st: Estado,
}

#[derive(Default)]
enum Estado {
    #[default]
    Normal,
    Esc,
    /// Tras `ESC ]`, leyendo el número antes de `;`.
    Numero(Vec<u8>),
    /// OSC 7: leyendo la URI.
    Uri(Vec<u8>),
    /// Otra OSC: se descarta hasta su terminador.
    Otra,
    /// Dentro de una OSC acaba de llegar un ESC: si sigue `\`, termina. Lleva la URI si era OSC 7.
    FinEsc(Option<Vec<u8>>),
}

const MAX_URI: usize = 4096;

impl Osc7 {
    /// Devuelve la última ruta de OSC 7 encontrada en `bytes`, si hubo alguna.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut ultima = None;
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
                    b';' if n == b"7" => Estado::Uri(Vec::new()),
                    b';' => Estado::Otra,
                    0x07 => Estado::Normal,
                    0x1b => Estado::FinEsc(None),
                    d if d.is_ascii_digit() && n.len() < 4 => {
                        n.push(d);
                        Estado::Numero(n)
                    }
                    _ => Estado::Otra,
                },
                Estado::Uri(mut u) => match b {
                    0x07 => {
                        ultima = ruta_de_osc7(&String::from_utf8_lossy(&u)).or(ultima);
                        Estado::Normal
                    }
                    0x1b => Estado::FinEsc(Some(u)),
                    _ => {
                        if u.len() < MAX_URI {
                            u.push(b);
                        }
                        Estado::Uri(u)
                    }
                },
                Estado::Otra => match b {
                    0x07 => Estado::Normal,
                    0x1b => Estado::FinEsc(None),
                    _ => Estado::Otra,
                },
                Estado::FinEsc(uri) => {
                    if b == b'\\' {
                        if let Some(u) = uri {
                            ultima = ruta_de_osc7(&String::from_utf8_lossy(&u)).or(ultima);
                        }
                        Estado::Normal
                    } else if b == 0x1b {
                        Estado::FinEsc(uri)
                    } else {
                        Estado::Normal
                    }
                }
            };
        }
        ultima
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_decodifica_la_ruta_e_ignora_el_host() {
        assert_eq!(ruta_de_osc7("file://mac.local/Users/x/Development/nexo").as_deref(), Some("/Users/x/Development/nexo"));
        assert_eq!(ruta_de_osc7("file:///tmp/con%20espacio").as_deref(), Some("/tmp/con espacio"));
        assert_eq!(ruta_de_osc7("http://no/es/osc7"), None);
        assert_eq!(ruta_de_osc7("file://host"), None);
    }

    #[test]
    fn osc7_con_bel_y_con_st() {
        let mut o = Osc7::default();
        assert_eq!(o.feed(b"hola\x1b]7;file://h/a/b\x07mundo").as_deref(), Some("/a/b"));
        assert_eq!(o.feed(b"\x1b]7;file://h/c\x1b\\").as_deref(), Some("/c"));
        assert_eq!(o.feed(b"sin secuencias"), None);
    }

    #[test]
    fn osc7_partida_entre_lecturas_tambien_sale() {
        let mut o = Osc7::default();
        assert_eq!(o.feed(b"\x1b]7;file://h/Users/x"), None);
        assert_eq!(o.feed(b"/nexo\x07"), Some("/Users/x/nexo".into()));
        assert_eq!(o.feed(b"\x1b"), None);
        assert_eq!(o.feed(b"]7;file://h/tmp\x1b"), None);
        assert_eq!(o.feed(b"\\"), Some("/tmp".into()));
    }

    #[test]
    fn otras_osc_no_cuentan_como_cwd() {
        let mut o = Osc7::default();
        assert_eq!(o.feed(b"\x1b]0;titulo\x07\x1b]8;;file://h/no\x07"), None);
        assert_eq!(o.feed(b"\x1b]7;file://h/si\x07"), Some("/si".into()));
    }

    #[test]
    fn el_cwd_de_un_proceso_es_el_suyo() {
        let esperado = std::env::current_dir().unwrap().canonicalize().unwrap();
        let leido = cwd_de_pid(std::process::id() as i32).expect("cwd del propio proceso");
        assert_eq!(std::path::PathBuf::from(leido).canonicalize().unwrap(), esperado);
        assert_eq!(cwd_de_pid(-1), None);
    }
}
