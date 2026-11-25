/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A TURN client (RFC 8656) for the peer connection's relay candidates, and the stun:/turn:/turns:
//! URLs (RFC 7064/7065) it is configured with.
//!
//! `TurnClient` is sans-IO, like str0m: feed it what the server sent (`handle_input`), the time
//! (`handle_timeout`) and what str0m wants relayed (`send_to`); take what it wants sent to the
//! server (`poll_transmit`) and what happened (`poll_event`). It does the allocation with
//! long-term credentials (401 then an authenticated retry, 438 stale nonces), refreshes it,
//! installs permissions for the peers str0m talks to, binds channels and falls back to Send
//! indications until a channel is bound.
//!
//! The transports to the server: UDP through the peer's host socket (webrtc.rs routes the
//! datagrams), or TCP and TLS through `spawn_stream` (a thread per connection, RFC 6062 framing).
//! TLS verifies the server certificate against the URL's host name with the platform verifier;
//! a test-only extra root (PARTII_WEBRTC_TURN_TEST_CA, a PEM file) may be added for an offline
//! test server, but verification is never skipped.
//!
//! Credentials never reach a log: `Credentials` and `TurnClient` have hand-written Debug impls.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::stun::{self, Builder, Class, Message, TransId};

// ---------- URLs ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Stun,
    Stuns,
    Turn,
    Turns,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
    Tls,
}

/// A parsed stun:, stuns:, turn: or turns: URL. Holds no credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceUrl {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub transport: Transport,
}

impl fmt::Display for IceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.scheme {
            Scheme::Stun => "stun",
            Scheme::Stuns => "stuns",
            Scheme::Turn => "turn",
            Scheme::Turns => "turns",
        };
        let t = match self.transport {
            Transport::Udp => "udp",
            Transport::Tcp => "tcp",
            Transport::Tls => "tls",
        };
        if self.host.contains(':') {
            write!(f, "{s}:[{}]:{} ({t})", self.host, self.port)
        } else {
            write!(f, "{s}:{}:{} ({t})", self.host, self.port)
        }
    }
}

/// Parse an ICE server URL: `stun:host[:port]`, `turn:host[:port][?transport=udp|tcp]`,
/// `turns:host[:port][?transport=tcp]` (IPv6 hosts in brackets). turns: is TLS over TCP.
pub fn parse_url(s: &str) -> Option<IceUrl> {
    let s = s.trim();
    let (scheme, rest) = s.split_once(':')?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "stun" => Scheme::Stun,
        "stuns" => Scheme::Stuns,
        "turn" => Scheme::Turn,
        "turns" => Scheme::Turns,
        _ => return None,
    };
    let (hostport, query) = match rest.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (rest, None),
    };
    // No user info, paths or fragments in these URLs.
    if hostport.is_empty() || hostport.contains(['/', '@', '#']) {
        return None;
    }
    let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
        let (h, after) = r.split_once(']')?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?.parse::<u16>().ok()?),
        };
        h.parse::<std::net::Ipv6Addr>().ok()?;
        (h.to_owned(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), Some(p.parse::<u16>().ok()?)),
            None => (hostport.to_owned(), None),
        }
    };
    if host.is_empty() || port == Some(0) {
        return None;
    }
    let secure = matches!(scheme, Scheme::Stuns | Scheme::Turns);
    let mut transport = if secure { Transport::Tls } else { Transport::Udp };
    if let Some(q) = query {
        for kv in q.split('&') {
            match kv.split_once('=') {
                Some(("transport", v)) => match (v.to_ascii_lowercase().as_str(), secure) {
                    ("udp", false) => transport = Transport::Udp,
                    ("tcp", false) => transport = Transport::Tcp,
                    ("tcp", true) => transport = Transport::Tls,
                    // turns: over UDP would be DTLS: not supported.
                    _ => return None,
                },
                _ => return None,
            }
        }
        if !matches!(scheme, Scheme::Turn | Scheme::Turns) {
            return None;
        }
    }
    let port = port.unwrap_or(if secure { 5349 } else { 3478 });
    Some(IceUrl {
        scheme,
        host,
        port,
        transport,
    })
}

