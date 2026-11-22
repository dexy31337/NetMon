use anyhow::{Context, Result, ensure};
use netmon::{collector, model::*, runtime::CaptureTask, storage};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
    path::PathBuf,
    thread,
    time::Duration,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|a| a == "--traffic") {
        return traffic();
    }
    if !args.iter().any(|a| a == "--self-test") {
        println!(
            "netmon-probe --self-test [--output PATH]\nRun elevated. Creates TCP/UDP traffic, Windows resolver lookups and direct A/AAAA UDP DNS queries to the configured IPv4 resolver."
        );
        return Ok(());
    }
    ensure!(
        collector::is_elevated(),
        "Run this probe from an elevated terminal (Administrator)."
    );
    let output = args
        .windows(2)
        .find(|a| a[0] == "--output")
        .map(|a| PathBuf::from(&a[1]))
        .unwrap_or_else(|| PathBuf::from("artifacts/probe"));
    std::fs::create_dir_all(&output)?;
    let task = CaptureTask::start(
        CaptureConfig {
            exe_filters: vec!["netmon-probe.exe".into()],
            include_children: false,
            udp_idle_seconds: 3,
            label: "Проверка TCP/UDP/DNS".into(),
        },
        output.join("data"),
    );
    for _ in 0..100 {
        if task.finished() {
            anyhow::bail!("Collector failed: {:?}", task.snapshot().error);
        }
        if task.snapshot().running {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure!(task.snapshot().running, "Collector startup timeout");
    // A child verifies process-start attribution as well as traffic correlation.
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--traffic")
        .spawn()?;
    let child_pid = child.id();
    let status = child.wait()?;
    ensure!(status.success(), "Traffic generator failed");
    thread::sleep(Duration::from_secs(4));
    task.stop();
    for _ in 0..200 {
        if task.finished() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure!(task.finished(), "Collector shutdown timeout");
    let snapshot = task.snapshot();
    serde_json::to_writer_pretty(
        std::fs::File::create(output.join("report.json"))?,
        &*snapshot,
    )?;
    let exported = storage::export(&snapshot, &output)?;
    println!("Export: {}", exported.display());
    println!(
        "Flows: {}, DNS: {}, error: {:?}",
        snapshot.flows.len(),
        snapshot.dns.len(),
        snapshot.error
    );
    let health = &snapshot.recording.as_ref().unwrap().health;
    println!(
        "Events: {}, ETW loss: {}, queue loss: {}, decode errors: {}",
        health.events, health.etw_lost, health.queue_lost, health.decode_errors
    );
    for f in &snapshot.flows {
        println!(
            "{} {} {:?} -> {} {:?} tx={} rx={} source={} error={:?}",
            f.process.exe,
            f.protocol,
            f.local,
            f.remote,
            f.outcome,
            f.sent,
            f.received,
            f.source,
            f.error
        );
    }
    for d in &snapshot.dns {
        println!(
            "DNS {} (PID {}) {} type={} status={} -> {} [{}]",
            d.process.exe, d.process.pid, d.name, d.query_type, d.status, d.results, d.source
        );
    }
    ensure!(snapshot.error.is_none(), "Capture error");
    ensure!(
        snapshot
            .flows
            .iter()
            .any(|f| f.protocol == Protocol::Tcp && f.outcome == Outcome::Established),
        "No successful TCP connection"
    );
    ensure!(
        snapshot
            .flows
            .iter()
            .any(|f| f.protocol == Protocol::Tcp && f.outcome == Outcome::Failed),
        "No failed TCP attempt detected"
    );
    ensure!(
        snapshot
            .flows
            .iter()
            .any(|f| f.protocol == Protocol::Udp && f.sent > 0 && f.received > 0),
        "No bidirectional UDP flow"
    );
    ensure!(
        snapshot
            .flows
            .iter()
            .any(|f| f.protocol == Protocol::Tcp && f.sent == 65536 && f.received == 32768),
        "TCP byte counters do not match exact transfer size"
    );
    ensure!(
        snapshot
            .dns
            .iter()
            .any(|d| d.name.trim_end_matches('.') == "example.com"
                && d.process.pid == child_pid
                && d.status == 0
                && !d.results.is_empty()
                && d.source.ends_with("/ 3008")),
        "No successful Windows DNS Client result attributed to the child"
    );
    for kind in [1, 28] {
        ensure!(
            snapshot.dns.iter().any(|d| d.name == "example.org"
                && d.query_type == kind
                && d.process.pid == child_pid
                && d.source.starts_with("UDP/53")
                && d.status == 0
                && (kind == 28
                    || d.results
                        .split(';')
                        .any(|s| matches!(s.trim().parse::<IpAddr>(), Ok(IpAddr::V4(_)))))),
            "No verified direct UDP/53 type {kind} result attributed to child PID {child_pid}"
        );
    }
    ensure!(
        snapshot.dns.iter().any(|d| d.name == "netmon-probe.invalid"
            && d.process.pid == child_pid
            && d.source.starts_with(UDP_DNS_SOURCE)
            && d.status == 3
            && d.status_label() == "NXDOMAIN"),
        "No direct NXDOMAIN (wire RCODE 3) attributed to child PID {child_pid}"
    );
    ensure!(!health.incomplete(), "Event loss or decode errors");
    ensure!(
        snapshot
            .flows
            .iter()
            .any(|f| f.protocol == Protocol::Udp && f.sent == 123 && f.received == 71),
        "UDP exact byte counts mismatch"
    );
    ensure!(
        snapshot.flows.iter().any(|f| f.protocol == Protocol::Udp
            && f.sent == 37
            && f.received == 0
            && f.outcome == Outcome::Datagram),
        "One-way UDP misclassified"
    );
    ensure!(
        snapshot
            .flows
            .iter()
            .all(|f| f.process.pid != std::process::id()),
        "Unexpected parent-process attribution"
    );
    println!(
        "PASS: ETW, TCP success/failure, exact TCP/UDP bytes, DNS, child-process attribution, storage, export."
    );
    Ok(())
}

fn traffic() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = thread::spawn(move || -> std::io::Result<()> {
        let (mut s, _) = listener.accept()?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut data = vec![0; 65536];
        s.read_exact(&mut data)?;
        s.write_all(&vec![7; 32768])?;
        Ok(())
    });
    let mut client = TcpStream::connect(addr)?;
    client.set_read_timeout(Some(Duration::from_secs(5)))?;
    client.write_all(&vec![3; 65536])?;
    client.read_exact(&mut vec![0; 32768])?;
    drop(client);
    server.join().unwrap()?;
    // Reserve then release an ephemeral port; connecting to it should be refused.
    let unused = TcpListener::bind("127.0.0.1:0")?;
    let addr = unused.local_addr()?;
    drop(unused);
    ensure!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(),
        "Expected refused TCP connection"
    );
    let server = UdpSocket::bind("127.0.0.1:0")?;
    let client = UdpSocket::bind("127.0.0.1:0")?;
    server.set_read_timeout(Some(Duration::from_secs(3)))?;
    client.set_read_timeout(Some(Duration::from_secs(3)))?;
    client.send_to(&[5; 123], server.local_addr()?)?;
    let mut buf = [0; 256];
    let (_, peer) = server.recv_from(&mut buf)?;
    server.send_to(&[6; 71], peer)?;
    client.recv_from(&mut buf)?;
    // Independent one-way UDP flow: lack of an answer must not become a failure.
    let sink = UdpSocket::bind("127.0.0.1:0")?;
    client.send_to(&[8; 37], sink.local_addr()?)?;
    let _ = ("example.com", 443)
        .to_socket_addrs()
        .context("DNS example.com")?;
    let _ = (format!("netmon-{}.invalid", now_ms()).as_str(), 443).to_socket_addrs();
    direct_dns()?;
    // IPv6 loopback is exercised where the host has it enabled.
    if let Ok(listener) = TcpListener::bind("[::1]:0") {
        let addr = listener.local_addr()?;
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut b = [0; 17];
            s.read_exact(&mut b).unwrap();
            s.write_all(&b).unwrap();
        });
        let mut client = TcpStream::connect(addr)?;
        client.set_read_timeout(Some(Duration::from_secs(3)))?;
        client.write_all(&[1; 17])?;
        client.read_exact(&mut [0; 17])?;
        drop(client);
        server.join().unwrap();
    }
    Ok(())
}

