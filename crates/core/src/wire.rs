//! Client↔host messages: JSON-RPC 2.0, one message per line. See `docs/architecture.md`.
//!
//! Encoding and decoding are pure functions over bytes. The runtime owns the socket and feeds what it reads into
//! a [`Decoder`]. Which messages get answered, and how, is documented on the `message` items below.

pub mod base64;
mod build;
mod codec;
mod error;
mod message;

pub use build::{BuildId, DigestHasher, ExeDigest};
pub use codec::{Decoder, LineTooLong, MAX_LINE};
pub use error::{WireError, code};
pub use message::{
    CleanupInfo, ClientMessage, ClientNotification, ClientRequest, CloseWorktree, DecodeError,
    Hello, HelloOk, HookFrame, HostMessage, HostNotification, HostResponse, HostResult,
    HostUpgrade, InputKind, LogEntry, Method, Notify, PROTOCOL_VERSION, PairJoin, PairOutcome,
    PairStart, PairStartOk, PairWait, PeerInfo, PeerList, PeerListOk, PeerRemove, ProfileInfo,
    PtyOutput, RawResponse, Request, RequestError, SessionClose, SessionInput, SessionNew,
    SessionNewOk, SessionRename, SessionResize, SessionResume, Subscribe, SubscribeFrom, Tunnel,
    UpgradeAnswer, ViewSize, WorkspaceSelect, WorktreeCheck, WorktreeCleanup, WorktreeCleanupOk,
    WorktreeInfo, WorktreeList, WorktreeListOk, WorktreeRemove, WorktreeRemoveOk, WorktreeStatusOk,
    decode_client_message, decode_host_message, encode_client_notification,
    encode_host_notification, encode_request, encode_response, negotiate,
};
