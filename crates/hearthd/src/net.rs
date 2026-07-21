//! Managed addresses, static leases, and managed publish. Everything in this
//! module is a pure function of its inputs so it can be
//! unit-tested without dnsmasq, nftables, or a booted VM. The daemon in `lib.rs`
//! does the IO (read the lease file, write drop-ins, shell out to `nft`) and
//! feeds the results through these functions.

use crate::registry::Publish;
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

/// One parsed dnsmasq lease. The lease file has whitespace-separated columns
/// `epoch mac ip hostname clientid`; we only need mac + ip + hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub mac: String,
    pub ip: String,
    pub hostname: String,
}

/// Parse the dnsmasq lease file. Malformed/short lines are skipped rather than
/// failing the whole parse — a truncated lease file must not blind `status`.
pub fn parse_leases(text: &str) -> Vec<Lease> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let _epoch = cols.next()?;
            let mac = cols.next()?;
            let ip = cols.next()?;
            let hostname = cols.next().unwrap_or("*");
            Some(Lease {
                mac: mac.to_string(),
                ip: ip.to_string(),
                hostname: hostname.to_string(),
            })
        })
        .collect()
}

/// Find the lease matching a service MAC. dnsmasq writes MACs lowercase; a
/// service MAC is also lowercase, but match case-insensitively to be safe.
pub fn lease_for_mac<'a>(leases: &'a [Lease], mac: &str) -> Option<&'a Lease> {
    leases.iter().find(|l| l.mac.eq_ignore_ascii_case(mac))
}

/// The part of the lease file that affects routing. Expiry and hostname changes
/// do not require an nftables rewrite; MAC additions, removals, and IP moves do.
pub fn lease_addresses(leases: &[Lease]) -> BTreeMap<String, String> {
    leases
        .iter()
        .map(|lease| (lease.mac.to_ascii_lowercase(), lease.ip.clone()))
        .collect()
}

/// Resolve the address to report/route for a service. An observed lease is
/// ground truth and wins; the static reservation is the expected address before
/// a lease appears. Returns `(ip, source)` where `source` is `"lease"` or
/// `"static"`, or `None` when neither is known.
pub fn resolve_address<'a>(
    lease_ip: Option<&'a str>,
    static_ip: Option<&'a str>,
) -> Option<(&'a str, &'static str)> {
    if let Some(ip) = lease_ip {
        Some((ip, "lease"))
    } else {
        static_ip.map(|ip| (ip, "static"))
    }
}

/// Allocate the lowest free IPv4 in `[start, start + count)` not already in
/// `used`. Returns `None` when the whole static slice is taken.
pub fn allocate_ip(start: Ipv4Addr, count: u32, used: &BTreeSet<Ipv4Addr>) -> Option<Ipv4Addr> {
    let base = u32::from(start);
    (0..count)
        .map(|offset| Ipv4Addr::from(base.wrapping_add(offset)))
        .find(|ip| !used.contains(ip))
}

/// The single `dhcp-host=` line a static-lease drop-in contains. dnsmasq pins
/// `mac` to `ip` and publishes `hostname` through dnsmasq DNS.
pub fn dhcp_host_line(mac: &str, ip: &str, hostname: &str) -> String {
    format!("dhcp-host={mac},{ip},{hostname}\n")
}

/// A service's publish surface with its resolved address, the input to
/// [`nat_ruleset`]. Owned so it can be built from a registry + lease read.
#[derive(Debug, Clone)]
pub struct PublishTarget {
    pub service: String,
    /// The guest's resolved address, or `None` if unknown (no lease, no static
    /// reservation) — such a service's rules are skipped.
    pub address: Option<String>,
    pub publishes: Vec<Publish>,
}

/// The generated nftables ruleset plus the services skipped for lack of an
/// address (so the caller can warn about each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatRuleset {
    pub text: String,
    pub skipped: Vec<String>,
}

