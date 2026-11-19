// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;

/// Malformed blob or blobmsg data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed blob data: {0}")]
pub struct DecodeError(pub(crate) &'static str);

/// A ubus status code, as carried in `UBUS_ATTR_STATUS`.
///
/// Handlers return one to fail a call; the caller sees it as the exit status
/// of `ubus call`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(pub i32);

impl Status {
    pub const OK: Status = Status(0);
    pub const INVALID_COMMAND: Status = Status(1);
    pub const INVALID_ARGUMENT: Status = Status(2);
    pub const METHOD_NOT_FOUND: Status = Status(3);
    pub const NOT_FOUND: Status = Status(4);
    pub const NO_DATA: Status = Status(5);
    pub const PERMISSION_DENIED: Status = Status(6);
    pub const TIMEOUT: Status = Status(7);
    pub const NOT_SUPPORTED: Status = Status(8);
    pub const UNKNOWN_ERROR: Status = Status(9);
    pub const CONNECTION_FAILED: Status = Status(10);
    pub const NO_MEMORY: Status = Status(11);
    pub const PARSE_ERROR: Status = Status(12);
    pub const SYSTEM_ERROR: Status = Status(13);

    fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0 => "Success",
            1 => "Invalid command",
            2 => "Invalid argument",
            3 => "Method not found",
            4 => "Not found",
            5 => "No response",
            6 => "Permission denied",
            7 => "Request timed out",
            8 => "Operation not supported",
            9 => "Unknown error",
            10 => "Connection failed",
            11 => "Out of memory",
            12 => "Parsing message data failed",
            13 => "System error",
            _ => return None,
        })
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Status({}", self.0)?;
        if let Some(name) = self.name() {
            write!(f, ", {name:?}")?;
        }
        f.write_str(")")
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "status {}", self.0),
        }
    }
}

impl std::error::Error for Status {}

/// Why a connection to ubusd ended.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ubus socket I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("ubusd closed the connection")]
    Closed,
    #[error("ubus protocol violation: {0}")]
    Protocol(&'static str),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}
