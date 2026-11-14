//! Host networking for microVMs: one TAP device and one /30 per VM, NAT
//! through the host's default interface, and a firewall table that keeps
//! VMs away from IMDS, from each other, and from the host itself.

use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use super::sys;

/// The address block VM subnets are carved from.
const BLOCK_BASE: u32 = u32::from_be_bytes([10, 200, 0, 0]);
const BLOCK_PREFIX: u32 = 16;
/// A /30 per VM: 16384 of them in a /16.
pub const MAX_SLOTS: u32 = 1 << (30 - BLOCK_PREFIX);
/// The VPC DNS resolver address every EC2 instance can reach.
pub const GUEST_DNS: &str = "169.254.169.253";
const TAP_PREFIX: &str = "mjt";
const TABLE: &str = "mj_microvm";

/// Addresses for one VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub tap: String,
    pub host: Ipv4Addr,
    pub guest: Ipv4Addr,
    pub mac: String,
}

impl Link {
    pub fn for_slot(slot: u32) -> Result<Self> {
        ensure!(slot < MAX_SLOTS, "network slot {slot} is out of range");
        let network = BLOCK_BASE + slot * 4;
        Ok(Self {
            tap: format!("{TAP_PREFIX}{slot}"),
            host: Ipv4Addr::from(network + 1),
            guest: Ipv4Addr::from(network + 2),
            mac: format!("06:00:0a:c8:{:02x}:{:02x}", (slot >> 8) & 0xff, slot & 0xff),
        })
    }

    /// `address/30` as the guest configures it.
    pub fn guest_cidr(&self) -> String {
        format!("{}/30", self.guest)
    }
}

/// A TAP device name this engine could have created.
pub fn is_engine_tap(name: &str) -> bool {
    name.strip_prefix(TAP_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Create the TAP device for a VM, addressed on the host side and owned by
/// `owner`, the user the jailed Firecracker runs as: an unprivileged process
/// may attach to a persistent TAP only if it owns it. A stale device from an
/// earlier run is replaced, since its owner may differ.
pub fn tap_up(link: &Link, owner: (u32, u32)) -> Result<()> {
    let ip = sys::find_tool("ip")?;
    tap_down(&link.tap)?;
    let (uid, gid) = (owner.0.to_string(), owner.1.to_string());
    sys::run_checked(
        &ip,
        &[
            "tuntap",
            "add",
            "dev",
            link.tap.as_str(),
            "mode",
            "tap",
            "user",
            uid.as_str(),
            "group",
            gid.as_str(),
        ],
    )?;
    let address = format!("{}/30", link.host);
    sys::run_checked(
        &ip,
        &[
            "addr",
            "replace",
            address.as_str(),
            "dev",
            link.tap.as_str(),
        ],
    )?;
    sys::run_checked(&ip, &["link", "set", link.tap.as_str(), "up"])?;
    Ok(())
}

/// Remove a TAP device. An absent device is the goal state, not an error.
pub fn tap_down(tap: &str) -> Result<()> {
    ensure!(
        is_engine_tap(tap),
        "{tap:?} is not a microvm TAP device name"
    );
    let ip = sys::find_tool("ip")?;
    let output = sys::run_unchecked(&ip, &["link", "del", tap])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("Cannot find device") {
            bail!("remove TAP device {tap}: {}", sys::tail(&stderr, 5));
        }
    }
    Ok(())
}

/// Whether a TAP device exists.
pub fn tap_exists(tap: &str) -> bool {
    Path::new("/sys/class/net").join(tap).exists()
}

/// The nftables ruleset, applied as one atomic replacement.
fn ruleset() -> String {
    format!(
        r#"add table ip6 {TABLE}
delete table ip6 {TABLE}
table ip6 {TABLE} {{
    chain input {{
        type filter hook input priority filter; policy accept;
        iifname "{TAP_PREFIX}*" drop
    }}
    chain forward {{
        type filter hook forward priority filter; policy accept;
        iifname "{TAP_PREFIX}*" drop
    }}
}}
add table ip {TABLE}
delete table ip {TABLE}
table ip {TABLE} {{
    chain input {{
        type filter hook input priority filter; policy accept;
        iifname "{TAP_PREFIX}*" ct state established,related accept
        iifname "{TAP_PREFIX}*" icmp type echo-request accept
        iifname "{TAP_PREFIX}*" drop
    }}
    chain forward {{
        type filter hook forward priority filter; policy accept;
        iifname "{TAP_PREFIX}*" ip daddr 169.254.169.254 reject
        iifname "{TAP_PREFIX}*" oifname "{TAP_PREFIX}*" drop
    }}
    chain postrouting {{
        type nat hook postrouting priority srcnat; policy accept;
        iifname "{TAP_PREFIX}*" oifname != "{TAP_PREFIX}*" masquerade
    }}
}}
"#
    )
}

/// Enable forwarding and install the firewall table. Idempotent; called
/// before every launch so a reboot needs no separate setup.
pub fn apply_host_policy() -> Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
        .context("enable IPv4 forwarding (/proc/sys/net/ipv4/ip_forward)")?;
    let nft = sys::find_tool("nft")?;
    let mut command = std::process::Command::new(&nft);
    command.args(["-f", "-"]);
    let output = mj_core::subprocess::run_with_input(&mut command, ruleset().as_bytes())
        .context("run nft")?;
    if !output.status.success() {
        bail!(
            "install the microvm firewall table with nft: {}",
            sys::tail(&String::from_utf8_lossy(&output.stderr), 10)
        );
    }
    Ok(())
}

/// Refuse to launch when the VM address block overlaps a route the host
/// already has: the VM subnets would shadow that network.
pub fn check_no_route_overlap() -> Result<()> {
    let routes = std::fs::read_to_string("/proc/net/route").context("read /proc/net/route")?;
    if let Some(conflict) = find_route_overlap(&routes) {
        bail!(
            "the host already routes {conflict}, which overlaps the microvm address block 10.200.0.0/16"
        );
    }
    Ok(())
}

/// The first route in `/proc/net/route` text that overlaps the VM address block.
fn find_route_overlap(routes: &str) -> Option<String> {
    for line in routes.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[0].starts_with(TAP_PREFIX) {
            continue;
        }
        let (Ok(destination), Ok(mask)) = (
            u32::from_str_radix(fields[1], 16),
            u32::from_str_radix(fields[7], 16),
        ) else {
            continue;
        };
        // The kernel prints the in-memory (network order) value as a host-order hex number.
        let destination = u32::from_be_bytes(destination.to_le_bytes());
        let mask = u32::from_be_bytes(mask.to_le_bytes());
        if mask == 0 {
            continue; // the default route reaches everything; it is not a conflict
        }
        let shared = mask & (u32::MAX << (32 - BLOCK_PREFIX));
        if destination & shared == BLOCK_BASE & shared {
            return Some(format!(
                "{}/{} on {}",
                Ipv4Addr::from(destination),
                mask.count_ones(),
                fields[0]
            ));
        }
    }
    None
}
