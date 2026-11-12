//! Loopback is shared by every account on the machine, and the history-preserving
//! Codex connection cannot send a gateway token. A loopback gateway therefore looks
//! up which account owns the connecting socket and drops connections from other
//! accounts, so they cannot use this user's providers and logins.
//!
//! The lookup is best effort: when it cannot tell (an unsupported platform, a tool
//! that fails or times out, a socket already closed), the connection is accepted, so
//! a lookup problem never locks the user out of their own gateway. Only a socket
//! positively owned by another account is refused. Set `CODETAS_PEER_CHECK=off` to
//! turn the check off.

use super::*;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerOwner {
    SameUser,
    OtherUser { pid: Option<u32> },
    Unknown,
}

pub(crate) fn peer_check_enabled() -> bool {
    // Crate tests open many loopback connections; the parsers are tested directly.
    !cfg!(test)
        && std::env::var("CODETAS_PEER_CHECK")
            .map(|value| !matches!(value.trim(), "off" | "0" | "false"))
            .unwrap_or(true)
}

/// Accepts connections from `inner` and drops those owned by another OS account.
pub(crate) struct PeerCheckedListener {
    inner: TcpListener,
    local: SocketAddr,
}

impl PeerCheckedListener {
    pub(crate) fn new(inner: TcpListener, local: SocketAddr) -> Self {
        Self { inner, local }
    }
}

