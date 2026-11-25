// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Remote ICE candidates in the shape str0m 0.25 parses (G7, 2026-11-24).
//!
//! str0m's SDP grammar takes a candidate's optional tails only in one order (raddr/rport, tcptype,
//! generation, network-id, ufrag, network-cost) followed by the end of the line. Any other order or
//! any other tail fails the whole `a=candidate` line, and str0m keeps it as an unknown attribute:
//! the candidate is dropped with no error, ICE never gets a pair and sends no binding request.
//! Cloudflare Calls' answers were dropped that way. These functions rewrite a candidate into
//! str0m's order and drop the tails it doesn't know; every other SDP line is left as it was.
//!
//! - [`normalize_candidate_line`]: one candidate, as an `a=candidate` value or as it arrives via
//!   addIceCandidate (with or without `a=` / `candidate:`). Returns `candidate:…`, or None when
//!   the line isn't a candidate at all.
//! - [`normalize_sdp`]: a whole remote description, each `a=candidate` line through the above.

use std::net::IpAddr;

/// The tails str0m knows, in the order it reads them (raddr and rport go together).
const ORDER: [&str; 5] = ["tcptype", "generation", "network-id", "ufrag", "network-cost"];

/// One candidate in str0m's order (`candidate:<foundation> <component> <proto> <prio> <ip> <port>
/// typ <kind> [raddr <ip> rport <port>] [tcptype <t>] [generation <g>] [network-id <n>]
/// [ufrag <u>] [network-cost <c>]`), unknown tails dropped. None when the base isn't a candidate.
pub fn normalize_candidate_line(value: &str) -> Option<String> {
    let v = value.trim();
    let v = v.strip_prefix("a=").unwrap_or(v);
    let v = v.strip_prefix("candidate:").unwrap_or(v);
    let t: Vec<&str> = v.split_ascii_whitespace().collect();
    if t.len() < 8 || !t[6].eq_ignore_ascii_case("typ") {
        return None;
    }
    let (foundation, component, proto, prio, ip, port) = (t[0], t[1], t[2].to_ascii_lowercase(), t[3], t[4], t[5]);
    component.parse::<u16>().ok()?;
    prio.parse::<u32>().ok()?;
    ip.parse::<IpAddr>().ok()?;
    port.parse::<u16>().ok()?;
    if proto != "udp" && proto != "tcp" && proto != "ssltcp" && proto != "tls" {
        return None;
    }
    let kind = t[7].to_ascii_lowercase();
    if !matches!(kind.as_str(), "host" | "srflx" | "prflx" | "relay") {
        return None;
    }
    // The tails: name value pairs after the kind (a dangling name with no value is dropped).
    let tails: Vec<(&str, &str)> = t[8..].chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect();
    let get = |k: &str| tails.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| *v);

    let mut out = format!("candidate:{foundation} {component} {proto} {prio} {ip} {port} typ {kind}");
    if let (Some(ra), Some(rp)) = (get("raddr"), get("rport")) {
        if ra.parse::<IpAddr>().is_ok() && rp.parse::<u16>().is_ok() {
            out.push_str(&format!(" raddr {ra} rport {rp}"));
        }
    }
    for name in ORDER {
        let Some(val) = get(name) else { continue };
        // str0m reads tcptype through TcpType::from_str: only the three RFC 6544 values.
        if name == "tcptype" && !matches!(val, "active" | "passive" | "so") {
            continue;
        }
        out.push(' ');
        out.push_str(name);
        out.push(' ');
        out.push_str(val);
    }
    Some(out)
}

