//! Plain UDP DNS over IPv4. Only decoded DNS metadata crosses the capture queue.
use crate::model::{DnsQuery, ProcessInfo, UDP_DNS_SOURCE};
use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Question {
    client: SocketAddr,
    server: SocketAddr,
    id: u16,
    name: String,
    rr_type: u16,
}

#[derive(Clone, Debug)]
pub struct Datagram {
    question: Question,
    length: u32,
    answer: Option<(u32, String)>,
}

impl Datagram {
    pub(crate) fn is_reply(&self) -> bool {
        self.answer.is_some()
    }
}

fn word(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn name(data: &[u8], pos: &mut usize) -> Option<String> {
    let mut at = *pos;
    let mut end = None;
    let mut output = String::new();
    let mut wire_len = 1;
    let mut jumps = 0;
    loop {
        let size = *data.get(at)? as usize;
        if size & 0xc0 == 0xc0 {
            let target = (word(data, at)? & 0x3fff) as usize;
            jumps += 1;
            if target < 12 || target >= at || jumps > 32 {
                return None;
            }
            end.get_or_insert(at + 2);
            at = target;
            continue;
        }
        if size > 63 {
            return None;
        }
        at += 1;
        if size == 0 {
            *pos = end.unwrap_or(at);
            return Some(output);
        }
        wire_len += size + 1;
        if wire_len > 255 {
            return None;
        }
        if !output.is_empty() {
            output.push('.');
        }
        for &ch in data.get(at..at + size)? {
            if ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_') {
                output.push(ch.to_ascii_lowercase() as char);
            } else {
                use std::fmt::Write;
                write!(output, "\\{ch:03}").ok()?;
            }
        }
        at += size;
    }
}

pub(crate) fn parse_ipv4(packet: &[u8]) -> Option<Datagram> {
    if packet.len() < 28 || packet[0] >> 4 != 4 || packet[9] != 17 || word(packet, 6)? & 0x3fff != 0
    {
        return None;
    }
    let header_len = (packet[0] as usize & 15) * 4;
    if header_len < 20 {
        return None;
    }
    let udp = packet.get(header_len..word(packet, 2)? as usize)?;
    let port_from = word(udp, 0)?;
    let port_to = word(udp, 2)?;
    if port_from != 53 && port_to != 53 {
        return None;
    }
    let data = udp.get(8..word(udp, 4)? as usize)?;
    let flags = word(data, 2)?;
    let reply = flags & 0x8000 != 0;
    if (if reply { port_from } else { port_to }) != 53 || flags & 0x7a00 != 0 || word(data, 4)? != 1
    {
        return None;
    }
    let src = SocketAddr::from(([packet[12], packet[13], packet[14], packet[15]], port_from));
    let dst = SocketAddr::from(([packet[16], packet[17], packet[18], packet[19]], port_to));
    let mut pos = 12;
    let question = Question {
        client: if reply { dst } else { src },
        server: if reply { src } else { dst },
        id: word(data, 0)?,
        name: name(data, &mut pos)?,
        rr_type: word(data, pos)?,
    };
    if question.name.is_empty() || word(data, pos + 2)? != 1 {
        return None;
    }
    pos += 4;
    let answer = if reply {
        let count = word(data, 6)? as usize;
        let authority = word(data, 8)? as usize;
        let total = count + authority + word(data, 10)? as usize;
        if total > 512 {
            return None;
        }
        let mut aliases = HashMap::new();
        let mut addresses = Vec::new();
        let mut rcode = (flags & 15) as u32;
        for index in 0..total {
            let owner = name(data, &mut pos)?;
            let kind = word(data, pos)?;
            let class = word(data, pos + 2)?;
            let len = word(data, pos + 8)? as usize;
            pos += 10;
            let end = pos + len;
            let rdata = data.get(pos..end)?;
            if index < count && class == 1 {
                match kind {
                    1 if len == 4 => addresses.push((
                        owner,
                        Ipv4Addr::from(<[u8; 4]>::try_from(rdata).ok()?).to_string(),
                    )),
                    28 if len == 16 => addresses.push((
                        owner,
                        Ipv6Addr::from(<[u8; 16]>::try_from(rdata).ok()?).to_string(),
                    )),
                    5 => {
                        let mut cursor = pos;
                        let target = name(data, &mut cursor)?;
                        if cursor != end || aliases.insert(owner, target).is_some() {
                            return None;
                        }
                    }
                    _ => {}
                }
            } else if index >= count + authority && kind == 41 && owner.is_empty() {
                rcode |= (data[pos - 6] as u32) << 4;
            }
            pos = end;
        }
        let mut allowed = HashSet::from([question.name.as_str()]);
        let mut owner = &question.name;
        let mut results = Vec::new();
        while let Some(target) = aliases.get(owner) {
            if !allowed.insert(target.as_str()) {
                return None;
            }
            results.push(format!("CNAME {target}."));
            owner = target;
        }
        results.extend(
            addresses
                .into_iter()
                .filter_map(|(owner, ip)| allowed.contains(owner.as_str()).then_some(ip)),
        );
        if results.is_empty() && count > 0 {
            results.push(format!("Записей в ответе: {count}"));
        }
        Some((rcode, results.join("; ")))
    } else {
        None
    };
    Some(Datagram {
        question,
        length: data.len() as u32,
        answer,
    })
}

