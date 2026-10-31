//! Which machine a listed session is on, and what that machine is called (T29).

use shepherd_core::ids::MachineId;
use shepherd_testkit::builders::ClientDriver;

/// The machine label of each listed session, by session name.
fn labels(c: &ClientDriver) -> Vec<(String, String)> {
    let client = c.client();
    client
        .sessions()
        .iter()
        .map(|s| (s.name.clone(), client.machine_label(&s.machine).to_owned()))
        .collect()
}

#[test]
fn the_hello_tells_the_client_what_the_hosts_machine_is_called() {
    let mut c = ClientDriver::new();
    c.connect_named("studio");
    assert_eq!(c.client().host_name(), Some("studio"));
}

#[test]
fn a_session_on_the_hosts_machine_is_labelled_with_its_name() {
    let mut c = ClientDriver::new();
    c.connect_named("studio");
    c.host().new_session("s1");
    c.sync();
    assert_eq!(c.client().sessions()[0].machine, MachineId::new("m_test"));
    assert_eq!(labels(&c), [("shell-1".to_owned(), "studio".to_owned())]);
}

/// A log written before machines had ids (every entry stamped `local`), continued by the same host after the upgrade
/// (entries stamped with its id), plus a session some other machine recorded.
#[test]
fn replaying_a_mixed_log_reads_local_entries_as_the_hosts_machine() {
    let mut c = ClientDriver::new();
    c.connect_named("studio");
    c.host().stamp_machine("local");
    c.host().new_session("old");
    c.host().rename("old", "old");
    c.host().stamp_machine("m_test");
    c.host().new_session("new");
    c.host().rename("new", "new");
    c.host().stamp_machine("m_other");
    c.host().new_session("elsewhere");
    c.host().rename("elsewhere", "elsewhere");
    c.sync();
    let sessions = c.client().sessions();
    let machines: Vec<_> = sessions.iter().map(|s| s.machine.as_str()).collect();
    assert_eq!(machines, ["m_other", "m_test", "m_test"]);
    assert_eq!(
        labels(&c),
        [
            ("elsewhere".to_owned(), "m_other".to_owned()),
            ("new".to_owned(), "studio".to_owned()),
            ("old".to_owned(), "studio".to_owned()),
        ]
    );
}

#[test]
fn a_host_older_than_machine_names_is_labelled_local() {
    let mut c = ClientDriver::new();
    c.connect();
    c.host().new_session("s1");
    c.sync();
    assert_eq!(c.client().host_name(), None);
    assert_eq!(labels(&c), [("shell-1".to_owned(), "local".to_owned())]);
}
