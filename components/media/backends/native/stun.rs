/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! STUN messages (RFC 8489) for the peer connection's own STUN binding (server-reflexive
//! candidates) and its TURN client (turn.rs, RFC 8656): encode and decode, XOR addresses,
//! MESSAGE-INTEGRITY with long-term credentials, FINGERPRINT.
//!
//! str0m's ICE agent does its own connectivity checks; this is only for talking to STUN/TURN
//! servers. Nothing here logs attribute values (USERNAME, keys, NONCE).

// The whole codec, though the client side uses only part of it (the tests use the rest).
#![allow(dead_code)]

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const MAGIC: u32 = 0x2112_A442;
pub const HEADER: usize = 20;

// Methods.
pub const BINDING: u16 = 0x001;
pub const ALLOCATE: u16 = 0x003;
pub const REFRESH: u16 = 0x004;
pub const SEND: u16 = 0x006;
pub const DATA: u16 = 0x007;
pub const CREATE_PERMISSION: u16 = 0x008;
pub const CHANNEL_BIND: u16 = 0x009;

// Attributes.
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
pub const ATTR_USERNAME: u16 = 0x0006;
pub const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
pub const ATTR_ERROR_CODE: u16 = 0x0009;
pub const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
pub const ATTR_LIFETIME: u16 = 0x000D;
pub const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
pub const ATTR_DATA: u16 = 0x0013;
pub const ATTR_REALM: u16 = 0x0014;
pub const ATTR_NONCE: u16 = 0x0015;
pub const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
pub const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const ATTR_SOFTWARE: u16 = 0x8022;
pub const ATTR_FINGERPRINT: u16 = 0x8028;

const FINGERPRINT_XOR: u32 = 0x5354_554E;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Class::Request => 0b00,
            Class::Indication => 0b01,
            Class::Success => 0b10,
            Class::Error => 0b11,
        }
    }
    fn from_bits(b: u16) -> Class {
        match b & 0b11 {
            0b00 => Class::Request,
            0b01 => Class::Indication,
            0b10 => Class::Success,
            _ => Class::Error,
        }
    }
}

/// The 16-bit message type from a method and class (RFC 8489 section 5).
pub fn message_type(method: u16, class: Class) -> u16 {
    let c = class.bits();
    (method & 0x000F) |
        ((method & 0x0070) << 1) |
        ((method & 0x0F80) << 2) |
        ((c & 0b01) << 4) |
        ((c & 0b10) << 7)
}

fn split_type(t: u16) -> (u16, Class) {
    let method = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let class = ((t >> 4) & 0b01) | ((t >> 7) & 0b10);
    (method, Class::from_bits(class))
}

pub type TransId = [u8; 12];

/// A fresh random transaction id.
pub fn new_tid() -> TransId {
    let mut t = [0u8; 12];
    // Not secret, only unpredictable enough that an off-path sender can't answer for the server.
    let _ = aws_lc_rs::rand::fill(&mut t);
    t
}

#[derive(Debug, PartialEq, Eq)]
pub enum StunError {
    NotStun,
    Truncated,
    BadAttribute(u16),
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StunError::NotStun => write!(f, "not a STUN message"),
            StunError::Truncated => write!(f, "truncated STUN message"),
            StunError::BadAttribute(t) => write!(f, "bad STUN attribute {t:#06x}"),
        }
    }
}

/// Does `buf` start with a whole STUN message? (Top two bits zero, the magic cookie, a length that
/// is a multiple of 4 and fits.)
pub fn is_stun(buf: &[u8]) -> bool {
    buf.len() >= HEADER &&
        buf[0] & 0xC0 == 0 &&
        u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) == MAGIC &&
        message_len(buf).is_some_and(|n| n <= buf.len())
}

/// The whole length of the STUN message at the start of `buf` (header included), from its header.
pub fn message_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let n = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    (n % 4 == 0).then_some(HEADER + n)
}

/// A decoded message: its attributes (in order) and where MESSAGE-INTEGRITY and FINGERPRINT sit,
/// for checking them against the raw bytes.
#[derive(Clone)]
pub struct Message {
    pub method: u16,
    pub class: Class,
    pub tid: TransId,
    pub attrs: Vec<(u16, Vec<u8>)>,
    raw: Vec<u8>,
    integrity_at: Option<usize>,
    fingerprint_at: Option<usize>,
}

