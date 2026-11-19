// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! blobmsg: named, typed values on top of blob attributes. Each value is an
//! extended attribute whose id is its type, with a header of a big-endian
//! `u16` name length, the name, a NUL and padding to 4 bytes.

use crate::blob::{self, Attr, Writer};
use crate::error::DecodeError;
use std::borrow::Cow;
use std::fmt;

const TYPE_UNSPEC: u8 = 0;
const TYPE_ARRAY: u8 = 1;
const TYPE_TABLE: u8 = 2;
const TYPE_STRING: u8 = 3;
const TYPE_INT64: u8 = 4;
const TYPE_INT32: u8 = 5;
const TYPE_INT16: u8 = 6;
const TYPE_INT8: u8 = 7;
const TYPE_DOUBLE: u8 = 8;

/// How deeply arrays and tables may nest in received data. Decoding
/// recurses, so without a bound a 1 MiB message of empty arrays nested
/// inside each other would overflow the stack. JSON-C, which the `ubus` CLI
/// parses arguments with, stops at 32.
const MAX_DEPTH: usize = 64;

/// The type of a method argument, advertised in the object's signature and
/// shown by `ubus -v list`. ubusd does not enforce it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArgType {
    Unspec = TYPE_UNSPEC,
    Array = TYPE_ARRAY,
    Table = TYPE_TABLE,
    String = TYPE_STRING,
    Int64 = TYPE_INT64,
    Int32 = TYPE_INT32,
    Int16 = TYPE_INT16,
    /// `BLOBMSG_TYPE_BOOL` is `BLOBMSG_TYPE_INT8`.
    Bool = TYPE_INT8,
    Double = TYPE_DOUBLE,
}

/// A blobmsg value, kept exactly as received: one variant per wire type.
///
/// `BLOBMSG_TYPE_UNSPEC` is read as [`Value::Null`] when empty, as JSON
/// `null` becomes, and as [`Value::Bytes`] otherwise.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    /// Raw bytes in a `BLOBMSG_TYPE_UNSPEC` field.
    Bytes(Vec<u8>),
    /// `BLOBMSG_TYPE_INT8`, which is also `BLOBMSG_TYPE_BOOL`: `ubus call`
    /// prints it as a boolean. `bool` converts to 0 or 1.
    Int8(u8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Double(f64),
    String(BlobString),
    /// Array elements are written without names. Names on received
    /// elements are dropped, as JSON conversion drops them too.
    Array(Vec<Value>),
    Table(Table),
}

