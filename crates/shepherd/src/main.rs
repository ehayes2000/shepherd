use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        None => client(),
        Some("host") => return shepherd::host::main(),
        Some("handoff-check") => handoff_check(),
        Some("hook") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            shepherd::hook::main(&args);
            return ExitCode::SUCCESS;
        }
        Some(command @ ("pair" | "join" | "peers" | "unpair")) => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            shepherd::peers::main(command, &args)
        }
        Some(other) => Err(format!("unknown command `{other}`")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(
                std::io::stderr(),
                "shepherd: {}",
                shepherd::stderr_safe(&why)
            );
            ExitCode::FAILURE
        }
    }
}

fn client() -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let socket = PathBuf::from(home).join(".shepherd").join("host.sock");
    shepherd::client::run(&socket).map_err(|e| e.to_string())
}

/// `shepherd handoff-check`: one JSON line saying which build this is, which handoff formats it can take over from,
/// and why it can't use the host's config (`$HOME/.shepherd/config.toml`) or machine key (`identity.key`, made here if
/// missing), if it can't. The host runs it from its
/// startup path before it re-execs that path to update in place (decision 0018); a binary without it answers
/// "unknown command" and exits 1, which counts as "can't hand off".
fn handoff_check() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let build = shepherd::build::build_id(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let (config_error, identity_error) = match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        None => (Some("$HOME is not set".to_owned()), None),
        Some(home) => {
            let root = PathBuf::from(home).join(".shepherd");
            let shell = std::env::var("SHELL").ok();
            let config = shepherd::host::load_config(&root, shell).err();
            let identity = shepherd::host::check_machine_key(&root).err();
            (
                config.map(|e| e.to_string()),
                identity.map(|e| e.to_string()),
            )
        }
    };
    let check = shepherd_core::host::upgrade::HandoffCheck::of(build, config_error)
        .with_identity_error(identity_error);
    let line = serde_json::to_string(&check).map_err(|e| e.to_string())?;
    writeln!(std::io::stdout(), "{line}").map_err(|e| e.to_string())
}
