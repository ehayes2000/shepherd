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
    ClientMessage, ClientNotification, ClientRequest, CloseWorktree, DecodeError, Hello, HelloOk,
    HookFrame, HostMessage, HostNotification, HostResponse, HostResult, HostUpgrade, InputKind,
    LogEntry, Method, Notify, PROTOCOL_VERSION, ProfileInfo, PtyOutput, RawResponse, Request,
    RequestError, SessionClose, SessionInput, SessionNew, SessionNewOk, SessionRename,
    SessionResize, SessionResume, Subscribe, SubscribeFrom, UpgradeAnswer, WorkspaceSelect,
    WorktreeCheck, WorktreeInfo, WorktreeList, WorktreeListOk, WorktreeStatusOk,
    decode_client_message, decode_host_message, encode_client_notification,
    encode_host_notification, encode_request, encode_response, negotiate,
};
