//! Discovers listening sockets by reading `/proc/net/*` and maps them to
//! owning processes through `/proc/<pid>/fd`.

use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const TCP_LISTEN: u8 = 0x0A;
const TCP_CLOSE: u8 = 0x07; // unconnected UDP sockets report this state

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn label(self) -> &'static str {
        match self {
            Proto::Tcp => "TCP",
            Proto::Udp => "UDP",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub name: String,
    pub cmdline: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub proto: Proto,
    pub addr: IpAddr,
    pub port: u16,
    pub user: String,
    pub process: Option<Process>,
}

impl Listener {
    pub fn address_label(&self) -> String {
        match self.addr {
            IpAddr::V4(a) if a.is_unspecified() => "0.0.0.0 (all IPv4)".into(),
            IpAddr::V6(a) if a.is_unspecified() => ":: (all IPv6)".into(),
            IpAddr::V6(a) => format!("[{a}]"),
            IpAddr::V4(a) => a.to_string(),
        }
    }
}

struct RawSocket {
    proto: Proto,
    addr: IpAddr,
    port: u16,
    uid: u32,
    inode: u64,
}

pub fn scan() -> Vec<Listener> {
    let mut raw = Vec::new();
    for (file, proto, v6) in [
        ("/proc/net/tcp", Proto::Tcp, false),
        ("/proc/net/tcp6", Proto::Tcp, true),
        ("/proc/net/udp", Proto::Udp, false),
        ("/proc/net/udp6", Proto::Udp, true),
    ] {
        if let Ok(text) = fs::read_to_string(file) {
            raw.extend(text.lines().skip(1).filter_map(|l| parse_line(l, proto, v6)));
        }
    }

    let owners = socket_owners();
    let users = user_names();

    let mut listeners: Vec<Listener> = raw
        .into_iter()
        .map(|s| Listener {
            proto: s.proto,
            addr: s.addr,
            port: s.port,
            user: users.get(&s.uid).cloned().unwrap_or_else(|| s.uid.to_string()),
            process: owners.get(&s.inode).cloned(),
        })
        .collect();

    listeners.sort_by(|a, b| {
        (a.port, a.proto, a.addr.is_ipv6(), a.addr).cmp(&(b.port, b.proto, b.addr.is_ipv6(), b.addr))
    });
    listeners.dedup();
    listeners
}

fn parse_line(line: &str, proto: Proto, v6: bool) -> Option<RawSocket> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 10 {
        return None;
    }
    let state = u8::from_str_radix(f[3], 16).ok()?;
    let wanted = match proto {
        Proto::Tcp => TCP_LISTEN,
        Proto::Udp => TCP_CLOSE,
    };
    if state != wanted {
        return None;
    }

    let (local_addr, local_port) = f[1].split_once(':')?;
    let port = u16::from_str_radix(local_port, 16).ok()?;
    if port == 0 {
        return None;
    }
    // A UDP socket with a remote peer is a client connection, not a listener.
    if proto == Proto::Udp && !f[2].ends_with(":0000") {
        return None;
    }

    Some(RawSocket {
        proto,
        addr: parse_addr(local_addr, v6)?,
        port,
        uid: f[7].parse().ok()?,
        inode: f[9].parse().ok()?,
    })
}

/// The kernel prints addresses as native-endian 32-bit words in hex.
fn parse_addr(hex: &str, v6: bool) -> Option<IpAddr> {
    if !v6 {
        let word = u32::from_str_radix(hex, 16).ok()?;
        return Some(IpAddr::V4(Ipv4Addr::from(word.to_ne_bytes())));
    }
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for i in 0..4 {
        let word = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_ne_bytes());
    }
    let v6 = Ipv6Addr::from(bytes);
    Some(match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    })
}

/// Maps socket inode -> owning process. Processes of other users are only
/// visible when running as root.
fn socket_owners() -> HashMap<u64, Process> {
    let mut map = HashMap::new();
    let Ok(procs) = fs::read_dir("/proc") else {
        return map;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut process = None;
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|t| t.parse::<u64>().ok())
            else {
                continue;
            };
            let p = process.get_or_insert_with(|| read_process(pid));
            map.entry(inode).or_insert_with(|| p.clone());
        }
    }
    map
}

fn read_process(pid: i32) -> Process {
    let name = fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim_end().to_owned())
        .unwrap_or_default();
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Process { pid, name, cmdline }
}

fn user_names() -> HashMap<u32, String> {
    fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, name.to_owned()))
        })
        .collect()
}

#[derive(Clone, Copy)]
pub enum Signal {
    Term,
    Kill,
}

impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "TERM",
            Signal::Kill => "KILL",
        }
    }
}

pub enum KillError {
    PermissionDenied,
    Other(std::io::Error),
}

pub fn send_signal(pid: i32, sig: Signal) -> Result<(), KillError> {
    let signo = match sig {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid, signo) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EPERM) => Err(KillError::PermissionDenied),
        _ => Err(KillError::Other(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_line() {
        let line = "   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0";
        let s = parse_line(line, Proto::Tcp, false).unwrap();
        assert_eq!(s.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(s.port, 3000);
        assert_eq!(s.uid, 1000);
        assert_eq!(s.inode, 12345);
    }

    #[test]
    fn skips_established() {
        let line = "   0: 0100007F:0BB8 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 12345 1";
        assert!(parse_line(line, Proto::Tcp, false).is_none());
    }

    #[test]
    fn parses_ipv6_loopback() {
        let addr = parse_addr("00000000000000000000000001000000", true).unwrap();
        assert_eq!(addr, IpAddr::V6(Ipv6Addr::LOCALHOST));
    }
}
