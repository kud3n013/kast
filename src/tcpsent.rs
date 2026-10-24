//! How much a process group has sent over TCP and had acknowledged by the other end: upload
//! progress for a child that reports none of its own (Proton's CLI prints nothing while it
//! uploads with `--json` and no terminal). The group's sockets come from /proc, their
//! `tcp_info` from a sock_diag netlink dump, so it needs no privileges beyond reading the
//! child's /proc entries (same user, or root).

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Sums what the group's connections had acknowledged; a closed connection keeps what it
/// had when last seen (sampled about once a second, so at most a second of it is missed).
#[derive(Debug, Default)]
pub struct Sent {
    pgid: i32,
    acked: HashMap<u32, u64>,
}

impl Sent {
    pub fn new(pgid: i32) -> Self {
        Sent { pgid, acked: HashMap::new() }
    }

    /// Bytes acknowledged so far; None when it can't be measured here.
    pub fn sample(&mut self) -> Option<u64> {
        let inodes = socket_inodes(&group_pids(self.pgid))?;
        let acked = tcp_acked().ok()?;
        for ino in inodes {
            if let Some(&b) = acked.get(&ino) {
                let seen = self.acked.entry(ino).or_insert(0);
                *seen = (*seen).max(b);
            }
        }
        Some(self.acked.values().sum())
    }
}

/// The processes of group `pgid` (field 5 of /proc/<pid>/stat, after the parenthesized name).
fn group_pids(pgid: i32) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| stat_pgrp(&s)) == Some(pgid))
        .collect()
}

fn stat_pgrp(stat: &str) -> Option<i32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(2)?.parse().ok()
}

/// The socket inodes the processes hold; None when none of their fd lists could be read.
fn socket_inodes(pids: &[i32]) -> Option<HashSet<u32>> {
    let mut out = HashSet::new();
    let mut readable = pids.is_empty();
    for pid in pids {
        let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        readable = true;
        for e in dir.flatten() {
            if let Some(ino) = std::fs::read_link(e.path()).ok().and_then(|l| socket_inode(&l.to_string_lossy())) {
                out.insert(ino);
            }
        }
    }
    readable.then_some(out)
}

/// `socket:[12345]` → 12345 (sock_diag reports inodes as 32 bits).
fn socket_inode(link: &str) -> Option<u32> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok().map(|i| i as u32)
}

const SOCK_DIAG_BY_FAMILY: u16 = 20;
const INET_DIAG_INFO: u16 = 2;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_HDR: usize = 16;
/// struct inet_diag_msg; idiag_inode is its last field
const DIAG_MSG: usize = 72;
const DIAG_INODE: usize = 68;
/// tcpi_bytes_acked in struct tcp_info (Linux 4.1+)
const BYTES_ACKED: usize = 120;

/// Every TCP socket's acknowledged bytes, by inode (IPv4 and IPv6).
fn tcp_acked() -> io::Result<HashMap<u32, u64>> {
    // SAFETY: plain socket(2); the fd is owned right away
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor nobody else owns
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut out = HashMap::new();
    for family in [libc::AF_INET, libc::AF_INET6] {
        dump(&fd, family as u8, &mut out)?;
    }
    Ok(out)
}

fn dump(fd: &OwnedFd, family: u8, out: &mut HashMap<u32, u64>) -> io::Result<()> {
    // nlmsghdr, then inet_diag_req_v2: family, protocol, extensions, pad, states, sockid (48)
    let mut req = [0u8; NLMSG_HDR + 56];
    let len = req.len() as u32;
    req[0..4].copy_from_slice(&len.to_ne_bytes());
    req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req[6..8].copy_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    req[16] = family;
    req[17] = libc::IPPROTO_TCP as u8;
    req[18] = 1 << (INET_DIAG_INFO - 1);
    req[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());
    // SAFETY: the buffer outlives the call; the kernel only reads it
    if unsafe { libc::send(fd.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: the kernel writes at most buf.len() bytes into buf
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 || parse_messages(&buf[..n as usize], out)? {
            return Ok(());
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// One recv's worth of netlink messages into `out`; true at the end of the dump.
fn parse_messages(mut b: &[u8], out: &mut HashMap<u32, u64>) -> io::Result<bool> {
    while b.len() >= NLMSG_HDR {
        let len = u32_at(b, 0).unwrap_or(0) as usize;
        if len < NLMSG_HDR || len > b.len() {
            break;
        }
        match u16_at(b, 4) {
            Some(NLMSG_DONE) => return Ok(true),
            Some(NLMSG_ERROR) => {
                let errno = u32_at(b, NLMSG_HDR).map_or(0, |e| -(e as i32));
                return Err(io::Error::from_raw_os_error(errno));
            }
            _ => {
                if let Some((ino, acked)) = parse_diag(&b[NLMSG_HDR..len]) {
                    out.insert(ino, acked);
                }
            }
        }
        b = &b[(len + 3) & !3..];
    }
    Ok(false)
}

/// An inet_diag_msg and its attributes → (inode, bytes acknowledged).
fn parse_diag(m: &[u8]) -> Option<(u32, u64)> {
    let ino = u32_at(m, DIAG_INODE)?;
    let mut a = m.get(DIAG_MSG..)?;
    while a.len() >= 4 {
        let len = u16_at(a, 0)? as usize;
        if len < 4 || len > a.len() {
            break;
        }
        if u16_at(a, 2)? == INET_DIAG_INFO {
            return Some((ino, u64_at(&a[4..len], BYTES_ACKED)?));
        }
        a = a.get((len + 3) & !3..)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn stat_and_fd_links() {
        assert_eq!(stat_pgrp("4242 (node (worker) x) S 1 4240 4240 0"), Some(4240));
        assert_eq!(socket_inode("socket:[123456]"), Some(123456));
        assert_eq!(socket_inode("/home/p3ter/x.zip"), None);
    }

    #[test]
    fn counts_what_a_connection_of_its_own_group_sent() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (mut s, _) = l.accept().unwrap();
        c.write_all(&[7u8; 300_000]).unwrap();
        let mut got = vec![0u8; 300_000];
        s.read_exact(&mut got).unwrap();
        // This test process's own group: the client socket's bytes are acknowledged
        let mut sent = Sent::new(nix::unistd::getpgrp().as_raw());
        let Some(n) = sent.sample() else { return }; // no sock_diag in this sandbox
        assert!(n >= 300_000, "{n}");
        drop((c, s));
        assert!(sent.sample().unwrap() >= 300_000, "closed connections keep their count");
    }
}
