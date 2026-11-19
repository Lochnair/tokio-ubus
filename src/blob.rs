// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! libubox `blob_attr` framing: a big-endian `u32` holding the extended flag,
//! a 7-bit id and a 24-bit length that counts the 4-byte header, followed by
//! the payload. Every attribute starts on a 4-byte boundary, and a nested
//! attribute's length includes its children's padding.

use crate::error::DecodeError;

pub(crate) const HEADER_LEN: usize = 4;
const EXTENDED: u32 = 0x8000_0000;
const ID_MASK: u32 = 0x7f00_0000;
const ID_SHIFT: u32 = 24;
const LEN_MASK: u32 = 0x00ff_ffff;

pub(crate) const fn pad(len: usize) -> usize {
    (len + 3) & !3
}

/// One attribute, borrowed from the buffer it was read from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Attr<'a> {
    pub id: u8,
    pub extended: bool,
    /// The payload, without the header or the trailing padding.
    pub data: &'a [u8],
}

/// Reads an attribute header: `(id, extended, raw length)`.
pub(crate) fn header(bytes: &[u8]) -> Option<(u8, bool, usize)> {
    let word = u32::from_be_bytes(bytes.get(..HEADER_LEN)?.try_into().ok()?);
    Some((
        ((word & ID_MASK) >> ID_SHIFT) as u8,
        word & EXTENDED != 0,
        (word & LEN_MASK) as usize,
    ))
}

/// Iterates over the attributes packed in `buf`, the payload of a nested
/// attribute. Malformed input ends the iteration with an error.
pub(crate) struct Iter<'a> {
    rest: &'a [u8],
}

pub(crate) fn iter(buf: &[u8]) -> Iter<'_> {
    Iter { rest: buf }
}

impl<'a> Iterator for Iter<'a> {
    type Item = Result<Attr<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let Some((id, extended, raw_len)) = header(self.rest) else {
            self.rest = &[];
            return Some(Err(DecodeError("truncated attribute header")));
        };
        if raw_len < HEADER_LEN || raw_len > self.rest.len() {
            self.rest = &[];
            return Some(Err(DecodeError("attribute length out of bounds")));
        }
        let data = &self.rest[HEADER_LEN..raw_len];
        // The last attribute's padding may be cut off; libubox tolerates it.
        self.rest = &self.rest[pad(raw_len).min(self.rest.len())..];
        Some(Ok(Attr { id, extended, data }))
    }
}

/// An append-only blob buffer, the equivalent of libubox's `blob_buf`.
///
/// Content the format cannot represent, such as an attribute over 16 MiB,
/// marks the buffer as overflowed instead of panicking; see
/// [`Writer::overflowed`].
#[derive(Default)]
pub(crate) struct Writer {
    buf: Vec<u8>,
    overflowed: bool,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Whether something written did not fit the format, leaving the
    /// buffer unusable.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn set_overflowed(&mut self) {
        self.overflowed = true;
    }

    /// Starts an attribute; its length is fixed by [`Writer::end`].
    pub fn start(&mut self, id: u8, extended: bool) -> usize {
        debug_assert!(id < 0x80);
        let offset = self.buf.len();
        let flag = if extended { EXTENDED } else { 0 };
        self.buf
            .extend_from_slice(&(flag | (u32::from(id) << ID_SHIFT)).to_be_bytes());
        offset
    }

    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pads the buffer to the next 4-byte boundary with zeroes.
    pub fn align(&mut self) {
        self.buf.resize(pad(self.buf.len()), 0);
    }

    /// Ends the attribute started at `offset`. A leaf's length excludes its
    /// padding; a nested attribute's children are already padded, so its
    /// length covers them in full. Either way the buffer ends aligned.
    pub fn end(&mut self, offset: usize) {
        let raw_len = self.buf.len() - offset;
        if raw_len > LEN_MASK as usize {
            self.overflowed = true;
            self.align();
            return;
        }
        let word = u32::from_be_bytes(self.buf[offset..offset + 4].try_into().unwrap());
        self.buf[offset..offset + 4].copy_from_slice(&(word | raw_len as u32).to_be_bytes());
        self.align();
    }

    pub fn put(&mut self, id: u8, payload: &[u8]) {
        let offset = self.start(id, false);
        self.extend(payload);
        self.end(offset);
    }

    pub fn put_u32(&mut self, id: u8, value: u32) {
        self.put(id, &value.to_be_bytes());
    }

    #[cfg(test)]
    pub fn put_u8(&mut self, id: u8, value: u8) {
        self.put(id, &[value]);
    }

    /// A NUL-terminated string, as `blob_put_string` writes it.
    pub fn put_str(&mut self, id: u8, value: &str) {
        let offset = self.start(id, false);
        self.extend(value.as_bytes());
        self.extend(&[0]);
        self.end(offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_length_excludes_padding() {
        let mut w = Writer::new();
        w.put_str(3, "ab");
        // header(id 3, len 4 + 3) "ab\0" + 1 pad byte
        assert_eq!(w.into_bytes(), [0x03, 0, 0, 7, b'a', b'b', 0, 0]);
    }

    #[test]
    fn nested_length_includes_child_padding() {
        let mut w = Writer::new();
        let outer = w.start(0, false);
        w.put_u8(1, 9);
        w.put_u32(2, 0x0102_0304);
        w.end(outer);
        let bytes = w.into_bytes();
        assert_eq!(&bytes[..4], &[0, 0, 0, 4 + 8 + 8]);
        let attrs: Vec<_> = iter(&bytes[4..]).map(Result::unwrap).collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!((attrs[0].id, attrs[0].data), (1, &[9][..]));
        assert_eq!((attrs[1].id, attrs[1].data), (2, &[1, 2, 3, 4][..]));
    }

    #[test]
    fn extended_flag_round_trips() {
        let mut w = Writer::new();
        let o = w.start(5, true);
        w.end(o);
        let bytes = w.into_bytes();
        assert_eq!(bytes, [0x85, 0, 0, 4]);
        let attr = iter(&bytes).next().unwrap().unwrap();
        assert!(attr.extended);
        assert_eq!(attr.id, 5);
    }

    #[test]
    fn oversized_attributes_overflow_instead_of_panicking() {
        let mut w = Writer::new();
        w.put(1, &vec![0; LEN_MASK as usize]);
        assert!(w.overflowed());
    }

    #[test]
    fn rejects_lengths_past_the_buffer() {
        let bytes = [0x01, 0, 0, 12, 0, 0, 0, 0];
        assert!(iter(&bytes).next().unwrap().is_err());
        let bytes = [0x01, 0, 0, 2];
        assert!(iter(&bytes).next().unwrap().is_err());
    }
}
