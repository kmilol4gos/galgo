//! galgo-mcp: servidor MCP por stdio. Lee solicitudes JSON-RPC por stdin y responde por stdout.
//! Nada más se escribe en stdout; los diagnósticos, si los hubiera, van a stderr.

use galgo_mcp::handle_line;
use galgo_proto::client::request;
use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if let Some(reply) = handle_line(&line, &|req: galgo_proto::Request| request(&req)) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