/// Generate the full `table ip hearth_nat` ruleset from every service's
/// publishes. Pure function of (services x addresses x publishes); the daemon
/// hands the text to `nft -f -`.
///
/// The `add table` + `flush table` prologue makes a single `nft -f` invocation
/// an idempotent full rewrite: the table is created if missing, emptied, then
/// repopulated — the same "own it and rewrite it wholesale" pattern as tap
/// setup, and it never names another table. No publishes anywhere yields just
/// the (valid, empty) prologue. A service with publishes but no known address
/// is skipped and reported in `skipped`.
pub fn nat_ruleset(targets: &[PublishTarget]) -> NatRuleset {
    let mut rules = String::new();
    let mut skipped = Vec::new();
    let mut chain_emitted = false;
    for target in targets {
        if target.publishes.is_empty() {
            continue;
        }
        let Some(address) = &target.address else {
            skipped.push(target.service.clone());
            continue;
        };
        for publish in &target.publishes {
            if !chain_emitted {
                // dstnat priority (-100) runs the DNAT before routing so a LAN/
                // tailnet packet to a published host port is redirected into the
                // guest. Reply traffic is un-DNAT'd by conntrack; the host is the
                // guest's gateway so no SNAT is needed here.
                rules.push_str(
                    "add chain ip hearth_nat prerouting { type nat hook prerouting priority -100 ; }\n",
                );
                chain_emitted = true;
            }
            let daddr = match &publish.bind {
                Some(bind) => format!("ip daddr {bind} "),
                None => String::new(),
            };
            rules.push_str(&format!(
                "add rule ip hearth_nat prerouting {daddr}{proto} dport {host_port} dnat to {address}:{guest_port}\n",
                proto = publish.protocol,
                host_port = publish.host_port,
                guest_port = publish.guest_port,
            ));
        }
    }
    let mut text = String::from("add table ip hearth_nat\nflush table ip hearth_nat\n");
    text.push_str(&rules);
    NatRuleset { text, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(host_port: u16, guest_port: u16, protocol: &str, bind: Option<&str>) -> Publish {
        Publish {
            name: String::new(),
            host_port,
            guest_port,
            protocol: protocol.to_string(),
            bind: bind.map(str::to_string),
        }
    }

    #[test]
    fn parse_leases_reads_epoch_mac_ip_hostname() {
        let text = "\
1720500000 52:54:00:8c:22:12 10.26.8.23 hermes 01:52:54:00:8c:22:12
1720500100 52:54:00:aa:bb:cc 10.26.8.24 * *
";
        let leases = parse_leases(text);
        assert_eq!(leases.len(), 2);
        assert_eq!(leases[0].mac, "52:54:00:8c:22:12");
        assert_eq!(leases[0].ip, "10.26.8.23");
        assert_eq!(leases[0].hostname, "hermes");
        assert_eq!(leases[1].hostname, "*");
    }

    #[test]
    fn parse_leases_skips_blank_and_short_lines() {
        let text = "\n   \ngarbage\n1720 52:54:00:00:00:01 10.26.8.9 host id\n";
        let leases = parse_leases(text);
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].ip, "10.26.8.9");
    }

    #[test]
    fn lease_for_mac_matches_case_insensitively() {
        let leases = parse_leases("1 52:54:00:8C:22:12 10.26.8.23 h id\n");
        assert_eq!(
            lease_for_mac(&leases, "52:54:00:8c:22:12").map(|l| l.ip.as_str()),
            Some("10.26.8.23")
        );
        assert!(lease_for_mac(&leases, "52:54:00:00:00:99").is_none());
    }

    #[test]
    fn lease_addresses_tracks_only_mac_to_ip_changes() {
        let first = parse_leases("1 52:54:00:8C:22:12 10.26.8.23 old id\n");
        let renewed = parse_leases("2 52:54:00:8c:22:12 10.26.8.23 new id\n");
        let moved = parse_leases("3 52:54:00:8c:22:12 10.26.8.24 new id\n");

        assert_eq!(lease_addresses(&first), lease_addresses(&renewed));
        assert_ne!(lease_addresses(&first), lease_addresses(&moved));
    }

    #[test]
    fn resolve_address_prefers_lease_then_static() {
        assert_eq!(
            resolve_address(Some("10.26.8.23"), Some("10.26.8.16")),
            Some(("10.26.8.23", "lease"))
        );
        assert_eq!(
            resolve_address(None, Some("10.26.8.16")),
            Some(("10.26.8.16", "static"))
        );
        assert_eq!(resolve_address(None, None), None);
    }

    #[test]
    fn allocate_ip_picks_lowest_free_and_skips_used() {
        let start: Ipv4Addr = "10.26.8.16".parse().unwrap();
        let mut used = BTreeSet::new();
        assert_eq!(
            allocate_ip(start, 64, &used),
            Some("10.26.8.16".parse().unwrap())
        );
        used.insert("10.26.8.16".parse().unwrap());
        used.insert("10.26.8.17".parse().unwrap());
        assert_eq!(
            allocate_ip(start, 64, &used),
            Some("10.26.8.18".parse().unwrap())
        );
    }

    #[test]
    fn allocate_ip_returns_none_when_slice_exhausted() {
        let start: Ipv4Addr = "10.26.8.16".parse().unwrap();
        let used: BTreeSet<Ipv4Addr> = (16..18)
            .map(|last| Ipv4Addr::new(10, 26, 8, last))
            .collect();
        assert_eq!(allocate_ip(start, 2, &used), None);
    }

    #[test]
    fn nat_ruleset_with_no_publishes_is_empty_but_valid() {
        let ruleset = nat_ruleset(&[]);
        assert_eq!(
            ruleset.text,
            "add table ip hearth_nat\nflush table ip hearth_nat\n"
        );
        assert!(ruleset.skipped.is_empty());
        // A service that exists but publishes nothing still contributes no rules.
        let ruleset = nat_ruleset(&[PublishTarget {
            service: "web".to_string(),
            address: Some("10.26.8.16".to_string()),
            publishes: vec![],
        }]);
        assert_eq!(
            ruleset.text,
            "add table ip hearth_nat\nflush table ip hearth_nat\n"
        );
    }

    #[test]
    fn nat_ruleset_emits_tcp_and_udp_dnat_rules() {
        let ruleset = nat_ruleset(&[PublishTarget {
            service: "dns".to_string(),
            address: Some("10.26.8.20".to_string()),
            publishes: vec![
                publish(9119, 9119, "tcp", None),
                publish(53, 53, "udp", None),
            ],
        }]);
        assert!(ruleset.text.contains(
            "add chain ip hearth_nat prerouting { type nat hook prerouting priority -100 ; }"
        ));
        assert!(ruleset
            .text
            .contains("add rule ip hearth_nat prerouting tcp dport 9119 dnat to 10.26.8.20:9119"));
        assert!(ruleset
            .text
            .contains("add rule ip hearth_nat prerouting udp dport 53 dnat to 10.26.8.20:53"));
        assert!(ruleset.skipped.is_empty());
    }

    #[test]
    fn nat_ruleset_restricts_to_bind_address() {
        let ruleset = nat_ruleset(&[PublishTarget {
            service: "web".to_string(),
            address: Some("10.26.8.16".to_string()),
            publishes: vec![publish(443, 8443, "tcp", Some("100.121.19.41"))],
        }]);
        assert!(ruleset.text.contains(
            "add rule ip hearth_nat prerouting ip daddr 100.121.19.41 tcp dport 443 dnat to 10.26.8.16:8443"
        ));
    }

    #[test]
    fn nat_ruleset_skips_and_reports_targets_without_an_address() {
        let ruleset = nat_ruleset(&[
            PublishTarget {
                service: "known".to_string(),
                address: Some("10.26.8.16".to_string()),
                publishes: vec![publish(80, 80, "tcp", None)],
            },
            PublishTarget {
                service: "unknown".to_string(),
                address: None,
                publishes: vec![publish(80, 80, "tcp", None)],
            },
        ]);
        assert!(ruleset.text.contains("dnat to 10.26.8.16:80"));
        assert!(!ruleset.text.contains("unknown"));
        assert_eq!(ruleset.skipped, vec!["unknown".to_string()]);
    }

    #[test]
    fn dhcp_host_line_pins_mac_to_ip() {
        assert_eq!(
            dhcp_host_line("52:54:00:8c:22:12", "10.26.8.16", "web"),
            "dhcp-host=52:54:00:8c:22:12,10.26.8.16,web\n"
        );
    }
}