// By hand: attribute values (USERNAME, NONCE, REALM, the integrity) stay out of logs.
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let types: Vec<String> = self.attrs.iter().map(|(t, _)| format!("{t:#06x}")).collect();
        write!(
            f,
            "StunMessage(method {:#05x} {:?}, attrs [{}])",
            self.method,
            self.class,
            types.join(" ")
        )
    }
}

impl Message {
    pub fn decode(buf: &[u8]) -> Result<Message, StunError> {
        if !is_stun(buf) {
            return Err(StunError::NotStun);
        }
        let len = message_len(buf).ok_or(StunError::NotStun)?;
        let buf = &buf[..len];
        let (method, class) = split_type(u16::from_be_bytes([buf[0], buf[1]]));
        let mut tid = [0u8; 12];
        tid.copy_from_slice(&buf[8..20]);
        let mut attrs = vec![];
        let mut integrity_at = None;
        let mut fingerprint_at = None;
        let mut i = HEADER;
        while i < len {
            if i + 4 > len {
                return Err(StunError::Truncated);
            }
            let t = u16::from_be_bytes([buf[i], buf[i + 1]]);
            let n = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
            let v = i + 4;
            if v + n > len {
                return Err(StunError::Truncated);
            }
            // After FINGERPRINT nothing may follow; after MESSAGE-INTEGRITY only FINGERPRINT counts.
            if fingerprint_at.is_some() {
                break;
            }
            match t {
                ATTR_MESSAGE_INTEGRITY if integrity_at.is_none() => {
                    if n != 20 {
                        return Err(StunError::BadAttribute(t));
                    }
                    integrity_at = Some(i);
                },
                ATTR_FINGERPRINT => {
                    if n != 4 {
                        return Err(StunError::BadAttribute(t));
                    }
                    fingerprint_at = Some(i);
                },
                _ if integrity_at.is_some() => {
                    // Ignored (RFC 8489 14.5).
                },
                _ => {},
            }
            attrs.push((t, buf[v..v + n].to_vec()));
            i = v + n.div_ceil(4) * 4;
        }
        Ok(Message {
            method,
            class,
            tid,
            attrs,
            raw: buf.to_vec(),
            integrity_at,
            fingerprint_at,
        })
    }

    pub fn attr(&self, t: u16) -> Option<&[u8]> {
        self.attrs
            .iter()
            .find(|(a, _)| *a == t)
            .map(|(_, v)| v.as_slice())
    }

    pub fn has_integrity(&self) -> bool {
        self.integrity_at.is_some()
    }

    /// MESSAGE-INTEGRITY checks out with `key` (the long-term key, or a short-term password).
    pub fn verify_integrity(&self, key: &[u8]) -> bool {
        let Some(at) = self.integrity_at else {
            return false;
        };
        let mut head = self.raw[..at].to_vec();
        let n = (at - HEADER + 24) as u16;
        head[2..4].copy_from_slice(&n.to_be_bytes());
        let mac = hmac_sha1(key, &head);
        constant_time_eq(&mac, &self.raw[at + 4..at + 24])
    }

    /// FINGERPRINT is present and right.
    pub fn verify_fingerprint(&self) -> bool {
        let Some(at) = self.fingerprint_at else {
            return false;
        };
        let want = crc32(&self.raw[..at]) ^ FINGERPRINT_XOR;
        self.raw[at + 4..at + 8] == want.to_be_bytes()
    }

    pub fn has_fingerprint(&self) -> bool {
        self.fingerprint_at.is_some()
    }

    /// XOR-MAPPED-ADDRESS, XOR-RELAYED-ADDRESS or XOR-PEER-ADDRESS.
    pub fn xor_addr(&self, t: u16) -> Option<SocketAddr> {
        decode_xor_addr(self.attr(t)?, &self.tid)
    }

    pub fn mapped_addr(&self) -> Option<SocketAddr> {
        self.xor_addr(ATTR_XOR_MAPPED_ADDRESS)
            .or_else(|| self.attr(ATTR_MAPPED_ADDRESS).and_then(decode_plain_addr))
    }

    /// ERROR-CODE as a number (401, 438, ...).
    pub fn error_code(&self) -> Option<u16> {
        let v = self.attr(ATTR_ERROR_CODE)?;
        if v.len() < 4 {
            return None;
        }
        Some(u16::from(v[2] & 0x07) * 100 + u16::from(v[3]))
    }

