#![no_main]
#![allow(dead_code)]

// Exercise the library's private codec without adding a public fuzz API.
#[path = "../../src/blob.rs"]
mod blob;
#[path = "../../src/blobmsg.rs"]
mod blobmsg;
#[path = "../../src/error.rs"]
mod error;
#[path = "../../src/wire.rs"]
mod wire;

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use blob::Writer;
use blobmsg::Table;
use libfuzzer_sys::fuzz_target;
use wire::Reader;

fn encode(table: &Table) -> Vec<u8> {
    let mut writer = Writer::new();
    table.encode_contents(&mut writer);
    assert!(!writer.overflowed());
    writer.into_bytes()
}

fn check_table(bytes: &[u8]) {
    if let Ok(table) = Table::decode(bytes) {
        let encoded = encode(&table);
        let decoded = Table::decode(&encoded).expect("encoder must produce decodable data");
        // Compare bytes so that NaNs work and inputs may be canonicalized.
        assert_eq!(encode(&decoded), encoded);
    }
}

fuzz_target!(|bytes: &[u8]| {
    check_table(bytes);

    let mut reader = Reader::new(bytes);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        let message = match pin!(reader.next()).poll(&mut context) {
            Poll::Ready(Ok(message)) => message,
            Poll::Ready(Err(_)) => break,
            Poll::Pending => unreachable!("a byte slice is always ready to read"),
        };
        if let Ok(attrs) = message.attrs()
            && let Some(data) = attrs.data
        {
            check_table(data);
        }
    }
});
