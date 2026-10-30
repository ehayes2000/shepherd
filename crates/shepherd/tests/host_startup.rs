//! `shepherd host` startup and shutdown on the real adapters, in a private tempdir (never `~/.shepherd`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use shepherd::host::{self, HostStartError, LOG_FILE, RuntimeEvent, SOCKET_FILE};
use shepherd_adapters::JsonlEventLog;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn open(root: &Path) -> Result<host::RealHost, HostStartError> {
    host::open(root, PathBuf::from("/"), Some("/bin/sh".into()))
}

#[test]
fn the_root_is_created_private() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let _host = open(&root).unwrap();
    assert_eq!(mode(&root), 0o700);
}

#[test]
fn a_root_others_can_read_is_refused_with_the_fix_and_left_alone() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

    let err = open(&root).err().expect("refused");
    assert_eq!(
        err,
        HostStartError::InsecureRoot {
            path: root.clone(),
            mode: 0o755
        }
    );
    assert!(
        err.to_string()
            .contains(&format!("chmod 700 {}", root.display())),
        "{err}"
    );
    assert_eq!(mode(&root), 0o755);
    assert!(!root.join(SOCKET_FILE).exists());
}

#[test]
fn a_second_host_fails_on_the_log_lock_before_it_touches_the_socket() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    // A running host: it holds the log lock. Its socket file is one nobody listens on right now, which a bind
    // would take for stale and replace.
    let _first = JsonlEventLog::open(&root).unwrap();
    let socket = root.join(SOCKET_FILE);
    drop(UnixListener::bind(&socket).unwrap());
    let inode = fs::symlink_metadata(&socket).unwrap().ino();

    let err = open(&root).err().expect("refused");
    assert_eq!(err, HostStartError::AlreadyRunning(root.clone()));
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), inode);
}

#[test]
fn a_second_host_next_to_a_live_one_is_already_running() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let _first = open(&root).unwrap();
    assert_eq!(
        open(&root).err(),
        Some(HostStartError::AlreadyRunning(root.clone()))
    );
}

#[test]
fn the_host_log_file_is_private_and_receives_tracing_output() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let file = host::open_log_file(&root).unwrap();
    assert_eq!(mode(&root.join(LOG_FILE)), 0o600);

    tracing::subscriber::with_default(host::file_subscriber(file), || {
        tracing::error!("something went wrong");
    });
    let written = fs::read_to_string(root.join(LOG_FILE)).unwrap();
    assert!(written.contains("something went wrong"), "{written}");
}

/// Writes `text` to `path`, private like everything under the root.
fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
}

#[test]
fn a_bad_config_names_the_config_file() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    let config = root.join(host::CONFIG_FILE);
    write_private(&config, "[agents.x]\nkind = \"robot\"\ncommand = \"x\"\n");

    let err = open(&root).err().expect("refused");
    assert!(
        matches!(err, HostStartError::Config { ref path, .. } if *path == config),
        "{err:?}"
    );
    let message = err.to_string();
    assert!(
        message.starts_with(&config.display().to_string()),
        "{message}"
    );
    assert_eq!(message.matches("config").count(), 1, "{message}");
}

#[test]
fn a_corrupt_log_names_the_file_and_what_to_do() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    drop(JsonlEventLog::open(&root).unwrap());
    let log = root.join("sessions").join("s_x.jsonl");
    write_private(&log, "this is not an envelope\n");

    let err = open(&root).err().expect("refused");
    assert!(
        matches!(err, HostStartError::CorruptLog { ref path, line: 1, .. } if *path == log),
        "{err:?}"
    );
    let message = err.to_string();
    assert!(message.contains(&log.display().to_string()), "{message}");
    assert!(message.contains("move the file aside"), "{message}");
}

#[test]
fn a_startup_error_is_also_logged() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    host::prepare_root(&root).unwrap();
    write_private(&root.join(host::CONFIG_FILE), "not = [toml");
    let file = host::open_log_file(&root).unwrap();

    let result = tracing::subscriber::with_default(host::file_subscriber(file), || open(&root));
    assert!(result.is_err());
    let logged = fs::read_to_string(root.join(LOG_FILE)).unwrap();
    assert!(logged.contains("host failed to start"), "{logged}");
    assert!(logged.contains(host::CONFIG_FILE), "{logged}");
}

