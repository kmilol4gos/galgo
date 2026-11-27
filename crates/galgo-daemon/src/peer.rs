//! Quién está al otro lado del socket. El remitente de `delegate`, `task done` y los hooks no se declara en la
//! solicitud: se deduce del proceso que conectó. Ese proceso pertenece a un panel si su sesión (`getsid`) es el pid
//! del líder del pty del panel: todo lo que el agente lanza (su shell, la CLI, el MCP) hereda la sesión.

/// Pid del proceso que está al otro extremo de un socket Unix (macOS: `LOCAL_PEERPID`; Linux: `SO_PEERCRED`).
#[cfg(target_os = "macos")]
pub fn pid_del_par(fd: std::os::unix::io::RawFd) -> Option<i32> {
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &mut pid as *mut _ as *mut libc::c_void, &mut len) };
    (r == 0 && pid > 0).then_some(pid)
}

#[cfg(not(target_os = "macos"))]
pub fn pid_del_par(fd: std::os::unix::io::RawFd) -> Option<i32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    (r == 0 && cred.pid > 0).then_some(cred.pid)
}

/// Padre (ppid) de un proceso. macOS: `proc_pidinfo(PROC_PIDTBSDINFO)`; Linux: `/proc/<pid>/stat`.
#[cfg(target_os = "macos")]
pub fn ppid_de(pid: i32) -> Option<i32> {
    use std::os::raw::{c_int, c_void};
    extern "C" {
        fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
    }
    // struct proc_bsdinfo: pbi_flags, pbi_status, pbi_xstatus, pbi_pid, pbi_ppid (uint32 cada uno).
    const PROC_PIDTBSDINFO: c_int = 3;
    const TAMANO: usize = 136;
    let mut buf = [0u8; TAMANO];
    let n = unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, buf.as_mut_ptr() as *mut c_void, TAMANO as c_int) };
    if n < TAMANO as c_int {
        return None;
    }
    let ppid = u32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]) as i32;
    (ppid > 0).then_some(ppid)
}

#[cfg(not(target_os = "macos"))]
pub fn ppid_de(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // El nombre va entre paréntesis y puede tener espacios: el ppid es el cuarto campo tras el último ')'.
    let tras = &stat[stat.rfind(')')? + 2..];
    tras.split_whitespace().nth(1)?.parse::<i32>().ok().filter(|p| *p > 0)
}

/// Los pids de la cadena de ancestros de `pid`, desde el propio proceso hacia arriba (sin el init).
pub fn ascendencia(pid: i32) -> Vec<i32> {
    let mut cadena = vec![pid];
    let mut actual = pid;
    while cadena.len() < 64 {
        match ppid_de(actual) {
            Some(p) if p > 1 && !cadena.contains(&p) => {
                cadena.push(p);
                actual = p;
            }
            _ => break,
        }
    }
    cadena
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_par_de_un_socket_es_este_proceso() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        use std::os::unix::io::AsRawFd;
        assert_eq!(pid_del_par(a.as_raw_fd()), Some(std::process::id() as i32));
    }

    #[test]
    fn el_padre_de_este_proceso_es_el_que_lo_lanzo() {
        let padre = unsafe { libc::getppid() };
        assert_eq!(ppid_de(std::process::id() as i32), Some(padre));
        assert!(ascendencia(std::process::id() as i32).contains(&padre));
    }
}
