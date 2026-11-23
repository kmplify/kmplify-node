//! Who may reach the router, and where it may listen.
//!
//! Pure rules, no I/O, compiled into every build: `kmplify-node set` checks
//! router settings with them even in a build without the router, so a value
//! the router would refuse is refused when it is typed, not at the next
//! start.
//!
//! The model (owner decision 2026-11-23):
//!
//! * **Off unless chosen.** [`Mode::Off`] is the default: no listener, no
//!   multicast, nothing.
//! * **Loopback by default.** [`Mode::Local`] binds 127.0.0.1 only. Apps on
//!   this computer can read the node's report; nothing on the network can
//!   even connect.
//! * **LAN only on explicit opt-in.** [`Mode::Lan`] binds every interface,
//!   but a connection from an address outside the [`Allowlist`] is dropped
//!   before a byte is read, and the allowlist may only name private,
//!   link-local or CGNAT ranges: a LAN router is not an internet service.
//! * **Every request is authenticated**, loopback included, with the
//!   cluster key (see `auth.rs`). The allowlist decides who may knock; the
//!   key decides who gets in.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The router's runtime mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Off,
    Local,
    Lan,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "no" | "0" | "" => Ok(Mode::Off),
            "local" | "loopback" | "on" | "true" | "yes" | "1" => Ok(Mode::Local),
            "lan" | "network" => Ok(Mode::Lan),
            other => Err(format!("router must be off, local or lan, not {other:?}")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Local => "local",
            Mode::Lan => "lan",
        }
    }
}

/// One allowlist entry: an address with a prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub net: IpAddr,
    pub prefix: u8,
}

/// An IPv4 address that arrived as IPv4-mapped IPv6 (`::ffff:a.b.c.d`, what
/// a dual-stack socket reports) is that IPv4 address for every rule here.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

fn mask_v4(ip: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(ip);
    if prefix == 0 {
        0
    } else {
        bits & (u32::MAX << (32 - u32::from(prefix)))
    }
}

fn mask_v6(ip: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(ip);
    if prefix == 0 {
        0
    } else {
        bits & (u128::MAX << (128 - u32::from(prefix)))
    }
}

impl Cidr {
    /// `192.168.1.0/24`, `10.0.0.7` (one host), `fd12::/64`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let net: IpAddr = addr
            .parse()
            .map_err(|_| format!("{s:?} is not an IP address or CIDR range"))?;
        let net = canonical(net);
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("{s:?}: the prefix must be 0 to {max}"))?,
        };
        let cidr = Cidr { net, prefix };
        if !cidr.is_lan() {
            return Err(format!(
                "{s:?} is not a local network range. The router's allowlist may name \
                 private (10/8, 172.16/12, 192.168/16, fc00::/7), link-local \
                 (169.254/16, fe80::/10) or CGNAT/VPN (100.64/10) addresses only"
            ));
        }
        Ok(cidr)
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, canonical(ip)) {
            (IpAddr::V4(n), IpAddr::V4(a)) => mask_v4(n, self.prefix) == mask_v4(a, self.prefix),
            (IpAddr::V6(n), IpAddr::V6(a)) => mask_v6(n, self.prefix) == mask_v6(a, self.prefix),
            _ => false,
        }
    }

    /// The whole range lies inside one local-network block. A range wider
    /// than its block (`10.0.0.0/7`, `0.0.0.0/0`) does not.
    pub fn is_lan(&self) -> bool {
        let inside =
            |block: Cidr| -> bool { self.prefix >= block.prefix && block.contains(self.net) };
        lan_blocks().into_iter().any(inside)
    }
}

/// Where a LAN may live: RFC 1918, link-local, CGNAT (Tailscale and other
/// mesh VPNs), and IPv6 unique-local and link-local.
fn lan_blocks() -> [Cidr; 7] {
    let v4 = |a, b, c, d, prefix| Cidr {
        net: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        prefix,
    };
    let v6 = |seg: u16, prefix| Cidr {
        net: IpAddr::V6(Ipv6Addr::new(seg, 0, 0, 0, 0, 0, 0, 0)),
        prefix,
    };
    [
        v4(10, 0, 0, 0, 8),
        v4(172, 16, 0, 0, 12),
        v4(192, 168, 0, 0, 16),
        v4(169, 254, 0, 0, 16),
        v4(100, 64, 0, 0, 10),
        v6(0xfc00, 7),
        v6(0xfe80, 10),
    ]
}

/// The addresses that may connect in LAN mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: Vec<Cidr>,
}

impl Allowlist {
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Self, String> {
        let entries = entries
            .iter()
            .map(|e| Cidr::parse(e.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        self.entries.iter().any(|c| c.contains(ip))
    }

    pub fn entries(&self) -> &[Cidr] {
        &self.entries
    }
}

/// A node label: what peers see. Letters, digits, `-` and `_`, 1 to 40
/// characters, so it is also a valid mDNS instance name and never a place to
/// smuggle anything into a log line.
pub fn check_name(s: &str) -> Result<String, String> {
    let s = s.trim();
    let ok = !s.is_empty()
        && s.len() <= 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!(
            "router-name must be 1 to 40 letters, digits, - or _, not {s:?}"
        ))
    }
}

/// The address the node-info surface binds in `mode`.
pub fn bind_addr(mode: Mode, port: u16) -> Option<SocketAddr> {
    match mode {
        Mode::Off => None,
        Mode::Local => Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)),
        Mode::Lan => Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)),
    }
}

