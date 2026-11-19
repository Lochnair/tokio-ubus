// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! ubus messages: an 8-byte header (version, type, big-endian sequence and
//! peer) followed by one blob attribute, id 0, whose children are the
//! `UBUS_ATTR_*` attributes.

use crate::blob::{self, Writer};
use crate::error::{DecodeError, Error};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) const HEADER_LEN: usize = 8;
/// `UBUS_MAX_MSGLEN` in OpenWrt's build; ubusd drops clients exceeding it.
pub(crate) const MAX_MSG_LEN: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum MsgType {
    Hello = 0,
    Status = 1,
    Data = 2,
    Ping = 3,
    Lookup = 4,
    Invoke = 5,
    AddObject = 6,
    RemoveObject = 7,
    Subscribe = 8,
    Unsubscribe = 9,
    Notify = 10,
    Monitor = 11,
}

impl MsgType {
    fn from_u8(v: u8) -> Option<Self> {
        use MsgType::*;
        Some(match v {
            0 => Hello,
            1 => Status,
            2 => Data,
            3 => Ping,
            4 => Lookup,
            5 => Invoke,
            6 => AddObject,
            7 => RemoveObject,
            8 => Subscribe,
            9 => Unsubscribe,
            10 => Notify,
            11 => Monitor,
            _ => return None,
        })
    }
}

pub(crate) mod attr {
    pub const STATUS: u8 = 1;
    pub const OBJPATH: u8 = 2;
    pub const OBJID: u8 = 3;
    pub const METHOD: u8 = 4;
    pub const OBJTYPE: u8 = 5;
    pub const SIGNATURE: u8 = 6;
    pub const DATA: u8 = 7;
    pub const NO_REPLY: u8 = 10;
    pub const USER: u8 = 12;
    pub const GROUP: u8 = 13;
}

/// A received message. `ty` is `None` for types this crate does not know.
#[derive(Debug)]
pub(crate) struct Message {
    pub ty: Option<MsgType>,
    pub seq: u16,
    pub peer: u32,
    /// The children of the top-level attribute.
    pub body: Vec<u8>,
}

/// The `UBUS_ATTR_*` attributes of a message. As with libubus's
/// `blob_parse_untrusted`, an attribute of the wrong shape is left out.
#[derive(Debug, Default)]
pub(crate) struct Attrs<'a> {
    pub status: Option<i32>,
    pub objid: Option<u32>,
    pub objtype: Option<u32>,
    /// Raw bytes: ubusd passes on whatever the caller sent.
    pub method: Option<&'a [u8]>,
    pub data: Option<&'a [u8]>,
    pub no_reply: bool,
    pub user: Option<&'a str>,
    pub group: Option<&'a str>,
}

impl Message {
    pub fn attrs(&self) -> Result<Attrs<'_>, DecodeError> {
        let mut attrs = Attrs::default();
        for a in blob::iter(&self.body) {
            let a = a?;
            let u32_of = || <[u8; 4]>::try_from(a.data).ok().map(u32::from_be_bytes);
            let bytes_of = || match a.data.split_last() {
                Some((0, s)) => Some(s),
                _ => None,
            };
            let str_of = || bytes_of().and_then(|s| std::str::from_utf8(s).ok());
            match a.id {
                attr::STATUS => attrs.status = u32_of().map(|v| v as i32),
                attr::OBJID => attrs.objid = u32_of(),
                attr::OBJTYPE => attrs.objtype = u32_of(),
                attr::METHOD => attrs.method = bytes_of(),
                attr::DATA => attrs.data = Some(a.data),
                attr::NO_REPLY => attrs.no_reply = a.data.first().is_some_and(|&b| b != 0),
                attr::USER => attrs.user = str_of(),
                attr::GROUP => attrs.group = str_of(),
                _ => {}
            }
        }
        Ok(attrs)
    }
}

/// Builds an outgoing message.
pub(crate) struct Builder {
    ty: MsgType,
    seq: u16,
    peer: u32,
    body: Writer,
    top: usize,
}

impl Builder {
    pub fn new(ty: MsgType, seq: u16, peer: u32) -> Self {
        let mut body = Writer::new();
        let top = body.start(0, false);
        Self {
            ty,
            seq,
            peer,
            body,
            top,
        }
    }

    pub fn writer(&mut self) -> &mut Writer {
        &mut self.body
    }

    /// The encoded message, or `None` when it exceeds ubusd's limit or
    /// holds something blob encoding cannot represent.
    pub fn finish(mut self) -> Option<Vec<u8>> {
        self.body.end(self.top);
        if self.body.overflowed() {
            return None;
        }
        let body = self.body.into_bytes();
        if body.len() > MAX_MSG_LEN {
            return None;
        }
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.push(0);
        out.push(self.ty as u8);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.peer.to_be_bytes());
        out.extend_from_slice(&body);
        Some(out)
    }
}

