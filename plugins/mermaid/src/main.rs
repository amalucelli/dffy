//! `dffy-render-mermaid`, an experimental renderer plugin for dffy: it
//! reads one request on stdin and prints both sides of a Mermaid file as
//! diagrams drawn in text, which dffy diffs block by block. It isn't part
//! of dffy; install it next to it to use it.

mod protocol;

use std::io::{Read as _, Write as _};
use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("dffy-render-mermaid {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut request = Vec::new();
    if std::io::stdin().read_to_end(&mut request).is_err() {
        return ExitCode::FAILURE;
    }
    let (answer, rendered) = protocol::answer(&request);
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(answer.as_bytes())
        .and_then(|()| stdout.flush())
        .is_err()
        || !rendered
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
