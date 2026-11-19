// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! Publishes a `demo` object exercising each value type, for trying the crate
//! against a real ubusd: `cargo run --example demo [socket [second-name]]`.
//! The second object is `demo.second` unless named.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_ubus::{ArgType, Object, Server, Status, Table, Value};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    simple_logger();
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| tokio_ubus::DEFAULT_SOCKET.into());
    let second = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "demo.second".into());
    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);

    let demo = Object::new("demo")
        .method("status", move |_| {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            async move {
                Ok(Some(
                    Table::new()
                        .with("calls", n as i64)
                        .with("version", env!("CARGO_PKG_VERSION"))
                        .with("dry_run", false)
                        .with("ratio", 0.25)
                        .with("small", -7i16)
                        .with("unset", Value::Null)
                        .with(
                            "download",
                            Table::new()
                                .with("interface", "ifb4wan")
                                .with("rate_kbps", 41200i32),
                        )
                        .with(
                            "reflectors",
                            vec![Value::from("1.1.1.1"), Value::from("9.9.9.9")],
                        ),
                ))
            }
        })
        .method_with_args(
            "echo",
            [("message", ArgType::String), ("count", ArgType::Int32)],
            |req| async move { Ok(Some(req.args.with("user", req.user))) },
        )
        .method_with_args("sleep", [("ms", ArgType::Int32)], |req| async move {
            let ms = req.args.get("ms").and_then(Value::as_i64).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(ms.max(0) as u64)).await;
            Ok(Some(Table::new().with("slept_ms", ms)))
        })
        .method("deny", |_| async { Err(Status::PERMISSION_DENIED) })
        .method("nothing", |_| async { Ok(None) })
        .method("big", |_| async {
            Ok(Some(Table::new().with("blob", "x".repeat(512 * 1024))))
        });

    Server::with_socket(socket)
        .object(demo)
        .object(Object::new(second).method("ping", |_| async {
            Ok(Some(Table::new().with("pong", true)))
        }))
        .run()
        .await
}

/// Prints log records to stderr without pulling in a logger crate.
fn simple_logger() {
    struct Stderr;
    impl log::Log for Stderr {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            eprintln!("demo: {} {}", record.level(), record.args());
        }
        fn flush(&self) {}
    }
    log::set_logger(&Stderr).unwrap();
    log::set_max_level(log::LevelFilter::Info);
}