impl Value {
    /// A string, if it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        self.as_string().and_then(BlobString::to_str)
    }

    /// A string as received, whatever its encoding.
    pub fn as_string(&self) -> Option<&BlobString> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// An `INT8` read as `blobmsg_get_bool` does: anything nonzero is true.
    pub fn as_bool(&self) -> Option<bool> {
        self.as_u8().map(|v| v != 0)
    }

    pub fn as_u8(&self) -> Option<u8> {
        match *self {
            Value::Int8(v) => Some(v),
            _ => None,
        }
    }

    /// `INT16`, `INT32` or `INT64`, widened. `INT8` is left out, since
    /// it nearly always holds a boolean; see [`Value::as_u8`].
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int16(v) => Some(v.into()),
            Value::Int32(v) => Some(v.into()),
            Value::Int64(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Double(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    fn type_id(&self) -> u8 {
        match self {
            Value::Null | Value::Bytes(_) => TYPE_UNSPEC,
            Value::Int8(_) => TYPE_INT8,
            Value::Int16(_) => TYPE_INT16,
            Value::Int32(_) => TYPE_INT32,
            Value::Int64(_) => TYPE_INT64,
            Value::Double(_) => TYPE_DOUBLE,
            Value::String(_) => TYPE_STRING,
            Value::Array(_) => TYPE_ARRAY,
            Value::Table(_) => TYPE_TABLE,
        }
    }

    pub(crate) fn encode(&self, w: &mut Writer, name: &[u8]) {
        let Ok(name_len) = u16::try_from(name.len()) else {
            w.set_overflowed();
            return;
        };
        let offset = w.start(self.type_id(), true);
        w.extend(&name_len.to_be_bytes());
        w.extend(name);
        w.extend(&[0]);
        w.align();
        match self {
            Value::Null => {}
            Value::Bytes(v) => w.extend(v),
            Value::Int8(v) => w.extend(&[*v]),
            Value::Int16(v) => w.extend(&v.to_be_bytes()),
            Value::Int32(v) => w.extend(&v.to_be_bytes()),
            Value::Int64(v) => w.extend(&v.to_be_bytes()),
            Value::Double(v) => w.extend(&v.to_bits().to_be_bytes()),
            Value::String(v) => {
                w.extend(&v.0);
                w.extend(&[0]);
            }
            Value::Array(items) => {
                for item in items {
                    item.encode(w, b"");
                }
            }
            Value::Table(table) => table.encode_contents(w),
        }
        w.end(offset);
    }

    /// Decodes one blobmsg attribute into its name and value. `depth` counts
    /// the arrays and tables it sits in.
    fn decode(attr: Attr<'_>, depth: usize) -> Result<(&[u8], Value), DecodeError> {
        if !attr.extended {
            return Err(DecodeError("blobmsg attribute without a name header"));
        }
        let data = attr.data;
        let name_len = usize::from(u16::from_be_bytes(
            data.get(..2)
                .ok_or(DecodeError("truncated blobmsg header"))?
                .try_into()
                .unwrap(),
        ));
        let header_len = blob::pad(2 + name_len + 1);
        if data.len() < header_len || data[2 + name_len] != 0 {
            return Err(DecodeError("truncated blobmsg name"));
        }
        // Like libubox, any bytes will do for a name.
        let name = &data[2..2 + name_len];
        let payload = &data[header_len..];
        let fixed = |len: usize| -> Result<&[u8], DecodeError> {
            if payload.len() == len {
                Ok(payload)
            } else {
                Err(DecodeError("blobmsg integer of the wrong size"))
            }
        };
        let value = match attr.id {
            TYPE_UNSPEC if payload.is_empty() => Value::Null,
            TYPE_UNSPEC => Value::Bytes(payload.to_vec()),
            TYPE_INT8 => Value::Int8(fixed(1)?[0]),
            TYPE_INT16 => Value::Int16(i16::from_be_bytes(fixed(2)?.try_into().unwrap())),
            TYPE_INT32 => Value::Int32(i32::from_be_bytes(fixed(4)?.try_into().unwrap())),
            TYPE_INT64 => Value::Int64(i64::from_be_bytes(fixed(8)?.try_into().unwrap())),
            TYPE_DOUBLE => Value::Double(f64::from_bits(u64::from_be_bytes(
                fixed(8)?.try_into().unwrap(),
            ))),
            TYPE_STRING => {
                let Some((0, text)) = payload.split_last() else {
                    return Err(DecodeError("blobmsg string without a NUL terminator"));
                };
                Value::String(BlobString(text.to_vec()))
            }
            TYPE_ARRAY | TYPE_TABLE if depth >= MAX_DEPTH => {
                return Err(DecodeError("blobmsg nested too deeply"));
            }
            TYPE_ARRAY => Value::Array(
                blob::iter(payload)
                    .map(|attr| Ok(Value::decode(attr?, depth + 1)?.1))
                    .collect::<Result<_, DecodeError>>()?,
            ),
            TYPE_TABLE => Value::Table(Table::decode_at(payload, depth + 1)?),
            _ => return Err(DecodeError("unknown blobmsg type")),
        };
        Ok((name, value))
    }
}

macro_rules! from_impls {
    ($($ty:ty => $variant:ident),* $(,)?) => {
        $(impl From<$ty> for Value {
            fn from(v: $ty) -> Self {
                Value::$variant(v.into())
            }
        })*
    };
}

from_impls! {
    u8 => Int8,
    i16 => Int16,
    i32 => Int32,
    i64 => Int64,
    f64 => Double,
    String => String,
    &str => String,
    Table => Table,
    Vec<Value> => Array,
    BlobString => String,
    Vec<u8> => Bytes,
}

// There is deliberately no `From<u32>`: `blobmsg_add_u32` writes `INT32`,
// which `ubus call` prints as signed, while `INT64` holds every `u32` but
// fails a strict `INT32` policy. Convert to `i32` or `i64` to choose.

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Int8(v.into())
    }
}

/// A blobmsg string: the payload before its final NUL terminator. libubox
/// requires neither UTF-8 nor the absence of NULs inside, and SSIDs or file
/// contents may break either, so the bytes are kept as they are and
/// interpreting them is up to the reader.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct BlobString(Vec<u8>);

impl BlobString {
    /// Every byte, embedded NULs included, as `blobmsg_data` exposes them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The bytes a C reader sees through `blobmsg_get_string`: those before
    /// the first NUL.
    pub fn c_bytes(&self) -> &[u8] {
        self.0.split(|&b| b == 0).next().unwrap_or_default()
    }