/// Splits a byte stream into messages. [`Reader::next`] is cancel safe:
/// bytes read before a cancellation stay buffered.
pub(crate) struct Reader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(4096),
        }
    }

    pub async fn next(&mut self) -> Result<Message, Error> {
        loop {
            if let Some(msg) = self.parse()? {
                return Ok(msg);
            }
            if self.inner.read_buf(&mut self.buf).await? == 0 {
                return Err(Error::Closed);
            }
        }
    }

    fn parse(&mut self) -> Result<Option<Message>, Error> {
        let Some((_, _, raw_len)) = self.buf.get(HEADER_LEN..).and_then(blob::header) else {
            return Ok(None);
        };
        // As in libubus, a bad header leaves no way to resynchronise.
        if self.buf[0] != 0 {
            return Err(Error::Protocol("unsupported message version"));
        }
        if raw_len < blob::HEADER_LEN || blob::pad(raw_len) > MAX_MSG_LEN {
            return Err(Error::Protocol("message length out of bounds"));
        }
        let total = HEADER_LEN + raw_len;
        if self.buf.len() < total {
            self.buf.reserve(total - self.buf.len());
            return Ok(None);
        }
        let msg = Message {
            ty: MsgType::from_u8(self.buf[1]),
            seq: u16::from_be_bytes([self.buf[2], self.buf[3]]),
            peer: u32::from_be_bytes(self.buf[4..8].try_into().unwrap()),
            body: self.buf[HEADER_LEN + blob::HEADER_LEN..total].to_vec(),
        };
        self.buf.drain(..total);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_message_matches_ubusd() {
        // ubus_complete_deferred_request: STATUS then OBJID, both int32.
        let mut b = Builder::new(MsgType::Status, 0x0102, 0x0a0b0c0d);
        b.writer().put_u32(attr::STATUS, 3);
        b.writer().put_u32(attr::OBJID, 0x1122_3344);
        let bytes = b.finish().unwrap();
        assert_eq!(
            bytes,
            [
                0, 1, 0x01, 0x02, 0x0a, 0x0b, 0x0c, 0x0d, // header
                0, 0, 0, 20, // top-level attribute
                0x01, 0, 0, 8, 0, 0, 0, 3, // STATUS
                0x03, 0, 0, 8, 0x11, 0x22, 0x33, 0x44, // OBJID
            ]
        );
    }

    #[tokio::test]
    async fn reader_reassembles_split_messages() {
        let mut b = Builder::new(MsgType::Invoke, 7, 9);
        b.writer().put_u32(attr::OBJID, 5);
        b.writer().put_str(attr::METHOD, "status");
        b.writer().put(attr::DATA, &[]);
        let one = b.finish().unwrap();
        let mut stream = one.clone();
        stream.extend_from_slice(&one);

        let (mut tx, rx) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for chunk in stream.chunks(3) {
                tx.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut reader = Reader::new(rx);
        for _ in 0..2 {
            let msg = reader.next().await.unwrap();
            assert_eq!((msg.ty, msg.seq, msg.peer), (Some(MsgType::Invoke), 7, 9));
            let attrs = msg.attrs().unwrap();
            assert_eq!(attrs.objid, Some(5));
            assert_eq!(attrs.method, Some(&b"status"[..]));
            assert_eq!(attrs.data, Some(&[][..]));
        }
        writer.await.unwrap();
        assert!(matches!(reader.next().await, Err(Error::Closed)));
    }

    #[tokio::test]
    async fn reader_rejects_oversized_messages() {
        let mut bytes = vec![0, 2, 0, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&(MAX_MSG_LEN as u32 + 4).to_be_bytes());
        let mut reader = Reader::new(&bytes[..]);
        assert!(matches!(reader.next().await, Err(Error::Protocol(_))));
    }

    #[test]
    fn oversized_builds_are_refused() {
        let mut b = Builder::new(MsgType::Data, 0, 0);
        b.writer().put(attr::DATA, &vec![0; MAX_MSG_LEN]);
        assert!(b.finish().is_none());
    }

    #[test]
    fn mistyped_attributes_are_ignored() {
        let mut w = Writer::new();
        w.put_u8(attr::OBJID, 1); // OBJID must be 4 bytes
        w.put(attr::METHOD, b"no-nul");
        let msg = Message {
            ty: Some(MsgType::Invoke),
            seq: 0,
            peer: 0,
            body: w.into_bytes(),
        };
        let attrs = msg.attrs().unwrap();
        assert_eq!(attrs.objid, None);
        assert_eq!(attrs.method, None);
    }
}