const MATCH_MS: i64 = 250;
const REPLY_MS: i64 = 15_000;
// During overload drop the correlation window as a whole: keeping only a subset
// of possible owners could falsely make a shared UDP port look unambiguous.
const LIMIT: usize = 16_384;
type PacketKey = (SocketAddr, SocketAddr, u32, bool);
type Owner = (u32, i64);

#[derive(Default)]
pub(crate) struct Tracker {
    kernel: HashMap<PacketKey, Vec<(i64, Owner)>>,
    kernel_count: usize,
    packets: Vec<(i64, Datagram)>,
    pending: HashMap<Question, Pending>,
    resume_after: i64,
    pub lost: u64,
}

struct Pending {
    at: i64,
    owner: Option<Owner>,
    repeated: bool,
}

impl Tracker {
    fn accept(&mut self, at: i64) -> bool {
        if at < self.resume_after {
            return false;
        }
        if self.kernel_count + self.packets.len() + self.pending.len() < LIMIT {
            return true;
        }
        self.kernel.clear();
        self.kernel_count = 0;
        self.packets.clear();
        self.pending.clear();
        self.lost += 1;
        self.resume_after = at + REPLY_MS;
        false
    }

    pub fn kernel(
        &mut self,
        at: i64,
        local: SocketAddr,
        remote: SocketAddr,
        bytes: u32,
        reply: bool,
        p: &ProcessInfo,
    ) {
        if self.accept(at) {
            self.kernel
                .entry((local, remote, bytes, reply))
                .or_default()
                .push((at, (p.pid, p.started)));
            self.kernel_count += 1;
        }
    }

    pub fn packet(&mut self, at: i64, datagram: Datagram) {
        if self.accept(at) {
            self.packets.push((at, datagram));
        }
    }