    pub fn u32_attr(&self, t: u16) -> Option<u32> {
        let v = self.attr(t)?;
        (v.len() >= 4).then(|| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
    }
}

/// Builds one message; `build` appends MESSAGE-INTEGRITY and FINGERPRINT as asked.
pub struct Builder {
    buf: Vec<u8>,
    tid: TransId,
}

impl Builder {
    pub fn new(method: u16, class: Class, tid: TransId) -> Builder {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&message_type(method, class).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.extend_from_slice(&tid);
        Builder { buf, tid }
    }

    pub fn attr(mut self, t: u16, v: &[u8]) -> Builder {
        self.push(t, v);
        self
    }

    fn push(&mut self, t: u16, v: &[u8]) {
        self.buf.extend_from_slice(&t.to_be_bytes());
        self.buf.extend_from_slice(&(v.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(v);
        let pad = v.len().div_ceil(4) * 4 - v.len();
        self.buf.extend(std::iter::repeat_n(0u8, pad));
        self.set_len();
    }

    pub fn u32(self, t: u16, v: u32) -> Builder {
        self.attr(t, &v.to_be_bytes())
    }

    pub fn xor_addr(self, t: u16, a: SocketAddr) -> Builder {
        let v = encode_xor_addr(a, &self.tid);
        self.attr(t, &v)
    }

    fn set_len(&mut self) {
        let n = (self.buf.len() - HEADER) as u16;
        self.buf[2..4].copy_from_slice(&n.to_be_bytes());
    }

    pub fn build(mut self, integrity_key: Option<&[u8]>, fingerprint: bool) -> Vec<u8> {
        if let Some(key) = integrity_key {
            // The length must already count the integrity attribute when it is computed.
            let n = (self.buf.len() - HEADER + 24) as u16;
            self.buf[2..4].copy_from_slice(&n.to_be_bytes());
            let mac = hmac_sha1(key, &self.buf);
            self.push(ATTR_MESSAGE_INTEGRITY, &mac);
        }
        if fingerprint {
            let n = (self.buf.len() - HEADER + 8) as u16;
            self.buf[2..4].copy_from_slice(&n.to_be_bytes());
            let crc = crc32(&self.buf) ^ FINGERPRINT_XOR;
            self.push(ATTR_FINGERPRINT, &crc.to_be_bytes());
        }
        self.buf
    }
}

pub fn encode_xor_addr(a: SocketAddr, tid: &TransId) -> Vec<u8> {
    let port = a.port() ^ (MAGIC >> 16) as u16;
    let mut v = vec![0u8];
    match a.ip() {
        IpAddr::V4(ip) => {
            v.push(0x01);
            v.extend_from_slice(&port.to_be_bytes());
            let x = u32::from(ip) ^ MAGIC;
            v.extend_from_slice(&x.to_be_bytes());
        },
        IpAddr::V6(ip) => {
            v.push(0x02);
            v.extend_from_slice(&port.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(tid);
            let o = ip.octets();
            v.extend((0..16).map(|i| o[i] ^ mask[i]));
        },
    }
    v
}

pub fn decode_xor_addr(v: &[u8], tid: &TransId) -> Option<SocketAddr> {
    if v.len() < 8 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC >> 16) as u16;
    match v[1] {
        0x01 => {
            let x = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC;
            Some(SocketAddr::new(Ipv4Addr::from(x).into(), port))
        },
        0x02 if v.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(tid);
            let mut o = [0u8; 16];
            for i in 0..16 {
                o[i] = v[4 + i] ^ mask[i];
            }
            Some(SocketAddr::new(Ipv6Addr::from(o).into(), port))
        },
        _ => None,
    }
}

fn decode_plain_addr(v: &[u8]) -> Option<SocketAddr> {
    if v.len() < 8 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]);
    match v[1] {
        0x01 => Some(SocketAddr::new(
            Ipv4Addr::new(v[4], v[5], v[6], v[7]).into(),
            port,
        )),
        0x02 if v.len() >= 20 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&v[4..20]);
            Some(SocketAddr::new(Ipv6Addr::from(o).into(), port))
        },
        _ => None,
    }
}

