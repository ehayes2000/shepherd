use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        None => client(),
        Some("host") => return shepherd::host::main(),
        Some(other) => Err(format!("unknown command `{other}`")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "shepherd: {why}");
            ExitCode::FAILURE
        }
    }
}

fn client() -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let socket = PathBuf::from(home).join(".shepherd").join("host.sock");
    shepherd::client::run(&socket).map_err(|e| e.to_string())
}
