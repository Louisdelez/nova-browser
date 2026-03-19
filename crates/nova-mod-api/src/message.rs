//! Messages exchanged on the IPC bus.

use serde::{Deserialize, Serialize};

use crate::content::TypedData;
use crate::types::ModId;

/// A message sent through the core's IPC bus.
#[derive(Debug)]
pub struct Message {
    /// Unique message ID for tracking/correlation.
    pub id: u64,
    /// Sender mod (or "core" for system messages).
    pub from: ModId,
    /// Destination (resolved by the core, not the sender).
    pub kind: MessageKind,
    /// Payload.
    pub data: TypedData,
}

/// The kind of message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageKind {
    /// A request expecting a response.
    Request,
    /// A response to a previous request.
    Response { request_id: u64 },
    /// A one-way notification (no response expected).
    Notification,
    /// An error response.
    Error { request_id: u64, error: String },
}