fn direct_dns() -> Result<()> {
    use windows_sys::Win32::{
        Foundation::ERROR_BUFFER_OVERFLOW,
        NetworkManagement::IpHelper::{FIXED_INFO_W2KSP1, GetNetworkParams},
    };
    let mut len = 0;
    ensure!(
        unsafe { GetNetworkParams(std::ptr::null_mut(), &mut len) } == ERROR_BUFFER_OVERFLOW,
        "Cannot size DNS settings"
    );
    let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
    let settings = buf.as_mut_ptr().cast::<FIXED_INFO_W2KSP1>();
    ensure!(
        unsafe { GetNetworkParams(settings, &mut len) } == 0,
        "Cannot read DNS settings"
    );
    let mut entry = unsafe { &raw const (*settings).DnsServerList };
    let mut server = None;
    while !entry.is_null() {
        let info = unsafe { &*entry };
        let bytes: Vec<u8> = info
            .IpAddress
            .String
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as u8)
            .collect();
        if let Ok(ip) = std::str::from_utf8(&bytes)?.parse::<Ipv4Addr>()
            && !ip.is_unspecified()
        {
            server = Some(ip);
            break;
        }
        entry = info.Next;
    }
    let server = server.context("No configured IPv4 DNS server")?;
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.connect((server, 53))?;
    sock.set_read_timeout(Some(Duration::from_secs(4)))?;
    for (question, kind) in [
        ("example.org", 1u16),
        ("example.org", 28),
        ("netmon-probe.invalid", 1),
    ] {
        let id = (now_ms() as u16).wrapping_add(kind);
        let mut data = id.to_be_bytes().to_vec();
        data.extend_from_slice(b"\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00");
        for label in question.split('.') {
            data.push(label.len() as u8);
            data.extend_from_slice(label.as_bytes());
        }
        data.push(0);
        data.extend_from_slice(&kind.to_be_bytes());
        data.extend_from_slice(&1u16.to_be_bytes());
        sock.send(&data)?;
        let mut answer = [0; 4096];
        let n = sock.recv(&mut answer).context("Direct UDP DNS reply")?;
        ensure!(
            n >= 12 && answer[..2] == id.to_be_bytes() && answer[2] & 0x80 != 0,
            "Invalid direct DNS reply"
        );
    }
    Ok(())
}
