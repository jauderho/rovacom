//! rovacom binary (also installed as `novacom`).
#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(rovacom::app::main_entry())
}
