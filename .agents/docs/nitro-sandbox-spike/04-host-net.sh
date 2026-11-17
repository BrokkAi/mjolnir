#!/bin/bash
# Runs ON the parent as root-capable user. Global network policy for all sandbox TAPs (idempotent).
# Sandboxes are on tap<N> with 10.77.<N>.0/30 (host .1, guest .2).
set -euo pipefail
IFACE=$(ip route show default | awk '{print $5; exit}')
sudo sysctl -qw net.ipv4.ip_forward=1
sudo nft delete table ip sandbox 2>/dev/null || true
sudo nft -f - <<NFT
table ip sandbox {
    chain input {            # what a sandbox may reach on the parent itself
        type filter hook input priority filter; policy accept;
        iifname "tap*" ct state established,related accept
        iifname "tap*" icmp type echo-request accept
        iifname "tap*" drop
    }
    chain forward {
        type filter hook forward priority filter; policy accept;
        iifname "tap*" ip daddr 169.254.169.254 reject          # IMDS (IPv4); IMDS IPv6 not routed
        iifname "tap*" oifname "tap*" drop                      # no sandbox-to-sandbox traffic
    }
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        iifname "tap*" oifname "$IFACE" masquerade
    }
}
NFT
sudo nft list table ip sandbox