/// The long-term credential key (RFC 8489 9.2.2): MD5(username ":" realm ":" password).
/// The values are used as given (no SASLprep/OpaqueString: TURN credentials handed out by our edge
/// are ASCII).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut s = Vec::with_capacity(username.len() + realm.len() + password.len() + 2);
    s.extend_from_slice(username.as_bytes());
    s.push(b':');
    s.extend_from_slice(realm.as_bytes());
    s.push(b':');
    s.extend_from_slice(password.as_bytes());
    md5(&s)
}

pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let k = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    let tag = aws_lc_rs::hmac::sign(&k, data);
    let mut out = [0u8; 20];
    out.copy_from_slice(tag.as_ref());
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// CRC-32 (ISO-HDLC, the one FINGERPRINT uses).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// MD5 (RFC 1321). Used only for the TURN long-term credential key (`long_term_key`), which the
/// protocol defines as an MD5 digest; nothing else here relies on it.
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: [u32; 64] = std::array::from_fn(|i| ((i as f64 + 1.).sin().abs() * 4294967296.) as u32);
    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for chunk in msg.chunks_exact(64) {
        let m: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ])
        });
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // RFC 5769 2.1: sample request (short-term credentials).
    const REQ: &str = "0001 0058 2112a442 b7e7a701bc34d686fa87dfae
        8022 0010 5354554e 20746573 7420636c 69656e74
        0024 0004 6e0001ff 8029 0008 932ff9b1 51263b36
        0006 0009 6576746a 3a683676 59202020
        0008 0014 9aeaa70c bfd8cb56 781ef2b5 b2d3f249 c1b571a2
        8028 0004 e57a3bcf";
    // RFC 5769 2.2: sample IPv4 response.
    const RESP4: &str = "0101 003c 2112a442 b7e7a701bc34d686fa87dfae
        8022 000b 74657374 20766563 746f7220
        0020 0008 0001a147 e112a643
        0008 0014 2b91f599 fd9e90c3 8c7489f9 2af9ba53 f06be7d7
        8028 0004 c07d4c96";
    // RFC 5769 2.4: sample request with long-term authentication.
    const REQ_LT: &str = "0001 0060 2112a442 78ad3433c6ad72c029da412e
        0006 0012 e3839ee3 8388e383 aae38383 e382afe3 82b90000
        0015 001c 662f2f34 39396b39 35346436 4f4c3334 6f4c3946 53547679 36347341
        0014 000b 6578616d 706c652e 6f726700
        0008 0014 f6702465 6dd64a3e 02b8e071 2e85c9a2 8ca89666";

    #[test]
    fn md5_rfc1321_vectors() {
        let v = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, want) in v {
            assert_eq!(to_hex(&md5(input.as_bytes())), want, "md5({input:?})");
        }
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn message_type_bits() {
        assert_eq!(message_type(BINDING, Class::Request), 0x0001);
        assert_eq!(message_type(BINDING, Class::Success), 0x0101);
        assert_eq!(message_type(BINDING, Class::Error), 0x0111);
        assert_eq!(message_type(ALLOCATE, Class::Request), 0x0003);
        assert_eq!(message_type(ALLOCATE, Class::Error), 0x0113);
        assert_eq!(message_type(SEND, Class::Indication), 0x0016);
        assert_eq!(message_type(DATA, Class::Indication), 0x0017);
        assert_eq!(message_type(CHANNEL_BIND, Class::Success), 0x0109);
        for m in [BINDING, ALLOCATE, REFRESH, SEND, DATA, CREATE_PERMISSION, CHANNEL_BIND, 0xABC] {
            for c in [Class::Request, Class::Indication, Class::Success, Class::Error] {
                assert_eq!(split_type(message_type(m, c)), (m, c));
            }
        }
    }

    #[test]
    fn rfc5769_request_short_term() {
        let b = hex(REQ);
        let m = Message::decode(&b).unwrap();
        assert_eq!((m.method, m.class), (BINDING, Class::Request));
        assert_eq!(m.attr(ATTR_USERNAME), Some(&b"evtj:h6vY"[..]));
        assert_eq!(m.attr(ATTR_SOFTWARE), Some(&b"STUN test client"[..]));
        assert!(m.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBt"));
        assert!(!m.verify_integrity(b"wrong"));
        assert!(m.verify_fingerprint());
    }

    #[test]
    fn rfc5769_response_ipv4() {
        let b = hex(RESP4);
        let m = Message::decode(&b).unwrap();
        assert_eq!((m.method, m.class), (BINDING, Class::Success));
        assert_eq!(m.mapped_addr(), Some("192.0.2.1:32853".parse().unwrap()));
        assert!(m.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBt"));
        assert!(m.verify_fingerprint());
    }

    #[test]
    fn rfc5769_long_term() {
        let b = hex(REQ_LT);
        let m = Message::decode(&b).unwrap();
        // Username "マトリックス", password "TheMatrIX" (the RFC's after SASLprep), realm "example.org".
        let key = long_term_key("\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}", "example.org", "TheMatrIX");
        assert!(m.verify_integrity(&key));
        assert!(!m.has_fingerprint());
        assert_eq!(m.attr(ATTR_REALM), Some(&b"example.org"[..]));
        assert_eq!(m.attr(ATTR_NONCE), Some(&b"f//499k954d6OL34oL9FSTvy64sA"[..]));
        // Our encoder makes the same bytes from the same attributes.
        let tid: TransId = b[8..20].try_into().unwrap();
        let ours = Builder::new(BINDING, Class::Request, tid)
            .attr(ATTR_USERNAME, m.attr(ATTR_USERNAME).unwrap())
            .attr(ATTR_NONCE, m.attr(ATTR_NONCE).unwrap())
            .attr(ATTR_REALM, b"example.org")
            .build(Some(&key), false);
        assert_eq!(to_hex(&ours), to_hex(&b));
    }

    #[test]
    fn encode_rfc5769_request_bytes() {
        let b = hex(REQ);
        let tid: TransId = b[8..20].try_into().unwrap();
        // RFC 5769 pads USERNAME with spaces; we pad with zeros, which MESSAGE-INTEGRITY covers, so
        // compare a decode of ours instead of the bytes.
        let ours = Builder::new(BINDING, Class::Request, tid)
            .attr(ATTR_SOFTWARE, b"STUN test client")
            .u32(0x0024, 0x6e0001ff)
            .attr(0x8029, &hex("932ff9b151263b36"))
            .attr(ATTR_USERNAME, b"evtj:h6vY")
            .build(Some(b"VOkJxbRl1RmTxUk/WvJxBt"), true);
        assert_eq!(ours.len(), b.len());
        let m = Message::decode(&ours).unwrap();
        assert!(m.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBt"));
        assert!(m.verify_fingerprint());
        assert_eq!(m.attr(ATTR_USERNAME), Some(&b"evtj:h6vY"[..]));
    }

    #[test]
    fn xor_addresses_roundtrip() {
        let tid = new_tid();
        for a in ["203.0.113.7:3478", "[2001:db8::1]:49152", "10.0.0.1:1"] {
            let a: SocketAddr = a.parse().unwrap();
            let m = Builder::new(ALLOCATE, Class::Success, tid)
                .xor_addr(ATTR_XOR_RELAYED_ADDRESS, a)
                .xor_addr(ATTR_XOR_MAPPED_ADDRESS, a)
                .build(None, true);
            let d = Message::decode(&m).unwrap();
            assert_eq!(d.xor_addr(ATTR_XOR_RELAYED_ADDRESS), Some(a));
            assert_eq!(d.mapped_addr(), Some(a));
            assert!(d.verify_fingerprint());
        }
    }

    #[test]
    fn error_code_and_garbage() {
        let tid = new_tid();
        let m = Builder::new(ALLOCATE, Class::Error, tid)
            .attr(ATTR_ERROR_CODE, &[0, 0, 4, 38, b'S', b't', b'a', b'l', b'e'])
            .build(None, false);
        let d = Message::decode(&m).unwrap();
        assert_eq!(d.error_code(), Some(438));
        assert_eq!(d.class, Class::Error);
        assert!(!is_stun(&[0x40, 0, 0, 4, 1, 2, 3, 4]));
        assert_eq!(Message::decode(&m[..m.len() - 4]).err(), Some(StunError::NotStun));
        let mut bad = m.clone();
        bad[3] = 0xFF;
        assert!(Message::decode(&bad).is_err());
        // The Debug output names attribute types only.
        let dbg = format!("{d:?}");
        assert!(!dbg.contains("Stale"));
    }
}