/// The real socket and log: attaching to a host whose history is bigger than the link's queue bound gets all of
/// it, in order, then the ack, with no disconnect. Bytes and entries are counted; nothing is timed.
#[test]
fn a_client_can_attach_to_a_history_bigger_than_the_link_queue() {
    use shepherd_core::acpp::{FrameDraft, Stamp};
    use shepherd_core::ports::{EventLog, Link, LinkEvent, LogId, MAX_QUEUED};
    use shepherd_core::wire::{
        ClientRequest, HostMessage, HostNotification, Request, Subscribe, decode_host_message,
        encode_request,
    };
    use shepherd_testkit::builders::HostDriver;
    use shepherd_testkit::contracts::link::Events;

    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let name = "n".repeat(8 * 1024);
    let mut prev = HostDriver::new();
    prev.new_session("s_a");
    for i in 0..(MAX_QUEUED * 3 / 2) / name.len() {
        prev.rename("s_a", &format!("{i}-{name}"));
    }
    {
        let mut log = JsonlEventLog::open(&root).unwrap();
        for env in prev.log() {
            let draft = FrameDraft::new(
                env.frame().clone(),
                env.source().clone(),
                env.dir().clone(),
                env.session().cloned(),
            );
            let stamp = Stamp {
                id: env.id().clone(),
                ts: env.ts().clone(),
                machine: env.machine().clone(),
            };
            log.append(LogId::of(&draft), draft, stamp).unwrap();
        }
    }
    // Plus the exit the new host records for s_a.
    let expected = prev.log().len() as u64 + 1;

    let mut host = open(&root).unwrap();
    let tx = host.sender();
    let socket = root.join(SOCKET_FILE);
    let client = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            use std::io::{BufRead, BufReader, Write};
            // A raw socket that doesn't read until the host has had its chance to flood it.
            let mut slow = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            let subscribe = encode_request(&Request {
                id: 1.into(),
                body: ClientRequest::Subscribe(Subscribe::default()),
            });
            slow.write_all(&subscribe).unwrap();

            // Round trips on a second conn: the host handles lines in arrival order, so by the time these are
            // answered it has (almost certainly) acted on the subscribe. Without pacing it would have queued the
            // whole history for a peer that isn't reading, and dropped it as a slow consumer.
            let (sink, events) = Events::channel();
            let (mut probe, conn) =
                shepherd_adapters::UnixConnectLink::connect(&socket, sink).unwrap();
            for n in 2..22 {
                let hello = encode_request(&Request {
                    id: n.into(),
                    body: ClientRequest::Hello(shepherd_core::wire::Hello {
                        version: shepherd_core::wire::PROTOCOL_VERSION,
                        machine: shepherd_core::ids::MachineId::new("probe"),
                    }),
                });
                probe
                    .send(conn, hello.strip_suffix(b"\n").unwrap())
                    .unwrap();
                events.wait_for(
                    |e| matches!(e, LinkEvent::Line { .. }),
                    std::time::Duration::from_secs(60),
                );
            }

            let mut seqs = Vec::new();
            let mut bytes = 0;
            let mut reader = BufReader::new(slow);
            loop {
                let mut line = Vec::new();
                let n = reader.read_until(b'\n', &mut line).unwrap();
                assert!(n > 0, "disconnected after {bytes} bytes");
                bytes += n;
                match decode_host_message(line.strip_suffix(b"\n").unwrap()).unwrap() {
                    HostMessage::Notification(HostNotification::LogEntry(e)) => {
                        seqs.push(e.envelope.seq().get());
                    }
                    HostMessage::Response(r) => {
                        assert_eq!(r.id, 1.into());
                        assert!(r.result.is_ok(), "{r:?}");
                        break;
                    }
                    _ => {}
                }
            }
            (seqs, bytes)
        });
        tx.send(RuntimeEvent::Shutdown);
        result
    });
    host.run();
    let (seqs, bytes) = client
        .join()
        .unwrap()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
    assert!(bytes > MAX_QUEUED, "only {bytes} bytes of history");
    assert_eq!(seqs, (1..=expected).collect::<Vec<_>>());
}

#[test]
fn shutdown_stops_the_loop_and_releases_the_log_lock() {
    let tmp = tempdir();
    let root = tmp.path().join("shepherd");
    let mut host = open(&root).unwrap();
    host.sender().send(RuntimeEvent::Shutdown);
    host.run();
    assert!(host.is_finished());
    drop(host);

    assert!(JsonlEventLog::open(&root).is_ok());
}