/// Resolve a URL's host (blocking: call it off the peer's thread).
pub fn resolve(url: &IceUrl) -> Vec<SocketAddr> {
    (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map(|a| a.collect())
        .unwrap_or_default()
}

// ---------- the client ----------

/// TURN long-term credentials. Debug prints neither.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TurnEvent {
    /// The allocation is up: `relay` is the relayed address (the relay candidate), `mapped` our
    /// address as the server saw it.
    Allocated {
        relay: SocketAddr,
        mapped: Option<SocketAddr>,
    },
    /// No allocation (an error response, no answer, or the allocation was lost).
    Failed(String),
    /// Data from a peer through the relay.
    Data { peer: SocketAddr, data: Vec<u8> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TxKind {
    Allocate,
    Refresh(u32),
    Permission(IpAddr),
    ChannelBind(SocketAddr, u16),
}

struct Tx {
    kind: TxKind,
    msg: Vec<u8>,
    next: Instant,
    rto: Duration,
    tries: u32,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Allocating,
    Allocated,
    Failed,
    Closed,
}

struct Perm {
    installed: bool,
    pending: bool,
    refresh_at: Instant,
    last_used: Instant,
    queue: Vec<(SocketAddr, Vec<u8>)>,
}

struct Chan {
    number: u16,
    bound: bool,
    pending: bool,
    refresh_at: Instant,
    last_used: Instant,
}

/// Retransmissions over UDP: 250 ms doubling, 6 sends in all (about 8 s). Over TCP/TLS one send
/// and a 10 s wait.
const RTO: Duration = Duration::from_millis(250);
const MAX_TRIES: u32 = 6;
const RELIABLE_WAIT: Duration = Duration::from_secs(10);
const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);
const CHANNEL_LIFETIME: Duration = Duration::from_secs(600);
/// Permissions and channels unused this long are left to expire.
const IDLE_KEEP: Duration = Duration::from_secs(300);
const MAX_QUEUED: usize = 16;
const UDP: u8 = 17;

pub struct TurnClient {
    creds: Credentials,
    reliable: bool,
    realm: Option<Vec<u8>>,
    nonce: Option<Vec<u8>>,
    key: Option<[u8; 16]>,
    /// 401/438 retries left for the current request chain (reset on a success).
    auth_retries: u32,
    state: State,
    relay: Option<SocketAddr>,
    refresh_at: Option<Instant>,
    perms: HashMap<IpAddr, Perm>,
    channels: HashMap<SocketAddr, Chan>,
    by_channel: HashMap<u16, SocketAddr>,
    next_channel: u16,
    txs: HashMap<TransId, Tx>,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<TurnEvent>,
}

impl fmt::Debug for TurnClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TurnClient({:?}, relay {:?}, {} permissions, {} channels)",
            self.state,
            self.relay,
            self.perms.len(),
            self.channels.len()
        )
    }
}