impl axum::serve::Listener for PeerCheckedListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, peer) = axum::serve::Listener::accept(&mut self.inner).await;
            match peer_owner(self.local, peer).await {
                PeerOwner::OtherUser { pid } => {
                    crate::debug::log_always(&format!(
                        "refused a loopback connection from another OS account (pid {})",
                        pid.map_or_else(|| "unknown".to_string(), |pid| pid.to_string())
                    ));
                    drop(stream);
                }
                PeerOwner::SameUser => return (stream, peer),
                PeerOwner::Unknown => {
                    static REPORTED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if peer.ip().is_loopback() && !REPORTED.swap(true, Ordering::Relaxed) {
                        crate::debug::log(
                            "could not tell which OS account owns a loopback connection; accepting it",
                        );
                    }
                    return (stream, peer);
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Which account owns the client end of a connection from `peer` to `local`.
pub(crate) async fn peer_owner(local: SocketAddr, peer: SocketAddr) -> PeerOwner {
    if !peer.ip().is_loopback() {
        return PeerOwner::Unknown;
    }
    lookup_peer_owner(local, peer).await
}

#[cfg(target_os = "linux")]
async fn lookup_peer_owner(local: SocketAddr, peer: SocketAddr) -> PeerOwner {
    use std::os::unix::fs::MetadataExt;
    let Ok(own_uid) = fs::metadata("/proc/self").map(|metadata| metadata.uid()) else {
        return PeerOwner::Unknown;
    };
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else {
            continue;
        };
        if let Some(uid) = proc_net_tcp_owner(&text, local.port(), peer.port()) {
            return if uid == own_uid {
                PeerOwner::SameUser
            } else {
                PeerOwner::OtherUser { pid: None }
            };
        }
    }
    PeerOwner::Unknown
}

#[cfg(target_os = "macos")]
async fn lookup_peer_owner(local: SocketAddr, peer: SocketAddr) -> PeerOwner {
    let Some(table) = command_output("/usr/sbin/netstat", &["-anv", "-p", "tcp"]).await else {
        return PeerOwner::Unknown;
    };
    let Some(peer_pid) = netstat_socket_pid(&table, local, peer) else {
        return PeerOwner::Unknown;
    };
    let own_pid = std::process::id();
    if peer_pid == own_pid {
        return PeerOwner::SameUser;
    }
    let pids = format!("{peer_pid},{own_pid}");
    let Some(listing) = command_output("/bin/ps", &["-o", "pid=,uid=", "-p", &pids]).await else {
        return PeerOwner::Unknown;
    };
    match ps_uids(&listing, peer_pid, own_pid) {
        Some((peer_uid, own_uid)) if peer_uid == own_uid => PeerOwner::SameUser,
        Some(_) => PeerOwner::OtherUser {
            pid: Some(peer_pid),
        },
        None => PeerOwner::Unknown,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn lookup_peer_owner(_local: SocketAddr, _peer: SocketAddr) -> PeerOwner {
    PeerOwner::Unknown
}

#[cfg(target_os = "macos")]
async fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(LOOKUP_TIMEOUT, output)
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Linux `/proc/net/tcp{,6}`: the owner uid of the socket whose own port is the
/// peer's and whose remote port is the gateway's.
#[cfg(test)]
pub(crate) fn proc_net_tcp_owner(table: &str, local_port: u16, peer_port: u16) -> Option<u32> {
    let hex_port = |address: &str| {
        address
            .rsplit_once(':')
            .and_then(|(_, port)| u16::from_str_radix(port, 16).ok())
    };
    table.lines().skip(1).find_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let socket_port = hex_port(fields.get(1)?)?;
        let remote_port = hex_port(fields.get(2)?)?;
        if socket_port != peer_port || remote_port != local_port {
            return None;
        }
        fields.get(7)?.parse().ok()
    })
}

/// macOS `netstat -anv -p tcp`: the pid owning the socket whose own address is the
/// peer's and whose remote address is the gateway's. Addresses end in `.port`, and
/// the owner is printed as `name:pid`, where the name may contain spaces.
pub(crate) fn netstat_socket_pid(table: &str, local: SocketAddr, peer: SocketAddr) -> Option<u32> {
    let socket = format!("{}.{}", peer.ip(), peer.port());
    let remote = format!("{}.{}", local.ip(), local.port());
    table.lines().find_map(|line| {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        if tokens.len() < 7
            || !tokens[0].starts_with("tcp")
            || tokens[3] != socket
            || tokens[4] != remote
        {
            return None;
        }
        tokens[6..].iter().find_map(|token| {
            let (_, pid) = token.rsplit_once(':')?;
            if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            pid.parse().ok()
        })
    })
}

/// `ps -o pid=,uid=` output: the uids of the peer and of this process.
pub(crate) fn ps_uids(listing: &str, peer_pid: u32, own_pid: u32) -> Option<(u32, u32)> {
    let (mut peer_uid, mut own_uid) = (None, None);
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(uid)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(pid), Ok(uid)) = (pid.parse::<u32>(), uid.parse::<u32>()) else {
            continue;
        };
        if pid == peer_pid {
            peer_uid = Some(uid);
        } else if pid == own_pid {
            own_uid = Some(uid);
        }
    }
    Some((peer_uid?, own_uid?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    const NETSTAT: &str = "\
Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)          rxbytes      txbytes  rhiwat  shiwat          process:pid    state  options           gencnt    flags   flags1 usecnt rtncnt fltrs
tcp4       0      0  127.0.0.1.42421        127.0.0.1.52000        ESTABLISHED         1712          185  407296  146808   codetas-gateway:900  00102 00000104 000000000002dcf0 00000081 01000800      2      0 000000
tcp4       0      0  127.0.0.1.52000        127.0.0.1.42421        ESTABLISHED          514          640  407680  146808 Google Chrome He:4321  00102 00000008 000000000002dcef 00000080 04000900      2      0 000000
tcp6       0      0  ::1.52001              ::1.42421              ESTABLISHED          514          640  407680  146808            codex:777  00102 00000008 000000000002dcef 00000080 04000900      2      0 000000
tcp4       0      0  127.0.0.1.42421        *.*                    LISTEN                 0            0  131072  131072   codetas-gateway:900  00100 00000006 00000000000314b3 00000001 00000800      1      0 000000
";

    #[test]
    fn netstat_finds_the_client_end_not_the_gateway_end() {
        let local = address("127.0.0.1:42421");
        assert_eq!(
            netstat_socket_pid(NETSTAT, local, address("127.0.0.1:52000")),
            Some(4321)
        );
        assert_eq!(
            netstat_socket_pid(NETSTAT, address("[::1]:42421"), address("[::1]:52001")),
            Some(777)
        );
        assert_eq!(
            netstat_socket_pid(NETSTAT, local, address("127.0.0.1:52999")),
            None
        );
    }

    #[test]
    fn netstat_rows_without_a_named_pid_are_not_guessed() {
        let old_format =
            "tcp4 0 0 127.0.0.1.52000 127.0.0.1.42421 ESTABLISHED 131072 131072 4321 0\n";
        assert_eq!(
            netstat_socket_pid(
                old_format,
                address("127.0.0.1:42421"),
                address("127.0.0.1:52000")
            ),
            None
        );
    }

    #[test]
    fn proc_net_tcp_reads_the_owner_of_the_client_socket() {
        // Ports are hex: 0xA5B5 = 42421 (gateway), 0xCB20 = 52000 (client).
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:A5B5 0100007F:CB20 01 00000000:00000000 00:00000000 00000000  1000        0 11 1 0 20 4 30 10 -1
   1: 0100007F:CB20 0100007F:A5B5 01 00000000:00000000 00:00000000 00000000  1001        0 12 1 0 20 4 30 10 -1
";
        assert_eq!(proc_net_tcp_owner(table, 42421, 52000), Some(1001));
        assert_eq!(proc_net_tcp_owner(table, 42421, 52999), None);
    }

    #[test]
    fn ps_listing_yields_both_uids() {
        let listing = " 4321   502\n  900   501\n";
        assert_eq!(ps_uids(listing, 4321, 900), Some((502, 501)));
        assert_eq!(ps_uids(" 900 501\n", 4321, 900), None);
    }

    #[tokio::test]
    async fn non_loopback_peers_are_left_to_authentication() {
        assert_eq!(
            peer_owner(address("0.0.0.0:42421"), address("192.168.1.20:52000")).await,
            PeerOwner::Unknown
        );
    }
}
