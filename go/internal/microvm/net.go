//go:build linux

// Port of mj-microvm/src/net.rs.
//
// Host networking for microVMs: one TAP device and one /30 per VM, NAT
// through the host's default interface, and a firewall table that keeps
// VMs away from IMDS, from each other, and from the host itself.

package microvm

import (
	"errors"
	"fmt"
	"math/bits"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// microvmBlockBase is the address block VM subnets are carved from:
// 10.200.0.0.
const microvmBlockBase uint32 = 10<<24 | 200<<16
const microvmBlockPrefix uint32 = 16

// MicrovmMaxSlots is one /30 per VM: 16384 of them in a /16.
const MicrovmMaxSlots uint32 = 1 << (30 - microvmBlockPrefix)

// MicrovmGuestDNS is the VPC DNS resolver address every EC2 instance can
// reach.
const MicrovmGuestDNS = "169.254.169.253"

const microvmTapPrefix = "mjt"
const microvmTable = "mj_microvm"

// MicrovmLink is the addresses for one VM.
type MicrovmLink struct {
	Tap   string
	Host  netip.Addr
	Guest netip.Addr
	MAC   string
}

// MicrovmLinkForSlot is Link::for_slot.
func MicrovmLinkForSlot(slot uint32) (MicrovmLink, error) {
	if slot >= MicrovmMaxSlots {
		return MicrovmLink{}, fmt.Errorf("network slot %d is out of range", slot)
	}
	network := microvmBlockBase + slot*4
	address := func(value uint32) netip.Addr {
		return netip.AddrFrom4([4]byte{byte(value >> 24), byte(value >> 16), byte(value >> 8), byte(value)})
	}
	return MicrovmLink{
		Tap:   fmt.Sprintf("%s%d", microvmTapPrefix, slot),
		Host:  address(network + 1),
		Guest: address(network + 2),
		MAC:   fmt.Sprintf("06:00:0a:c8:%02x:%02x", (slot>>8)&0xff, slot&0xff),
	}, nil
}

// GuestCIDR is `address/30` as the guest configures it.
func (l MicrovmLink) GuestCIDR() string {
	return l.Guest.String() + "/30"
}

// MicrovmIsEngineTap reports whether name is a TAP device name this engine
// could have created.
func MicrovmIsEngineTap(name string) bool {
	rest, ok := strings.CutPrefix(name, microvmTapPrefix)
	if !ok || rest == "" {
		return false
	}
	for i := 0; i < len(rest); i++ {
		if rest[i] < '0' || rest[i] > '9' {
			return false
		}
	}
	return true
}

// MicrovmTapUp creates the TAP device for a VM, addressed on the host side
// and owned by owner, the user the jailed Firecracker runs as: an
// unprivileged process may attach to a persistent TAP only if it owns it. A
// stale device from an earlier run is replaced, since its owner may differ.
func MicrovmTapUp(link MicrovmLink, owner MicrovmOwner) error {
	ip, err := MicrovmFindTool("ip")
	if err != nil {
		return err
	}
	if err := MicrovmTapDown(link.Tap); err != nil {
		return err
	}
	uid, gid := strconv.FormatUint(uint64(owner.UID), 10), strconv.FormatUint(uint64(owner.GID), 10)
	if _, err := MicrovmRunChecked(ip, []string{"tuntap", "add", "dev", link.Tap, "mode", "tap", "user", uid, "group", gid}); err != nil {
		return err
	}
	address := link.Host.String() + "/30"
	if _, err := MicrovmRunChecked(ip, []string{"addr", "replace", address, "dev", link.Tap}); err != nil {
		return err
	}
	if _, err := MicrovmRunChecked(ip, []string{"link", "set", link.Tap, "up"}); err != nil {
		return err
	}
	return nil
}

// MicrovmTapDown removes a TAP device. An absent device is the goal state,
// not an error.
func MicrovmTapDown(tap string) error {
	if !MicrovmIsEngineTap(tap) {
		return fmt.Errorf("%s is not a microvm TAP device name", microvmDebugString(tap))
	}
	ip, err := MicrovmFindTool("ip")
	if err != nil {
		return err
	}
	output, err := MicrovmRunUnchecked(ip, []string{"link", "del", tap})
	if err != nil {
		return err
	}
	if !output.Status.Success() {
		stderr := microvmLossy(output.Stderr)
		if !strings.Contains(stderr, "Cannot find device") {
			return fmt.Errorf("remove TAP device %s: %s", tap, MicrovmTail(stderr, 5))
		}
	}
	return nil
}

// MicrovmTapExists reports whether a TAP device exists.
func MicrovmTapExists(tap string) bool {
	_, err := os.Stat(filepath.Join("/sys/class/net", tap))
	return err == nil
}

// microvmNetRuleset is the nftables ruleset, applied as one atomic
// replacement.
func microvmNetRuleset() string {
	return strings.NewReplacer("{TABLE}", microvmTable, "{TAP_PREFIX}", microvmTapPrefix).Replace(`add table ip6 {TABLE}
delete table ip6 {TABLE}
table ip6 {TABLE} {
    chain input {
        type filter hook input priority filter; policy accept;
        iifname "{TAP_PREFIX}*" drop
    }
    chain forward {
        type filter hook forward priority filter; policy accept;
        iifname "{TAP_PREFIX}*" drop
    }
}
add table ip {TABLE}
delete table ip {TABLE}
table ip {TABLE} {
    chain input {
        type filter hook input priority filter; policy accept;
        iifname "{TAP_PREFIX}*" ct state established,related accept
        iifname "{TAP_PREFIX}*" icmp type echo-request accept
        iifname "{TAP_PREFIX}*" drop
    }
    chain forward {
        type filter hook forward priority filter; policy accept;
        iifname "{TAP_PREFIX}*" ip daddr 169.254.169.254 reject
        iifname "{TAP_PREFIX}*" oifname "{TAP_PREFIX}*" drop
    }
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        iifname "{TAP_PREFIX}*" oifname != "{TAP_PREFIX}*" masquerade
    }
}
`)
}

// MicrovmApplyHostPolicy enables forwarding and installs the firewall
// table. Idempotent; called before every launch so a reboot needs no
// separate setup.
func MicrovmApplyHostPolicy() error {
	if err := os.WriteFile("/proc/sys/net/ipv4/ip_forward", []byte("1"), 0o666); err != nil {
		return fmt.Errorf("enable IPv4 forwarding (/proc/sys/net/ipv4/ip_forward): %w", microvmIOError(err))
	}
	nft, err := MicrovmFindTool("nft")
	if err != nil {
		return err
	}
	output, err := core.RunWithInput(exec.Command(nft, "-f", "-"), []byte(microvmNetRuleset()))
	if err != nil {
		return fmt.Errorf("run nft: %w", err)
	}
	if !output.Status.Success() {
		return fmt.Errorf("install the microvm firewall table with nft: %s", MicrovmTail(microvmLossy(output.Stderr), 10))
	}
	return nil
}

// MicrovmCheckNoRouteOverlap refuses to launch when the VM address block
// overlaps a route the host already has: the VM subnets would shadow that
// network.
func MicrovmCheckNoRouteOverlap() error {
	routes, err := os.ReadFile("/proc/net/route")
	if err != nil {
		return fmt.Errorf("read /proc/net/route: %w", microvmIOError(err))
	}
	if conflict, ok := microvmFindRouteOverlap(string(routes)); ok {
		return errors.New("the host already routes " + conflict + ", which overlaps the microvm address block 10.200.0.0/16")
	}
	return nil
}

// microvmFindRouteOverlap is the first route in `/proc/net/route` text that
// overlaps the VM address block.
func microvmFindRouteOverlap(routes string) (string, bool) {
	lines := microvmLines(routes)
	if len(lines) > 0 {
		lines = lines[1:]
	}
	for _, line := range lines {
		fields := strings.Fields(line)
		if len(fields) < 8 || strings.HasPrefix(fields[0], microvmTapPrefix) {
			continue
		}
		destination, err1 := strconv.ParseUint(fields[1], 16, 32)
		mask, err2 := strconv.ParseUint(fields[7], 16, 32)
		if err1 != nil || err2 != nil {
			continue
		}
		// The kernel prints the in-memory (network order) value as a
		// host-order hex number.
		destination32 := bits.ReverseBytes32(uint32(destination))
		mask32 := bits.ReverseBytes32(uint32(mask))
		if mask32 == 0 {
			continue // the default route reaches everything; it is not a conflict
		}
		shared := mask32 &^ (1<<(32-microvmBlockPrefix) - 1)
		if destination32&shared == microvmBlockBase&shared {
			address := netip.AddrFrom4([4]byte{byte(destination32 >> 24), byte(destination32 >> 16), byte(destination32 >> 8), byte(destination32)})
			return fmt.Sprintf("%s/%d on %s", address, bits.OnesCount32(mask32), fields[0]), true
		}
	}
	return "", false
}
