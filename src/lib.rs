// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! Publish objects on OpenWrt's ubus from a tokio application, without
//! linking libubus.
//!
//! ```no_run
//! use tokio_ubus::{Object, Server, Table};
//!
//! # async fn example() {
//! let server = Server::new().object(Object::new("example").method("status", |_req| async {
//!     Ok(Some(Table::new().with("up", true)))
//! }));
//! server.run().await;
//! # }
//! ```
//!
//! This implements the object-provider side of the protocol: registering
//! objects and answering calls. It does not make calls, subscribe or send
//! events. Each call gets one reply, data with success or a failure status,
//! where libubus can also stream several data messages first. File
//! descriptors are neither received with a call nor sent with a reply.

mod blob;
mod blobmsg;
mod error;
mod object;
mod server;
mod wire;

pub use blobmsg::{ArgType, BlobString, Table, Value};
pub use error::{DecodeError, Error, Status};
pub use object::{Object, Reply, Request};
pub use server::{DEFAULT_SOCKET, Server};
