use anyhow::{Context, Result, ensure};
use netmon::{collector, model::*, runtime::CaptureTask, storage};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
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
            "netmon-probe --self-test [--output PATH]\nRun elevated. Creates local TCP/UDP traffic and two DNS queries; saves a report."
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
    let status = std::process::Command::new(std::env::current_exe()?)
        .arg("--traffic")
        .status()?;
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
            "DNS {} {} {} -> {}",
            d.process.exe, d.name, d.status, d.results
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
            .any(|d| d.name.trim_end_matches('.') == "example.com"),
        "No process-attributed DNS result"
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