impl TurnClient {
    /// `reliable`: the transport to the server is TCP or TLS (no retransmissions; ChannelData
    /// padded to 4 bytes).
    pub fn new(creds: Credentials, reliable: bool) -> TurnClient {
        TurnClient {
            creds,
            reliable,
            realm: None,
            nonce: None,
            key: None,
            auth_retries: 3,
            state: State::Idle,
            relay: None,
            refresh_at: None,
            perms: HashMap::new(),
            channels: HashMap::new(),
            by_channel: HashMap::new(),
            next_channel: 0x4000,
            txs: HashMap::new(),
            out: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn relay(&self) -> Option<SocketAddr> {
        (self.state == State::Allocated).then_some(self.relay).flatten()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_failed(&self) -> bool {
        self.state == State::Failed
    }

    /// Send the first Allocate (unauthenticated: the server answers 401 with its realm and nonce).
    pub fn start(&mut self, now: Instant) {
        if self.state == State::Idle {
            self.state = State::Allocating;
            self.request(TxKind::Allocate, now);
        }
    }

    /// Deallocate (a Refresh with lifetime 0, sent once) and stop.
    pub fn close(&mut self, now: Instant) {
        if self.state == State::Allocated {
            self.request(TxKind::Refresh(0), now);
        }
        self.state = State::Closed;
        self.txs.clear();
    }

    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<TurnEvent> {
        self.events.pop_front()
    }

    fn fail(&mut self, why: String) {
        if !matches!(self.state, State::Failed | State::Closed) {
            self.state = State::Failed;
            self.relay = None;
            self.txs.clear();
            self.perms.clear();
            self.channels.clear();
            self.by_channel.clear();
            self.events.push_back(TurnEvent::Failed(why));
        }
    }

    fn request(&mut self, kind: TxKind, now: Instant) {
        let tid = stun::new_tid();
        let method = match kind {
            TxKind::Allocate => stun::ALLOCATE,
            TxKind::Refresh(_) => stun::REFRESH,
            TxKind::Permission(_) => stun::CREATE_PERMISSION,
            TxKind::ChannelBind(..) => stun::CHANNEL_BIND,
        };
        let mut b = Builder::new(method, Class::Request, tid);
        match kind {
            TxKind::Allocate => {
                b = b.attr(stun::ATTR_REQUESTED_TRANSPORT, &[UDP, 0, 0, 0]);
            },
            TxKind::Refresh(lifetime) => {
                b = b.u32(stun::ATTR_LIFETIME, lifetime);
            },
            TxKind::Permission(ip) => {
                b = b.xor_addr(stun::ATTR_XOR_PEER_ADDRESS, SocketAddr::new(ip, 0));
            },
            TxKind::ChannelBind(peer, number) => {
                b = b
                    .attr(stun::ATTR_CHANNEL_NUMBER, &[(number >> 8) as u8, number as u8, 0, 0])
                    .xor_addr(stun::ATTR_XOR_PEER_ADDRESS, peer);
            },
        }
        let msg = if let (Some(key), Some(realm), Some(nonce)) = (self.key, &self.realm, &self.nonce)
        {
            b.attr(stun::ATTR_USERNAME, self.creds.username.as_bytes())
                .attr(stun::ATTR_REALM, realm)
                .attr(stun::ATTR_NONCE, nonce)
                .build(Some(&key), true)
        } else {
            b.build(None, true)
        };
        self.out.push_back(msg.clone());
        let wait = if self.reliable { RELIABLE_WAIT } else { RTO };
        self.txs.insert(
            tid,
            Tx {
                kind,
                msg,
                next: now + wait,
                rto: wait,
                tries: 1,
                deadline: now + if self.reliable { RELIABLE_WAIT } else { RTO * 63 },
            },
        );
    }

    /// Retransmissions, and the refreshes of the allocation, permissions and channels.
    pub fn handle_timeout(&mut self, now: Instant) {
        if matches!(self.state, State::Failed | State::Closed | State::Idle) {
            return;
        }
        let mut expired = vec![];
        for (tid, tx) in self.txs.iter_mut() {
            if now < tx.next {
                continue;
            }
            if self.reliable || tx.tries >= MAX_TRIES || now >= tx.deadline {
                expired.push(*tid);
                continue;
            }
            tx.tries += 1;
            tx.rto *= 2;
            tx.next = now + tx.rto;
            self.out.push_back(tx.msg.clone());
        }
        for tid in expired {
            let Some(tx) = self.txs.remove(&tid) else {
                continue;
            };
            match tx.kind {
                TxKind::Allocate => self.fail("no answer to Allocate".into()),
                TxKind::Refresh(0) => {},
                TxKind::Refresh(_) => self.fail("no answer to Refresh".into()),
                TxKind::Permission(ip) => {
                    self.perms.remove(&ip);
                },
                TxKind::ChannelBind(peer, n) => {
                    self.channels.remove(&peer);
                    self.by_channel.remove(&n);
                },
            }
        }
        if self.state != State::Allocated {
            return;
        }
        if self.refresh_at.is_some_and(|t| now >= t) {
            self.refresh_at = None;
            self.request(TxKind::Refresh(600), now);
        }
        let perms: Vec<IpAddr> = self
            .perms
            .iter()
            .filter(|(_, p)| p.installed && !p.pending && now >= p.refresh_at && now - p.last_used < IDLE_KEEP)
            .map(|(ip, _)| *ip)
            .collect();
        for ip in perms {
            if let Some(p) = self.perms.get_mut(&ip) {
                p.pending = true;
            }
            self.request(TxKind::Permission(ip), now);
        }
        let chans: Vec<(SocketAddr, u16)> = self
            .channels
            .iter()
            .filter(|(_, c)| c.bound && !c.pending && now >= c.refresh_at && now - c.last_used < IDLE_KEEP)
            .map(|(a, c)| (*a, c.number))
            .collect();
        for (peer, n) in chans {
            if let Some(c) = self.channels.get_mut(&peer) {
                c.pending = true;
            }
            self.request(TxKind::ChannelBind(peer, n), now);
        }
    }

    /// Something from the server (one datagram, or one frame off a stream). Returns false when it
    /// is not ours (not STUN for one of our transactions, a Data indication or ChannelData).
    pub fn handle_input(&mut self, buf: &[u8], now: Instant) -> bool {
        if buf.len() >= 4 && (0x40..0x80).contains(&buf[0]) {
            return self.channel_data(buf);
        }
        let Ok(m) = Message::decode(buf) else {
            return false;
        };
        if m.class == Class::Indication {
            if m.method == stun::DATA &&
                let (Some(peer), Some(data)) =
                    (m.xor_addr(stun::ATTR_XOR_PEER_ADDRESS), m.attr(stun::ATTR_DATA))
            {
                self.events.push_back(TurnEvent::Data {
                    peer,
                    data: data.to_vec(),
                });
                return true;
            }
            return m.method == stun::DATA;
        }
        if !matches!(m.class, Class::Success | Class::Error) || !self.txs.contains_key(&m.tid) {
            return false;
        }
        // A response that claims integrity must check out with our key (else it is not the server's).
        if m.has_integrity() && let Some(key) = self.key && !m.verify_integrity(&key) {
            return true;
        }
        let Some(tx) = self.txs.remove(&m.tid) else {
            return false;
        };
        if m.class == Class::Error {
            self.error_response(tx.kind, &m, now);
        } else {
            self.auth_retries = 3;
            self.success_response(tx.kind, &m, now);
        }
        true
    }

    fn error_response(&mut self, kind: TxKind, m: &Message, now: Instant) {
        let code = m.error_code().unwrap_or(0);
        match code {
            // Unauthenticated (first Allocate) or our nonce expired: take the realm and nonce, retry.
            401 | 438 if self.auth_retries > 0 && m.attr(stun::ATTR_NONCE).is_some() => {
                self.auth_retries -= 1;
                // A 401 to a request we already authenticated means the credentials are wrong,
                // unless the realm changed: retry once more only then.
                let realm = m.attr(stun::ATTR_REALM).map(<[u8]>::to_vec);
                if code == 401 && self.key.is_some() && (realm.is_none() || realm == self.realm) {
                    return self.give_up(kind, "401: credentials refused".into());
                }
                if let Some(r) = realm {
                    self.realm = Some(r);
                }
                self.nonce = m.attr(stun::ATTR_NONCE).map(<[u8]>::to_vec);
                let Some(realm) = self.realm.as_ref() else {
                    return self.give_up(kind, format!("{code} without REALM"));
                };
                let realm = String::from_utf8_lossy(realm).into_owned();
                self.key = Some(stun::long_term_key(
                    &self.creds.username,
                    &realm,
                    &self.creds.password,
                ));
                self.request(kind, now);
            },
            _ => self.give_up(kind, format!("error {code}")),
        }
    }

    fn give_up(&mut self, kind: TxKind, why: String) {
        match kind {
            TxKind::Allocate => self.fail(format!("Allocate: {why}")),
            TxKind::Refresh(0) => {},
            TxKind::Refresh(_) => self.fail(format!("Refresh: {why}")),
            TxKind::Permission(ip) => {
                self.perms.remove(&ip);
            },
            TxKind::ChannelBind(peer, n) => {
                self.channels.remove(&peer);
                self.by_channel.remove(&n);
            },
        }
    }

    fn success_response(&mut self, kind: TxKind, m: &Message, now: Instant) {
        match kind {
            TxKind::Allocate => {
                if self.state != State::Allocating {
                    return;
                }
                let Some(relay) = m.xor_addr(stun::ATTR_XOR_RELAYED_ADDRESS) else {
                    return self.fail("Allocate success without XOR-RELAYED-ADDRESS".into());
                };
                let lifetime = m.u32_attr(stun::ATTR_LIFETIME).unwrap_or(600);
                self.refresh_at = Some(now + refresh_after(lifetime));
                self.relay = Some(relay);
                self.state = State::Allocated;
                self.events.push_back(TurnEvent::Allocated {
                    relay,
                    mapped: m.mapped_addr(),
                });
            },
            TxKind::Refresh(0) => {},
            TxKind::Refresh(_) => {
                let lifetime = m.u32_attr(stun::ATTR_LIFETIME).unwrap_or(600);
                self.refresh_at = Some(now + refresh_after(lifetime));
            },
            TxKind::Permission(ip) => {
                let Some(p) = self.perms.get_mut(&ip) else {
                    return;
                };
                p.installed = true;
                p.pending = false;
                p.refresh_at = now + PERMISSION_LIFETIME - Duration::from_secs(60);
                let queue = std::mem::take(&mut p.queue);
                for (peer, data) in queue {
                    self.send_to(peer, &data, now);
                }
            },
            TxKind::ChannelBind(peer, number) => {
                if let Some(c) = self.channels.get_mut(&peer) &&
                    c.number == number
                {
                    c.bound = true;
                    c.pending = false;
                    c.refresh_at = now + CHANNEL_LIFETIME - Duration::from_secs(60);
                }
                // A channel bind also refreshes the permission.
                if let Some(p) = self.perms.get_mut(&peer.ip()) {
                    p.refresh_at = now + PERMISSION_LIFETIME - Duration::from_secs(60);
                }
            },
        }
    }

    fn channel_data(&mut self, buf: &[u8]) -> bool {
        let number = u16::from_be_bytes([buf[0], buf[1]]);
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let Some(peer) = self.by_channel.get(&number).copied() else {
            return false;
        };
        if buf.len() < 4 + len {
            return true;
        }
        self.events.push_back(TurnEvent::Data {
            peer,
            data: buf[4..4 + len].to_vec(),
        });
        true
    }

    /// Relay `data` to `peer`: through a bound channel, else a Send indication once the
    /// permission is installed (queued until then); creates the permission and binds a channel
    /// the first time.
    pub fn send_to(&mut self, peer: SocketAddr, data: &[u8], now: Instant) {
        if self.state != State::Allocated {
            return;
        }
        let perm = self.perms.entry(peer.ip()).or_insert(Perm {
            installed: false,
            pending: false,
            refresh_at: now,
            last_used: now,
            queue: vec![],
        });
        perm.last_used = now;
        if !perm.installed {
            if perm.queue.len() < MAX_QUEUED {
                perm.queue.push((peer, data.to_vec()));
            }
            if !perm.pending {
                perm.pending = true;
                self.request(TxKind::Permission(peer.ip()), now);
            }
            return;
        }
        if let Some(c) = self.channels.get_mut(&peer) {
            c.last_used = now;
            if c.bound {
                let number = c.number;
                let mut v = Vec::with_capacity(4 + data.len() + 3);
                v.extend_from_slice(&number.to_be_bytes());
                v.extend_from_slice(&(data.len() as u16).to_be_bytes());
                v.extend_from_slice(data);
                if self.reliable {
                    v.resize(v.len().div_ceil(4) * 4, 0);
                }
                self.out.push_back(v);
                return;
            }
        } else if self.next_channel <= 0x4FFF {
            let number = self.next_channel;
            self.next_channel += 1;
            self.channels.insert(
                peer,
                Chan {
                    number,
                    bound: false,
                    pending: true,
                    refresh_at: now,
                    last_used: now,
                },
            );
            self.by_channel.insert(number, peer);
            self.request(TxKind::ChannelBind(peer, number), now);
        }
        let ind = Builder::new(stun::SEND, Class::Indication, stun::new_tid())
            .xor_addr(stun::ATTR_XOR_PEER_ADDRESS, peer)
            .attr(stun::ATTR_DATA, data)
            .build(None, false);
        self.out.push_back(ind);
    }
}

fn refresh_after(lifetime: u32) -> Duration {
    let l = u64::from(lifetime.max(30));
    Duration::from_secs(if l > 120 { l - 60 } else { l / 2 })
}

// ---------- TCP/TLS framing (RFC 8656 section 11.5) ----------

/// Splits a TCP/TLS byte stream into STUN messages and ChannelData frames.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut frames = vec![];
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let n = match self.buf[0] >> 6 {
                0b00 => {
                    let n = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                    stun::HEADER + n
                },
                0b01 => {
                    let n = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                    4 + n.div_ceil(4) * 4
                },
                _ => {
                    // Not TURN framing: the stream is broken.
                    self.buf.clear();
                    break;
                },
            };
            if self.buf.len() < n {
                break;
            }
            frames.push(self.buf.drain(..n).collect());
        }
        frames
    }
}

