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

/// Sesión del proceso `pid` (su `getsid`): el pid del líder de sesión, que para un panel es su shell.
pub fn sesion_de(pid: i32) -> Option<i32> {
    let s = unsafe { libc::getsid(pid) };
    (s > 0).then_some(s)
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
    fn la_sesion_de_este_proceso_existe() {
        assert!(sesion_de(std::process::id() as i32).is_some());
    }
}
