//! The machine's hostname (`gethostname`), which names the machine when the config doesn't (T29). Core makes it
//! display-safe and decides what to do without one.

/// The hostname as the system reports it, invalid UTF-8 replaced. `None` if it can't be read or is empty.
pub fn hostname() -> Option<String> {
    // Longer than any hostname a system allows (POSIX caps it at `HOST_NAME_MAX`, 255 on Linux and macOS).
    let mut buf = [0u8; 1024];
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes for the duration of the call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    // A name that filled the buffer may not be NUL-terminated: it is used up to the buffer's end.
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).into_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::hostname;

    #[test]
    fn hostname_is_what_the_hostname_command_prints() {
        let out = Command::new("hostname").output().unwrap();
        assert!(out.status.success());
        let expected = String::from_utf8(out.stdout).unwrap();
        assert_eq!(hostname().as_deref(), Some(expected.trim_end()));
    }
}