// ---------- TCP/TLS links ----------

/// The writing half of a TCP or TLS connection to a TURN server (the reader is its own thread).
pub enum StreamWriter {
    Tcp(TcpStream),
    Tls(Arc<Mutex<rustls::ClientConnection>>, TcpStream),
}

impl StreamWriter {
    pub fn send(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            StreamWriter::Tcp(s) => s.write_all(data),
            StreamWriter::Tls(conn, s) => {
                let mut c = conn.lock().map_err(|_| std::io::Error::other("tls poisoned"))?;
                c.writer().write_all(data)?;
                while c.wants_write() {
                    c.write_tls(s)?;
                }
                Ok(())
            },
        }
    }

    pub fn shutdown(&self) {
        let s = match self {
            StreamWriter::Tcp(s) | StreamWriter::Tls(_, s) => s,
        };
        let _ = s.shutdown(Shutdown::Both);
    }
}

pub enum LinkEvent {
    Connected {
        local: SocketAddr,
        server: SocketAddr,
        writer: StreamWriter,
    },
    Frames(Vec<Vec<u8>>),
    Closed(String),
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CFG: std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, String>> =
        std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        // Test-only: one more trusted root (an offline test TURN server's CA). Verification itself
        // always runs.
        let extra: Vec<rustls_pki_types::CertificateDer<'static>> =
            match std::env::var("PARTII_WEBRTC_TURN_TEST_CA") {
                Ok(path) => {
                    use rustls_pki_types::pem::PemObject;
                    let pem = std::fs::read(&path).map_err(|e| format!("test CA: {e}"))?;
                    rustls_pki_types::CertificateDer::pem_slice_iter(&pem)
                        .filter_map(Result::ok)
                        .collect()
                },
                Err(_) => vec![],
            };
        #[cfg(not(target_os = "android"))]
        let verifier = if extra.is_empty() {
            rustls_platform_verifier::Verifier::new(provider.clone())
        } else {
            rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider.clone())
        }
        .map_err(|e| format!("verifier: {e}"))?;
        #[cfg(target_os = "android")]
        let verifier = {
            let _ = extra;
            rustls_platform_verifier::Verifier::new(provider.clone())
                .map_err(|e| format!("verifier: {e}"))?
        };
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls: {e}"))?
            // The platform verifier (not a custom bypass): checks the chain and the host name.
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        Ok(Arc::new(cfg))
    })
    .clone()
}