/// A remote description with each `a=candidate` line in str0m's shape; every other line (and the
/// line endings) as it was. A candidate line that isn't one is left for str0m to ignore as before.
pub fn normalize_sdp(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len() + 16);
    for line in sdp.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let end = &line[body.len()..];
        if body.starts_with("a=candidate:") {
            if let Some(n) = normalize_candidate_line(body) {
                out.push_str("a=");
                out.push_str(&n);
                out.push_str(end);
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// The `a=candidate` lines in a description (what the remote sent, parsed or not).
pub fn candidate_lines(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.trim_start().starts_with("a=candidate:")).count()
}

/// The candidates str0m parsed from a remote description (an SdpOffer or SdpAnswer: str0m's Sdp
/// type isn't nameable outside it, its fields are public).
#[macro_export]
macro_rules! sdp_parsed_candidates {
    ($sdp:expr) => {
        $sdp.session.ice_candidates().count() +
            $sdp.media_lines.iter().map(|m| m.ice_candidates().count()).sum::<usize>()
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use str0m::change::SdpAnswer;
    use str0m::media::{Direction, MediaKind};
    use str0m::{Candidate, RtcConfig};

    /// Cloudflare Calls' answer candidate (b903b18f / calls-4c, the session's ufrag replaced by a fake).
    const CALLS: &str = "a=candidate:513273236 1 udp 2130648831 141.101.90.0 1473 typ host ufrag 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef generation 0";
    /// ice-repro's Calls-shaped line (str0m's own candidate plus " ufrag <64 hex>").
    const REPRO: &str = "a=candidate:fffeff7e2d8bf3a11b834bbb 1 udp 2130706175 141.101.90.0 1473 typ host ufrag a8ff1bc9cbb9e00c7055ebd39d46f0ee57680b9605292b8286d63aa8d90b4994";
    /// Chrome's shape: generation, ufrag, network-id, network-cost (network-id after ufrag: str0m's order breaks).
    const CHROME: &str = "a=candidate:3348143458 1 udp 2122260223 192.168.1.20 54321 typ host generation 0 ufrag AbCd network-id 1 network-cost 10";
    /// A srflx with a tail str0m doesn't know, first.
    const UNKNOWN: &str = "a=candidate:842163049 1 udp 1677729535 203.0.113.7 61000 typ srflx x-made-up 7 raddr 10.0.0.5 rport 40000 generation 0";

    fn one(line: &str) -> String {
        let n = normalize_candidate_line(line).expect("a candidate");
        Candidate::from_sdp_string(&n).unwrap_or_else(|e| panic!("is can't parse {n:?}: {e:?}"));
        n
    }

    #[test]
    fn candidate_line_forms() {
        assert_eq!(
            one(CALLS),
            "candidate:513273236 1 udp 2130648831 141.101.90.0 1473 typ host generation 0 ufrag 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(one(REPRO), REPRO.trim_start_matches("a="));
        assert_eq!(
            one(CHROME),
            "candidate:3348143458 1 udp 2122260223 192.168.1.20 54321 typ host generation 0 network-id 1 ufrag AbCd network-cost 10"
        );
        assert_eq!(
            one(UNKNOWN),
            "candidate:842163049 1 udp 1677729535 203.0.113.7 61000 typ srflx raddr 10.0.0.5 rport 40000 generation 0"
        );
        // As addIceCandidate hands it: no "a=", with or without "candidate:".
        assert_eq!(one(&CALLS[2..]), one(CALLS));
        assert_eq!(one(&CALLS["a=candidate:".len()..]), one(CALLS));
        // Not candidates.
        assert_eq!(normalize_candidate_line(""), None);
        assert_eq!(normalize_candidate_line("a=ice-lite"), None);
        assert_eq!(normalize_candidate_line("candidate:1 1 udp 1 not-an-ip 9 typ host"), None);
        assert_eq!(normalize_candidate_line("candidate:1 1 udp 1 1.2.3.4 9 typ weird"), None);
    }

    /// A real answer (str0m's own, ice-lite like Calls) with its candidate line swapped for each case,
    /// normalised, then parsed the way webrtc.rs parses a remote answer: str0m must find the candidate.
    #[test]
    fn whole_sdp_candidates_parse() {
        let t0 = Instant::now();
        let mut off = RtcConfig::new().clear_codecs().enable_opus(true, false).build(t0);
        off.add_local_candidate(Candidate::host("51.81.208.25:5000".parse().unwrap(), "udp").unwrap());
        let mut ans = RtcConfig::new().set_ice_lite(true).clear_codecs().enable_opus(true, false).build(t0);
        ans.add_local_candidate(Candidate::host("141.101.90.0:1473".parse().unwrap(), "udp").unwrap());
        let mut api = off.sdp_api();
        api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, _pending) = api.apply().unwrap();
        let base = ans.sdp_api().accept_offer(offer).unwrap().to_sdp_string();
        let count = |sdp: &str| {
            let a = SdpAnswer::from_sdp_string(sdp).expect("answer parses");
            crate::sdp_parsed_candidates!(a)
        };
        for case in [CALLS, REPRO, CHROME, UNKNOWN] {
            let swapped: String = base
                .split_inclusive('\n')
                .map(|l| if l.starts_with("a=candidate:") { format!("{case}\r\n") } else { l.to_string() })
                .collect();
            assert_eq!(candidate_lines(&swapped), 1);
            let fixed = normalize_sdp(&swapped);
            assert_eq!(count(&fixed), 1, "no candidate parsed from {case:?}");
            // Only the candidate line changed.
            let other = |s: &str| s.lines().filter(|l| !l.starts_with("a=candidate:")).map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(other(&fixed), other(&swapped));
            assert!(fixed.ends_with("\r\n") == swapped.ends_with("\r\n"));
        }
        // The case that started this: unnormalised, str0m finds nothing.
        let raw: String = base
            .split_inclusive('\n')
            .map(|l| if l.starts_with("a=candidate:") { format!("{CALLS}\r\n") } else { l.to_string() })
            .collect();
        assert_eq!(count(&raw), 0, "str0m 0.25 parses Calls' line as it is: the normaliser may no longer be needed");
    }
}