    pub fn drain(&mut self, through: i64, mut emit: impl FnMut(Owner, DnsQuery)) {
        self.packets.sort_by_key(|(at, _)| *at);
        let ready = through - MATCH_MS;
        let n = self.packets.partition_point(|(at, _)| *at <= ready);
        for (at, packet) in self.packets.drain(..n) {
            let q = packet.question;
            let key = (q.client, q.server, packet.length, packet.answer.is_some());
            let mut owners = self
                .kernel
                .get(&key)
                .into_iter()
                .flatten()
                .filter(|(time, _)| at.abs_diff(*time) <= MATCH_MS as u64)
                .map(|(_, owner)| *owner);
            let owner = owners
                .next()
                .filter(|first| first.1 != 0 && owners.all(|other| other == *first));
            if let Some((status, results)) = packet.answer {
                if let Some(request) = self.pending.remove(&q)
                    && let Some(client) = request.owner
                    && Some(client) == owner
                    && (0..=REPLY_MS).contains(&(at - request.at))
                {
                    emit(
                        client,
                        DnsQuery {
                            id: 0,
                            at,
                            process: ProcessInfo::default(),
                            name: q.name,
                            query_type: q.rr_type as u32,
                            status,
                            results,
                            duration_ms: (!request.repeated).then_some(at - request.at),
                            source: format!("{UDP_DNS_SOURCE}{} · Kernel ETW", q.server),
                        },
                    );
                }
            } else {
                let fresh = Pending {
                    at,
                    owner,
                    repeated: false,
                };
                self.pending
                    .entry(q)
                    .and_modify(|last| {
                        if at - last.at > REPLY_MS {
                            last.owner = owner;
                            last.repeated = false;
                        } else {
                            // Do not invent a duration for a retransmitted question.
                            last.repeated = true;
                            if last.owner != owner {
                                last.owner = None;
                            }
                        }
                        last.at = at;
                    })
                    .or_insert(fresh);
            }
        }
        self.pending.retain(|_, p| ready - p.at <= REPLY_MS);
        self.kernel_count = 0;
        self.kernel.retain(|_, events| {
            events.retain(|(at, _)| *at >= ready - MATCH_MS);
            self.kernel_count += events.len();
            !events.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(reply: bool, id: u16, kind: u16, tail: &[u8], count: u16) -> Vec<u8> {
        let mut data = Vec::new();
        for value in [id, if reply { 0x8180 } else { 0x0100 }, 1, count, 0, 0] {
            data.extend(value.to_be_bytes());
        }
        data.extend_from_slice(b"\x03www\x07example\x04test\0");
        data.extend(kind.to_be_bytes());
        data.extend(1u16.to_be_bytes());
        data.extend(tail);
        let mut packet = vec![0; 28];
        packet[0] = 0x45;
        packet[9] = 17;
        packet[2..4].copy_from_slice(&((28 + data.len()) as u16).to_be_bytes());
        let (source, dest) = if reply { (53u16, 5000u16) } else { (5000, 53) };
        packet[12..16].copy_from_slice(&[10, 0, 0, if reply { 53 } else { 1 }]);
        packet[16..20].copy_from_slice(&[10, 0, 0, if reply { 1 } else { 53 }]);
        packet[20..22].copy_from_slice(&source.to_be_bytes());
        packet[22..24].copy_from_slice(&dest.to_be_bytes());
        packet[24..26].copy_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        packet.extend(data);
        packet
    }

    #[test]
    fn ipv4_and_compressed_dns_answers_follow_only_the_question() {
        // www.example.test -> node.example.test; unrelated address must stay out.
        let rr = b"\xc0\x0c\x00\x05\x00\x01\x00\x00\x00\x3c\x00\x07\x04node\xc0\x10\x04node\xc0\x10\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\xc0\x00\x02\x01\x03bad\xc0\x10\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\xc0\x00\x02\xff";
        let p = parse_ipv4(&packet(true, 12, 1, rr, 3)).unwrap();
        assert_eq!(p.question.name, "www.example.test");
        assert_eq!(
            p.answer,
            Some((0, "CNAME node.example.test.; 192.0.2.1".into()))
        );
        let mut rr6 = b"\xc0\x0c\x00\x1c\x00\x01\x00\x00\x00\x00\x00\x10".to_vec();
        rr6.extend("2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(
            parse_ipv4(&packet(true, 12, 28, &rr6, 1)).unwrap().answer,
            Some((0, "2001:db8::1".into()))
        );
    }

    #[test]
    fn malformed_truncated_fragmented_and_non_dns_traffic_is_ignored() {
        let original = packet(true, 1, 1, &[], 0);
        for (offset, value) in [
            (0, 0x4f),
            (6, 0x20),
            (9, 6),
            (20, 1),
            (30, 0x83),
            (40, 0xc0),
        ] {
            let mut bytes = original.clone();
            bytes[offset] = value;
            assert!(parse_ipv4(&bytes).is_none(), "offset {offset}");
        }
        for limit in 0..original.len() {
            assert!(parse_ipv4(&original[..limit]).is_none());
        }
        let mut nx = original;
        nx[31] = 0x83;
        assert_eq!(parse_ipv4(&nx).unwrap().answer, Some((3, String::new())));
    }

    fn observe(t: &mut Tracker, reply: bool, id: u16, at: i64, p: &ProcessInfo) {
        let wire = parse_ipv4(&packet(reply, id, 1, &[], 0)).unwrap();
        t.kernel(
            at + 3,
            wire.question.client,
            wire.question.server,
            wire.length,
            reply,
            p,
        );
        t.packet(at, wire);
    }

    #[test]
    fn attribution_requires_query_reply_and_unambiguous_process_instance_on_both() {
        let a = ProcessInfo {
            pid: 42,
            started: 1,
            ..Default::default()
        };
        let b = ProcessInfo {
            pid: 42,
            started: 25,
            ..Default::default()
        };
        for (other_client, other_id, too_late, shared_port, expected) in [
            (false, false, false, false, 1),
            (true, false, false, false, 0),
            (false, true, false, false, 0),
            (false, false, true, false, 0),
            (false, false, false, true, 0),
        ] {
            let mut tracker = Tracker::default();
            observe(&mut tracker, false, 5, 10, &a);
            observe(
                &mut tracker,
                true,
                if other_id { 6 } else { 5 },
                if too_late { 20_000 } else { 30 },
                if other_client { &b } else { &a },
            );
            if shared_port {
                let q = &tracker.packets[0].1;
                tracker.kernel(
                    12,
                    q.question.client,
                    q.question.server,
                    q.length,
                    false,
                    &b,
                );
            }
            let mut count = 0;
            tracker.drain(i64::MAX, |owner, d| {
                assert_eq!(owner, (a.pid, a.started));
                assert_eq!(d.duration_ms, Some(20));
                count += 1;
            });
            assert_eq!(count, expected);
        }
        let mut tracker = Tracker::default();
        observe(&mut tracker, true, 5, 100, &a);
        tracker.drain(i64::MAX, |_, _| panic!("unsolicited reply"));
    }
}