/// Connect to a TURN server over TCP or TLS on a thread of its own; `sink` gets the connection,
/// then the frames read, then the close. It returns false when the peer is gone (the thread ends).
pub fn spawn_stream(url: IceUrl, sink: Box<dyn Fn(LinkEvent) -> bool + Send>) {
    let _ = thread::Builder::new()
        .name("PartiiRtcTurn".to_owned())
        .spawn(move || {
            let why = match run_stream(&url, &*sink) {
                Ok(()) => "closed".to_owned(),
                Err(e) => e,
            };
            sink(LinkEvent::Closed(why));
        });
}

fn run_stream(url: &IceUrl, sink: &dyn Fn(LinkEvent) -> bool) -> Result<(), String> {
    let addrs = resolve(url);
    // IPv4 first: the host candidates are IPv4.
    let mut addrs: Vec<_> = addrs.iter().filter(|a| a.is_ipv4()).chain(addrs.iter().filter(|a| a.is_ipv6())).copied().collect();
    addrs.dedup();
    if addrs.is_empty() {
        return Err(format!("{url}: no address"));
    }
    let mut last = String::new();
    let mut sock = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
            Ok(s) => {
                sock = Some((s, a));
                break;
            },
            Err(e) => last = format!("{url}: connect: {e}"),
        }
    }
    let Some((sock, server)) = sock else {
        return Err(last);
    };
    let _ = sock.set_nodelay(true);
    let local = sock.local_addr().map_err(|e| e.to_string())?;
    let mut reader = sock.try_clone().map_err(|e| e.to_string())?;
    let _ = reader.set_read_timeout(Some(Duration::from_millis(500)));
    let mut framer = Framer::default();
    let mut buf = vec![0u8; 16 * 1024];
    if url.transport == Transport::Tls {
        let cfg = tls_config()?;
        let name = rustls_pki_types::ServerName::try_from(url.host.clone())
            .map_err(|e| format!("{url}: server name: {e}"))?;
        let conn = rustls::ClientConnection::new(cfg, name).map_err(|e| format!("tls: {e}"))?;
        let conn = Arc::new(Mutex::new(conn));
        let mut w = sock.try_clone().map_err(|e| e.to_string())?;
        {
            let mut c = conn.lock().map_err(|_| "tls poisoned")?;
            while c.wants_write() {
                c.write_tls(&mut w).map_err(|e| format!("tls write: {e}"))?;
            }
        }
        if !sink(LinkEvent::Connected {
            local,
            server,
            writer: StreamWriter::Tls(conn.clone(), sock),
        }) {
            return Ok(());
        }
        let mut plain = vec![0u8; 16 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if !sink(LinkEvent::Frames(vec![])) {
                        return Ok(());
                    }
                    continue;
                },
                Err(e) => return Err(format!("read: {e}")),
            };
            let mut got = vec![];
            {
                let mut c = conn.lock().map_err(|_| "tls poisoned")?;
                let mut rd = &buf[..n];
                // Feed the records in, taking the plaintext out as it comes (rustls's buffer is
                // bounded, so a big read takes several rounds).
                let mut stuck = 0;
                while !rd.is_empty() && stuck < 2 {
                    let k = c.read_tls(&mut rd).map_err(|e| format!("tls read: {e}"))?;
                    c.process_new_packets().map_err(|e| format!("tls: {e}"))?;
                    loop {
                        match c.reader().read(&mut plain) {
                            Ok(0) => break,
                            Ok(k) => got.extend_from_slice(&plain[..k]),
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) => return Err(format!("tls: {e}")),
                        }
                    }
                    stuck = if k == 0 { stuck + 1 } else { 0 };
                }
                if !rd.is_empty() {
                    return Err("tls: records not taken".into());
                }
                while c.wants_write() {
                    c.write_tls(&mut w).map_err(|e| format!("tls write: {e}"))?;
                }
            }
            let frames = framer.push(&got);
            if !frames.is_empty() && !sink(LinkEvent::Frames(frames)) {
                return Ok(());
            }
        }
    } else {
        let w = sock.try_clone().map_err(|e| e.to_string())?;
        if !sink(LinkEvent::Connected {
            local,
            server,
            writer: StreamWriter::Tcp(w),
        }) {
            return Ok(());
        }
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if !sink(LinkEvent::Frames(vec![])) {
                        return Ok(());
                    }
                    continue;
                },
                Err(e) => return Err(format!("read: {e}")),
            };
            let frames = framer.push(&buf[..n]);
            if !frames.is_empty() && !sink(LinkEvent::Frames(frames)) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials {
            username: "test-user".into(),
            password: "test-pass".into(),
        }
    }

    /// A minimal TURN server side for the client under test: answers what the client sent.
    struct Server {
        realm: &'static str,
        nonce: String,
        key: [u8; 16],
        relay: SocketAddr,
        mapped: SocketAddr,
    }

    impl Server {
        fn new() -> Server {
            Server {
                realm: "partii.test",
                nonce: "nonce-1".into(),
                key: stun::long_term_key("test-user", "partii.test", "test-pass"),
                relay: "198.51.100.9:50000".parse().unwrap(),
                mapped: "203.0.113.5:40000".parse().unwrap(),
            }
        }

        fn error(&self, m: &Message, code: u16) -> Vec<u8> {
            Builder::new(m.method, Class::Error, m.tid)
                .attr(stun::ATTR_ERROR_CODE, &[0, 0, (code / 100) as u8, (code % 100) as u8])
                .attr(stun::ATTR_REALM, self.realm.as_bytes())
                .attr(stun::ATTR_NONCE, self.nonce.as_bytes())
                .build(None, true)
        }

        /// The response to one request, with 401 when unauthenticated and 438 for an old nonce.
        fn answer(&self, req: &[u8]) -> Vec<u8> {
            let m = Message::decode(req).unwrap();
            assert_eq!(m.class, Class::Request);
            assert!(m.verify_fingerprint());
            if !m.has_integrity() {
                return self.error(&m, 401);
            }
            assert!(m.verify_integrity(&self.key), "client integrity");
            assert_eq!(m.attr(stun::ATTR_USERNAME), Some(&b"test-user"[..]));
            if m.attr(stun::ATTR_NONCE) != Some(self.nonce.as_bytes()) {
                return self.error(&m, 438);
            }
            let b = Builder::new(m.method, Class::Success, m.tid);
            let b = match m.method {
                stun::ALLOCATE => {
                    assert_eq!(m.attr(stun::ATTR_REQUESTED_TRANSPORT), Some(&[17u8, 0, 0, 0][..]));
                    b.xor_addr(stun::ATTR_XOR_RELAYED_ADDRESS, self.relay)
                        .xor_addr(stun::ATTR_XOR_MAPPED_ADDRESS, self.mapped)
                        .u32(stun::ATTR_LIFETIME, 600)
                },
                stun::REFRESH => {
                    let l = m.u32_attr(stun::ATTR_LIFETIME).unwrap();
                    b.u32(stun::ATTR_LIFETIME, l)
                },
                _ => b,
            };
            b.build(Some(&self.key), true)
        }
    }

    fn pump(c: &mut TurnClient, s: &Server, now: Instant) -> Vec<Vec<u8>> {
        // Answer every request; return what else the client sent (indications, channel data).
        let mut other = vec![];
        for _ in 0..20 {
            let Some(out) = c.poll_transmit() else { break };
            if stun::is_stun(&out) && Message::decode(&out).unwrap().class == Class::Request {
                let r = s.answer(&out);
                assert!(c.handle_input(&r, now));
            } else {
                other.push(out);
            }
        }
        other
    }

    fn allocated(reliable: bool) -> (TurnClient, Server, Instant) {
        let s = Server::new();
        let mut c = TurnClient::new(creds(), reliable);
        let now = Instant::now();
        c.start(now);
        // First Allocate: no integrity, answered 401; the retry carries it.
        let first = c.poll_transmit().unwrap();
        let m = Message::decode(&first).unwrap();
        assert_eq!(m.method, stun::ALLOCATE);
        assert!(!m.has_integrity());
        assert!(c.handle_input(&s.answer(&first), now));
        pump(&mut c, &s, now);
        assert_eq!(
            c.poll_event(),
            Some(TurnEvent::Allocated {
                relay: s.relay,
                mapped: Some(s.mapped)
            })
        );
        assert_eq!(c.relay(), Some(s.relay));
        (c, s, now)
    }

    #[test]
    fn allocate_with_401_challenge() {
        allocated(false);
    }

    #[test]
    fn stale_nonce_then_refresh() {
        let (mut c, mut s, now) = allocated(false);
        // The server rotates its nonce: the next Refresh gets 438 and is retried with the new one.
        s.nonce = "nonce-2".into();
        let later = now + Duration::from_secs(541);
        c.handle_timeout(later);
        let r = c.poll_transmit().unwrap();
        let m = Message::decode(&r).unwrap();
        assert_eq!(m.method, stun::REFRESH);
        assert_eq!(m.u32_attr(stun::ATTR_LIFETIME), Some(600));
        let resp = s.answer(&r);
        assert_eq!(Message::decode(&resp).unwrap().error_code(), Some(438));
        assert!(c.handle_input(&resp, later));
        let retry = c.poll_transmit().unwrap();
        let m2 = Message::decode(&retry).unwrap();
        assert_eq!(m2.attr(stun::ATTR_NONCE), Some(&b"nonce-2"[..]));
        assert!(c.handle_input(&s.answer(&retry), later));
        assert_eq!(c.poll_event(), None);
        assert_eq!(c.relay(), Some(s.relay));
        // Not due again until about the lifetime later.
        c.handle_timeout(later + Duration::from_secs(10));
        assert!(c.poll_transmit().is_none());
    }

    #[test]
    fn wrong_credentials_fail() {
        let mut s = Server::new();
        s.key = stun::long_term_key("test-user", "partii.test", "other");
        let mut c = TurnClient::new(creds(), false);
        let now = Instant::now();
        c.start(now);
        let first = c.poll_transmit().unwrap();
        c.handle_input(&s.answer(&first), now);
        let second = c.poll_transmit().unwrap();
        // The server can't verify it: answers 401 again (as coturn does), and the client gives up.
        let m = Message::decode(&second).unwrap();
        assert!(!m.verify_integrity(&s.key));
        c.handle_input(&s.error(&m, 401), now);
        assert!(matches!(c.poll_event(), Some(TurnEvent::Failed(_))));
        assert!(c.is_failed());
    }

    #[test]
    fn retransmits_then_fails_over_udp() {
        let mut c = TurnClient::new(creds(), false);
        let now = Instant::now();
        c.start(now);
        assert!(c.poll_transmit().is_some());
        let mut t = now;
        let mut sends = 1;
        for _ in 0..200 {
            t += Duration::from_millis(100);
            c.handle_timeout(t);
            while c.poll_transmit().is_some() {
                sends += 1;
            }
        }
        assert_eq!(sends, MAX_TRIES);
        assert!(matches!(c.poll_event(), Some(TurnEvent::Failed(_))));
    }

    #[test]
    fn permission_channel_and_data() {
        let (mut c, s, now) = allocated(false);
        let peer: SocketAddr = "192.0.2.77:6000".parse().unwrap();
        // Before the permission: queued, and a CreatePermission goes out.
        c.send_to(peer, b"hello", now);
        let p = c.poll_transmit().unwrap();
        let m = Message::decode(&p).unwrap();
        assert_eq!(m.method, stun::CREATE_PERMISSION);
        assert_eq!(m.xor_addr(stun::ATTR_XOR_PEER_ADDRESS).map(|a| a.ip()), Some(peer.ip()));
        assert!(c.handle_input(&s.answer(&p), now));
        // The queued packet goes now: a ChannelBind and, until it is bound, a Send indication.
        let mut bind = None;
        let mut ind = None;
        while let Some(o) = c.poll_transmit() {
            let m = Message::decode(&o).unwrap();
            match (m.method, m.class) {
                (stun::CHANNEL_BIND, Class::Request) => bind = Some(o),
                (stun::SEND, Class::Indication) => ind = Some(m),
                x => panic!("unexpected {x:?}"),
            }
        }
        let ind = ind.unwrap();
        assert_eq!(ind.xor_addr(stun::ATTR_XOR_PEER_ADDRESS), Some(peer));
        assert_eq!(ind.attr(stun::ATTR_DATA), Some(&b"hello"[..]));
        let bind = bind.unwrap();
        let bm = Message::decode(&bind).unwrap();
        assert_eq!(bm.attr(stun::ATTR_CHANNEL_NUMBER), Some(&[0x40u8, 0, 0, 0][..]));
        assert!(c.handle_input(&s.answer(&bind), now));
        // Bound: ChannelData.
        c.send_to(peer, b"abc", now);
        let cd = c.poll_transmit().unwrap();
        assert_eq!(cd, vec![0x40, 0x00, 0x00, 0x03, b'a', b'b', b'c']);
        // Incoming: a Data indication and ChannelData both come out as Data.
        let di = Builder::new(stun::DATA, Class::Indication, stun::new_tid())
            .xor_addr(stun::ATTR_XOR_PEER_ADDRESS, peer)
            .attr(stun::ATTR_DATA, b"from-ind")
            .build(None, false);
        assert!(c.handle_input(&di, now));
        assert_eq!(c.poll_event(), Some(TurnEvent::Data { peer, data: b"from-ind".to_vec() }));
        assert!(c.handle_input(&[0x40, 0x00, 0x00, 0x02, b'h', b'i', 0, 0], now));
        assert_eq!(c.poll_event(), Some(TurnEvent::Data { peer, data: b"hi".to_vec() }));
        // An unknown channel or a stranger's STUN is not ours.
        assert!(!c.handle_input(&[0x41, 0x00, 0x00, 0x00], now));
        let stray = Builder::new(stun::BINDING, Class::Success, stun::new_tid()).build(None, false);
        assert!(!c.handle_input(&stray, now));
        // Permission refresh before the 300 s lifetime, while in use.
        let later = now + Duration::from_secs(241);
        c.send_to(peer, b"x", later);
        let _ = c.poll_transmit();
        c.handle_timeout(later);
        let mut methods = vec![];
        while let Some(o) = c.poll_transmit() {
            methods.push(Message::decode(&o).unwrap().method);
        }
        assert!(methods.contains(&stun::CREATE_PERMISSION), "{methods:?}");
    }

    #[test]
    fn reliable_pads_channel_data_and_close_deallocates() {
        let (mut c, s, now) = allocated(true);
        let peer: SocketAddr = "192.0.2.80:7000".parse().unwrap();
        c.send_to(peer, b"q", now);
        let other = pump(&mut c, &s, now);
        assert_eq!(other.len(), 1); // the Send indication
        c.send_to(peer, b"abcde", now);
        let cd = c.poll_transmit().unwrap();
        assert_eq!(cd.len(), 12);
        assert_eq!(&cd[..4], &[0x40, 0, 0, 5]);
        c.close(now);
        let r = Message::decode(&c.poll_transmit().unwrap()).unwrap();
        assert_eq!((r.method, r.u32_attr(stun::ATTR_LIFETIME)), (stun::REFRESH, Some(0)));
        assert_eq!(c.relay(), None);
    }

    #[test]
    fn spoofed_response_is_dropped() {
        let s = Server::new();
        let mut c = TurnClient::new(creds(), false);
        let now = Instant::now();
        c.start(now);
        let first = c.poll_transmit().unwrap();
        c.handle_input(&s.answer(&first), now);
        let second = c.poll_transmit().unwrap();
        let m = Message::decode(&second).unwrap();
        // A success for our transaction with a wrong MESSAGE-INTEGRITY: ignored, still allocating.
        let fake = Builder::new(stun::ALLOCATE, Class::Success, m.tid)
            .xor_addr(stun::ATTR_XOR_RELAYED_ADDRESS, "6.6.6.6:6".parse().unwrap())
            .build(Some(b"not-the-key"), true);
        assert!(c.handle_input(&fake, now));
        assert_eq!(c.poll_event(), None);
        assert!(c.handle_input(&s.answer(&second), now));
        assert!(matches!(c.poll_event(), Some(TurnEvent::Allocated { .. })));
    }

    #[test]
    fn framer_splits_stream() {
        let tid = stun::new_tid();
        let a = Builder::new(stun::ALLOCATE, Class::Success, tid).u32(stun::ATTR_LIFETIME, 1).build(None, true);
        let cd = vec![0x40, 0x01, 0x00, 0x03, 1, 2, 3, 0];
        let mut all = a.clone();
        all.extend_from_slice(&cd);
        let mut f = Framer::default();
        // Byte by byte: the same two frames.
        let mut frames = vec![];
        for b in &all {
            frames.extend(f.push(std::slice::from_ref(b)));
        }
        assert_eq!(frames, vec![a.clone(), cd.clone()]);
        assert_eq!(Framer::default().push(&all), vec![a, cd]);
    }

    #[test]
    fn urls() {
        let u = parse_url("turns:turn.cloudflare.com:443?transport=tcp").unwrap();
        assert_eq!((u.scheme, u.host.as_str(), u.port, u.transport), (Scheme::Turns, "turn.cloudflare.com", 443, Transport::Tls));
        let u = parse_url("turn:turn.cloudflare.com:3478?transport=udp").unwrap();
        assert_eq!((u.port, u.transport), (3478, Transport::Udp));
        let u = parse_url("turn:turn.cloudflare.com:80?transport=tcp").unwrap();
        assert_eq!((u.port, u.transport), (80, Transport::Tcp));
        let u = parse_url("stun:stun.cloudflare.com").unwrap();
        assert_eq!((u.scheme, u.port, u.transport), (Scheme::Stun, 3478, Transport::Udp));
        let u = parse_url("turns:example.org").unwrap();
        assert_eq!((u.port, u.transport), (5349, Transport::Tls));
        let u = parse_url("turn:[2001:db8::1]:3479").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("2001:db8::1", 3479));
        for bad in [
            "http://x",
            "stun:",
            "stun:x:0",
            "stun:x?transport=udp",
            "turns:x?transport=udp",
            "turn:user@x",
            "turn:x:99999",
            "turn:x?foo=bar",
            "turn:[::1",
        ] {
            assert_eq!(parse_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn debug_redacts_credentials() {
        let c = TurnClient::new(creds(), false);
        let s = format!("{:?} {:?}", c, creds());
        assert!(!s.contains("test-user") && !s.contains("test-pass"), "{s}");
    }
}
