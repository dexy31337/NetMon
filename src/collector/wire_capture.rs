use super::{CapturedEvent, Counters};
use crate::{dns_wire::parse_ipv4, model::*};
use crossbeam_channel::Sender;
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, UdpSocket},
    os::windows::io::{AsRawSocket, FromRawSocket},
    ptr::null_mut,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::ERROR_INSUFFICIENT_BUFFER, NetworkManagement::IpHelper::*, Networking::WinSock::*,
};

fn interfaces() -> io::Result<BTreeMap<u32, Ipv4Addr>> {
    let mut needed = 0;
    let mut buf: Vec<u32> = Vec::new();
    loop {
        let table = if buf.is_empty() {
            null_mut()
        } else {
            buf.as_mut_ptr().cast::<MIB_IPADDRTABLE>()
        };
        let status = unsafe { GetIpAddrTable(table, &mut needed, 0) };
        if status == ERROR_INSUFFICIENT_BUFFER {
            buf.resize((needed as usize).div_ceil(4), 0);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let count = buf.first().copied().unwrap_or_default() as usize;
        let rows = buf.get(1..).unwrap_or_default();
        if count > std::mem::size_of_val(rows) / size_of::<MIB_IPADDRROW_XP>() {
            return Err(io::Error::other("Некорректная таблица адресов IPv4"));
        }
        let rows =
            unsafe { std::slice::from_raw_parts(rows.as_ptr().cast::<MIB_IPADDRROW_XP>(), count) };
        let mut result = BTreeMap::new();
        for row in rows {
            if row.dwAddr != 0
                && row.wType as u32 & (MIB_IPADDR_DELETED | MIB_IPADDR_DISCONNECTED) == 0
            {
                result
                    .entry(row.dwIndex)
                    .or_insert(Ipv4Addr::from(row.dwAddr.to_ne_bytes()));
            }
        }
        return Ok(result);
    }
}

fn socket_for(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let error = || io::Error::from_raw_os_error(unsafe { WSAGetLastError() });
    let raw = unsafe { socket(AF_INET as i32, SOCK_RAW, IPPROTO_IP) };
    if raw == INVALID_SOCKET {
        return Err(error());
    }
    let owned = unsafe { UdpSocket::from_raw_socket(raw as _) };
    let local = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(ip.octets()),
            },
        },
        ..Default::default()
    };
    if unsafe {
        bind(
            raw,
            (&raw const local).cast(),
            size_of::<SOCKADDR_IN>() as i32,
        )
    } != 0
    {
        return Err(error());
    }
    let mut returned = 0;
    let mode = RCVALL_IPLEVEL;
    if unsafe {
        WSAIoctl(
            owned.as_raw_socket() as _,
            SIO_RCVALL,
            (&raw const mode).cast(),
            size_of_val(&mode) as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
            None,
        )
    } != 0
    {
        return Err(error());
    }
    owned.set_read_timeout(Some(Duration::from_millis(150)))?;
    Ok(owned)
}

#[derive(Default)]
pub(super) struct Capture {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    messages: Arc<Mutex<Vec<String>>>,
}

impl Capture {
    pub fn start(&mut self, tx: Sender<CapturedEvent>, counters: Arc<Counters>) {
        // std::net initializes Winsock before we create the native raw sockets.
        let addresses = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).and_then(|_| interfaces());
        let addresses = match addresses {
            Ok(list) if !list.is_empty() => list,
            other => {
                let reason = other
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "нет IPv4-интерфейсов".into());
                self.messages
                    .lock()
                    .unwrap()
                    .push(format!("Захват DNS по UDP/IPv4 недоступен: {reason}"));
                return;
            }
        };
        for (index, ip) in addresses {
            let stop = self.stop.clone();
            let messages = self.messages.clone();
            let counters = counters.clone();
            let tx = tx.clone();
            let sock = match socket_for(ip) {
                Ok(sock) => sock,
                Err(err) => {
                    messages.lock().unwrap().push(format!(
                        "Захват DNS по UDP/IPv4 на интерфейсе {index} недоступен: {err}"
                    ));
                    continue;
                }
            };
            self.threads.push(thread::spawn(move || {
                let mut buf = vec![0; 65535];
                while !stop.load(Ordering::Relaxed) {
                    let size = match sock.recv(&mut buf) {
                        Ok(size) => size,
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(err) => {
                            messages.lock().unwrap().push(format!(
                                "Захват DNS по UDP/IPv4 на интерфейсе {index} прерван: {err}"
                            ));
                            break;
                        }
                    };
                    let at = now_ms();
                    if let Some(packet) = parse_ipv4(&buf[..size]) {
                        let source = if packet.is_reply() {
                            "UDP/53 reply"
                        } else {
                            "UDP/53 query"
                        };
                        *counters
                            .sources
                            .lock()
                            .unwrap()
                            .entry(source.into())
                            .or_default() += 1;
                        let event = CapturedEvent {
                            ticks: (at + 11_644_473_600_000) * 10_000,
                            event: Event::WireDns { at, packet },
                        };
                        if tx.try_send(event).is_err() {
                            counters.queue_lost.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
    }

    pub fn health(&self, h: &mut Health) {
        for message in self.messages.lock().unwrap().iter() {
            if !h.messages.contains(message) {
                h.messages.push(message.clone());
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}
