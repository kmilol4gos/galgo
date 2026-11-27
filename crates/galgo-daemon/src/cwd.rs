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

/// Nombre del ejecutable de un proceso (`vim`, `claude`, `zsh`…). macOS: `proc_pidpath`; Linux: `/proc/<pid>/comm`.
#[cfg(target_os = "macos")]
pub fn nombre_de_pid(pid: i32) -> Option<String> {
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
    let ruta = std::path::PathBuf::from(String::from_utf8_lossy(&bytes).into_owned());
    ruta.file_name().map(|f| f.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn nombre_de_pid(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
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

/// Lo que anuncia el programa por OSC: su directorio (OSC 7) o su título (OSC 0 y 2).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tipo {
    Cwd,
    Titulo,
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
}

const MAX_CARGA: usize = 4096;

fn cierra(tipo: Tipo, carga: &[u8], out: &mut Anuncios) {
    let texto = String::from_utf8_lossy(carga).into_owned();
    match tipo {
        Tipo::Cwd => {
            if let Some(d) = ruta_de_osc7(&texto) {
                out.cwd = Some(d);
            }
        }
        Tipo::Titulo => {
            let t = texto.trim();
            if !t.is_empty() {
                out.titulo = Some(t.to_string());
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
                        if c.len() < MAX_CARGA {
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
        assert_eq!(o.feed(b"\x1b]8;;file://h/no\x07\x1b]52;c;aGk=\x07"), Anuncios::default());
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
    fn el_cwd_de_un_proceso_es_el_suyo() {
        let esperado = std::env::current_dir().unwrap().canonicalize().unwrap();
        let leido = cwd_de_pid(std::process::id() as i32).expect("cwd del propio proceso");
        assert_eq!(std::path::PathBuf::from(leido).canonicalize().unwrap(), esperado);
        assert_eq!(cwd_de_pid(-1), None);
    }

    #[test]
    fn el_nombre_del_proceso_propio_es_el_del_binario() {
        let nombre = nombre_de_pid(std::process::id() as i32).expect("nombre del propio proceso");
        let esperado = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(nombre, esperado);
    }
}