    /// The text a C reader sees, if it is valid UTF-8.
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(self.c_bytes()).ok()
    }

    /// The text a C reader sees, with invalid UTF-8 replaced.
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.c_bytes())
    }
}

impl fmt::Debug for BlobString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match std::str::from_utf8(&self.0) {
            Ok(s) => fmt::Debug::fmt(s, f),
            Err(_) => write!(f, "b\"{}\"", self.0.escape_ascii()),
        }
    }
}

impl AsRef<[u8]> for BlobString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Compares every byte, so `"a\0b"` is not equal to `"a"`.
impl PartialEq<str> for BlobString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for BlobString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl From<&str> for BlobString {
    fn from(v: &str) -> Self {
        Self(v.into())
    }
}

impl From<String> for BlobString {
    fn from(v: String) -> Self {
        Self(v.into_bytes())
    }
}

impl From<Vec<u8>> for BlobString {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for BlobString {
    fn from(v: &[u8]) -> Self {
        Self(v.into())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// An ordered blobmsg table: the arguments of a call or its reply.
/// Duplicate names are kept, as blobmsg allows them; [`Table::get`] returns
/// the first. Names are [`BlobString`]s, since libubox does not require
/// them to be UTF-8 either.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table(Vec<(BlobString, Value)>);

impl Table {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry, builder style.
    pub fn with(mut self, name: impl Into<BlobString>, value: impl Into<Value>) -> Self {
        self.push(name, value);
        self
    }

    pub fn push(&mut self, name: impl Into<BlobString>, value: impl Into<Value>) {
        self.0.push((name.into(), value.into()));
    }

    /// The first entry whose name has exactly these bytes.
    pub fn get(&self, name: impl AsRef<[u8]>) -> Option<&Value> {
        let name = name.as_ref();
        self.0
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&BlobString, &Value)> {
        self.0.iter().map(|(n, v)| (n, v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Writes the entries without an enclosing table attribute, as the
    /// payload of `UBUS_ATTR_DATA` carries them.
    pub(crate) fn encode_contents(&self, w: &mut Writer) {
        for (name, value) in &self.0 {
            value.encode(w, name.as_bytes());
        }
    }

    /// Reads entries packed without an enclosing table attribute.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Self::decode_at(bytes, 0)
    }

    fn decode_at(bytes: &[u8], depth: usize) -> Result<Self, DecodeError> {
        blob::iter(bytes)
            .map(|attr| {
                let (name, value) = Value::decode(attr?, depth)?;
                Ok((BlobString::from(name), value))
            })
            .collect::<Result<_, _>>()
            .map(Table)
    }
}

impl<K: Into<BlobString>, V: Into<Value>> FromIterator<(K, V)> for Table {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Table(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl IntoIterator for Table {
    type Item = (BlobString, Value);
    type IntoIter = std::vec::IntoIter<(BlobString, Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(table: &Table) -> Vec<u8> {
        let mut w = Writer::new();
        table.encode_contents(&mut w);
        w.into_bytes()
    }

    #[test]
    fn string_matches_blobmsg_add_string() {
        // blobmsg_add_string(&b, "a", "xy"): extended STRING attr, len 4 + 4
        // (namelen + "a\0" + pad) + 3 ("xy\0"), then one pad byte.
        let bytes = encode(&Table::new().with("a", "xy"));
        assert_eq!(bytes, [0x83, 0, 0, 11, 0, 1, b'a', 0, b'x', b'y', 0, 0]);
    }

    #[test]
    fn integers_are_big_endian_and_sized() {
        let bytes = encode(&Table::new().with("n", 0x0102_0304i32));
        assert_eq!(bytes, [0x85, 0, 0, 12, 0, 1, b'n', 0, 1, 2, 3, 4]);
        let bytes = encode(&Table::new().with("b", true));
        assert_eq!(bytes, [0x87, 0, 0, 9, 0, 1, b'b', 0, 1, 0, 0, 0]);
    }

    #[test]
    fn name_padding_follows_name_length() {
        // namelen 2 + "abc\0" = 6, padded to 8.
        let bytes = encode(&Table::new().with("abc", Value::Null));
        assert_eq!(bytes, [0x80, 0, 0, 12, 0, 3, b'a', b'b', b'c', 0, 0, 0]);
    }

    #[test]
    fn nested_values_round_trip() {
        let table = Table::new()
            .with("s", "hello")
            .with("i16", -2i16)
            .with("i32", -3i32)
            .with("i64", i64::MIN)
            .with("d", 1.5f64)
            .with("t", true)
            .with("f", false)
            .with("n", Value::Null)
            .with("raw", vec![0u8, 1, 2, 0xff, 0])
            .with("u8", 200u8)
            .with("latin1", BlobString::from(&b"caf\xe9"[..]))
            .with("", "empty name")
            .with(
                "arr",
                vec![Value::from(1i32), Value::from("two"), Value::Array(vec![])],
            )
            .with(
                "tbl",
                Table::new().with("inner", Table::new().with("deep", 7i64)),
            );
        assert_eq!(Table::decode(&encode(&table)).unwrap(), table);
    }

    #[test]
    fn rejects_malformed_values() {
        // INT32 with a 2-byte payload.
        let bytes = [0x85, 0, 0, 10, 0, 1, b'n', 0, 1, 2, 0, 0];
        assert!(Table::decode(&bytes).is_err());
        // String without a terminator.
        let bytes = [0x83, 0, 0, 10, 0, 1, b'a', 0, b'x', b'y', 0, 0];
        assert!(Table::decode(&bytes).is_err());
        // Not extended.
        let bytes = [0x03, 0, 0, 8, 0, 0, 0, 0];
        assert!(Table::decode(&bytes).is_err());
        // Name length past the attribute.
        let bytes = [0x83, 0, 0, 8, 0, 9, b'a', 0];
        assert!(Table::decode(&bytes).is_err());
    }

    #[test]
    fn int8_keeps_its_value() {
        let bytes = [0x87, 0, 0, 9, 0, 1, b'b', 0, 200, 0, 0, 0];
        let table = Table::decode(&bytes).unwrap();
        let value = table.get("b").unwrap();
        assert_eq!((value.as_u8(), value.as_bool()), (Some(200), Some(true)));
        assert_eq!(encode(&table), bytes);
    }

    #[test]
    fn strings_keep_their_bytes() {
        let bytes = [0x83, 0, 0, 11, 0, 1, b'a', 0, b'x', 0xff, 0, 0];
        let table = Table::decode(&bytes).unwrap();
        let value = table.get("a").unwrap();
        assert_eq!(value.as_string().unwrap().as_bytes(), b"x\xff");
        assert_eq!(value.as_str(), None, "not UTF-8");
        assert_eq!(encode(&table), bytes);
    }

    #[test]
    fn non_utf8_names_are_kept() {
        let bytes = [0x85, 0, 0, 12, 0, 1, 0xe9, 0, 0, 0, 0, 1];
        let table = Table::decode(&bytes).unwrap();
        assert_eq!(table.get([0xe9]), Some(&Value::Int32(1)));
        assert_eq!(encode(&table), bytes);
    }

    #[test]
    fn unspec_payloads_are_kept() {
        // A 3-byte UNSPEC field named "u", as blobmsg_add_field writes it.
        let bytes = [0x80, 0, 0, 11, 0, 1, b'u', 0, 7, 8, 9, 0];
        let table = Table::decode(&bytes).unwrap();
        assert_eq!(
            table.get("u").and_then(Value::as_bytes),
            Some(&[7, 8, 9][..])
        );
    }

    /// `depth` arrays, each holding the next, as one table entry.
    fn nested_arrays(depth: usize) -> Vec<u8> {
        let mut w = Writer::new();
        let offsets: Vec<_> = (0..depth)
            .map(|_| {
                let o = w.start(TYPE_ARRAY, true);
                w.extend(&[0, 0, 0, 0]); // empty name, padded
                o
            })
            .collect();
        for o in offsets.into_iter().rev() {
            w.end(o);
        }
        w.into_bytes()
    }

    #[test]
    fn nesting_depth_is_bounded() {
        assert!(Table::decode(&nested_arrays(MAX_DEPTH)).is_ok());
        assert!(Table::decode(&nested_arrays(MAX_DEPTH + 1)).is_err());
        // Deep enough to overflow the stack if decoding recursed all the way.
        assert!(Table::decode(&nested_arrays(100_000)).is_err());
    }

    #[test]
    fn overlong_names_overflow_instead_of_panicking() {
        let mut w = Writer::new();
        Table::new()
            .with("n".repeat(70_000), 1i32)
            .encode_contents(&mut w);
        assert!(w.overflowed());
    }

    #[test]
    fn embedded_nuls_are_kept_but_text_stops_there() {
        let bytes = [0x83, 0, 0, 12, 0, 1, b'a', 0, b'x', 0, b'y', 0];
        let table = Table::decode(&bytes).unwrap();
        let value = table.get("a").unwrap();
        assert_eq!(value.as_string().unwrap().as_bytes(), b"x\0y");
        assert_eq!(value.as_str(), Some("x"));
        assert_eq!(encode(&table), bytes);
    }
}