/// Belt and braces after binding: the socket that is actually listening
/// must be what the mode allows.
pub fn check_bound(mode: Mode, addr: &SocketAddr) -> Result<(), String> {
    match mode {
        Mode::Off => Err("the router is off and binds nothing".into()),
        Mode::Local if !addr.ip().is_loopback() => Err(format!(
            "local mode binds loopback only, never {}",
            addr.ip()
        )),
        _ => Ok(()),
    }
}

/// Why a connection is turned away before its request is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not loopback, and the router is in local mode.
    NotLocal,
    /// LAN mode, but the address is not on the allowlist.
    NotAllowed,
}

/// May a connection from `peer` be read at all? Loopback always may (it
/// still has to authenticate); anything else only in LAN mode and only from
/// the allowlist.
pub fn admit(mode: Mode, allow: &Allowlist, peer: IpAddr) -> Result<(), Refusal> {
    let peer = canonical(peer);
    if peer.is_loopback() {
        return Ok(());
    }
    match mode {
        Mode::Lan if allow.allows(peer) => Ok(()),
        Mode::Lan => Err(Refusal::NotAllowed),
        _ => Err(Refusal::NotLocal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn modes_parse_and_default_to_off() {
        assert_eq!(Mode::default(), Mode::Off);
        assert_eq!(Mode::parse("LAN"), Ok(Mode::Lan));
        assert_eq!(Mode::parse("local"), Ok(Mode::Local));
        assert_eq!(Mode::parse("on"), Ok(Mode::Local), "on means the safe mode");
        assert_eq!(Mode::parse("off"), Ok(Mode::Off));
        assert!(Mode::parse("public").is_err());
        for m in [Mode::Off, Mode::Local, Mode::Lan] {
            assert_eq!(Mode::parse(m.as_str()), Ok(m));
        }
    }

    #[test]
    fn local_mode_binds_loopback_and_off_binds_nothing() {
        assert_eq!(bind_addr(Mode::Off, 14418), None);
        let local = bind_addr(Mode::Local, 14418).unwrap();
        assert!(local.ip().is_loopback());
        assert!(check_bound(Mode::Local, &local).is_ok());
        let any = bind_addr(Mode::Lan, 14418).unwrap();
        assert!(any.ip().is_unspecified());
        assert!(check_bound(Mode::Local, &any).is_err());
        assert!(check_bound(Mode::Lan, &any).is_ok());
    }

    #[test]
    fn the_allowlist_takes_local_ranges_only() {
        for good in [
            "192.168.1.0/24",
            "10.0.0.7",
            "172.16.0.0/12",
            "100.64.0.0/10",
            "169.254.0.0/16",
            "fd12:3456::/64",
            "fe80::1",
        ] {
            assert!(Cidr::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "0.0.0.0/0",
            "::/0",
            "10.0.0.0/7",
            "8.8.8.8",
            "203.0.113.0/24",
            "2001:db8::/32",
            "192.168.1.0/33",
            "192.168.1",
            "",
            "localhost",
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
        assert!(Allowlist::parse(&["192.168.1.0/24", "8.8.8.8"]).is_err());
    }

    #[test]
    fn membership_follows_the_prefix() {
        let a = Allowlist::parse(&["192.168.1.0/24", "10.1.2.3"]).unwrap();
        assert!(a.allows(ip("192.168.1.200")));
        assert!(!a.allows(ip("192.168.2.1")));
        assert!(a.allows(ip("10.1.2.3")));
        assert!(!a.allows(ip("10.1.2.4")));
        // A dual-stack socket reports IPv4 peers as mapped IPv6.
        assert!(a.allows(ip("::ffff:192.168.1.9")));
        assert!(!a.allows(ip("fd00::1")));
        let v6 = Allowlist::parse(&["fd00:1::/32"]).unwrap();
        assert!(v6.allows(ip("fd00:1:ffff::1")));
        assert!(!v6.allows(ip("fd00:2::1")));
        assert!(Allowlist::default().is_empty());
        assert!(!Allowlist::default().allows(ip("192.168.1.1")));
    }

    #[test]
    fn admission_is_loopback_by_default_and_allowlist_on_the_lan() {
        let allow = Allowlist::parse(&["192.168.1.0/24"]).unwrap();
        // Loopback may always knock; it still has to authenticate.
        for mode in [Mode::Local, Mode::Lan] {
            assert_eq!(admit(mode, &allow, ip("127.0.0.1")), Ok(()));
            assert_eq!(admit(mode, &allow, ip("::1")), Ok(()));
        }
        // Local mode: nothing from the network, allowlisted or not.
        assert_eq!(
            admit(Mode::Local, &allow, ip("192.168.1.5")),
            Err(Refusal::NotLocal)
        );
        // LAN mode: the allowlist, and nothing else.
        assert_eq!(admit(Mode::Lan, &allow, ip("192.168.1.5")), Ok(()));
        assert_eq!(
            admit(Mode::Lan, &allow, ip("192.168.7.5")),
            Err(Refusal::NotAllowed)
        );
        assert_eq!(
            admit(Mode::Lan, &Allowlist::default(), ip("192.168.1.5")),
            Err(Refusal::NotAllowed),
            "an empty allowlist lets nobody in"
        );
        assert_eq!(admit(Mode::Lan, &allow, ip("::ffff:192.168.1.5")), Ok(()));
    }

    #[test]
    fn names_are_plain_labels() {
        assert_eq!(check_name(" studio-2 ").unwrap(), "studio-2");
        for bad in ["", "a b", "x\ny", "ä", &"n".repeat(41), "../x"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }
}
