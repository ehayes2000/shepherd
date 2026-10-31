//! Where the host runtime gets time and new ids. Not a core port: core never calls it. The runtime asks it, then
//! hands the values to core in events and stamps.

use std::time::Instant;

use shepherd_core::acpp::{EntryId, Timestamp};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{PairToken, TOKEN_LEN};

pub trait Env {
    /// Wall-clock time, for envelope `ts` and event `at`.
    fn now(&mut self) -> Timestamp;
    /// Monotonic time, for deadlines (the sync-update timeout, the shutdown bound).
    fn instant(&mut self) -> Instant;
    /// A fresh session id. Must match `[a-z0-9_-]{1,64}`.
    fn new_session_id(&mut self) -> SessionId;
    /// A fresh, globally unique envelope id.
    fn new_entry_id(&mut self) -> EntryId;
    /// A fresh pairing token: secret, from OS randomness. Fails only if the OS has none to give.
    fn pair_token(&mut self) -> Result<PairToken, String>;
}

/// The real clock and ULIDs.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealEnv;

impl Env for RealEnv {
    /// RFC 3339 UTC with milliseconds, e.g. `2026-10-29T12:00:00.123Z`.
    fn now(&mut self) -> Timestamp {
        let text = format!("{:.3}", jiff::Timestamp::now());
        Timestamp::parse(text)
            .unwrap_or_else(|e| unreachable!("jiff formatted an unparseable timestamp: {e}"))
    }

    fn instant(&mut self) -> Instant {
        Instant::now()
    }

    /// `s_<lowercase ulid>`.
    fn new_session_id(&mut self) -> SessionId {
        SessionId::new(format!(
            "s_{}",
            ulid::Ulid::new().to_string().to_lowercase()
        ))
    }

    fn new_entry_id(&mut self) -> EntryId {
        EntryId::new(ulid::Ulid::new().to_string())
    }

    /// 16 bytes from the OS.
    fn pair_token(&mut self) -> Result<PairToken, String> {
        let mut bytes = [0; TOKEN_LEN];
        getrandom::fill(&mut bytes).map_err(|e| format!("the OS gave no randomness: {e}"))?;
        Ok(PairToken::new(bytes))
    }
}

#[cfg(test)]
mod tests {
    use shepherd_core::ports::LogId;

    use super::*;

    #[test]
    fn session_ids_are_s_and_a_lowercase_ulid_that_can_name_a_log() {
        let id = RealEnv.new_session_id();
        let ulid = id.as_str().strip_prefix("s_").expect("s_ prefix");
        assert_eq!(ulid.len(), 26);
        assert!(
            ulid.chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        );
        assert_eq!(LogId::Session(id).validate(), Ok(()));
    }

    #[test]
    fn pairing_tokens_differ() {
        let mut env = RealEnv;
        assert_ne!(env.pair_token(), env.pair_token());
        assert!(env.pair_token().is_ok());
    }

    #[test]
    fn session_ids_are_unique() {
        let mut env = RealEnv;
        assert_ne!(env.new_session_id(), env.new_session_id());
        assert_ne!(env.new_entry_id(), env.new_entry_id());
    }

    #[test]
    fn timestamps_are_rfc3339_utc_with_milliseconds() {
        let ts = RealEnv.now();
        let s = ts.as_str();
        // 2026-10-29T12:00:00.123Z
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(&s[19..20], ".", "{s}");
        assert!(s[20..23].chars().all(|c| c.is_ascii_digit()), "{s}");
    }
}
