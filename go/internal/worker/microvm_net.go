//go:build linux

// Port of mj-worker/src/microvm/net.rs.
//
// Host networking for microVMs: one TAP device and one /30 per VM, NAT
// through the host's default interface, and a firewall table that keeps
// VMs away from IMDS, from each other, and from the host itself.

package worker

import (
	"errors"
	"fmt"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
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
	panic("unported: mj-worker/src/microvm/net.rs tap_up")
}

// MicrovmTapDown removes a TAP device. An absent device is the goal state,
// not an error.
func MicrovmTapDown(tap string) error {
	panic("unported: mj-worker/src/microvm/net.rs tap_down")
}

// MicrovmTapExists reports whether a TAP device exists.
func MicrovmTapExists(tap string) bool {
	_, err := os.Stat(filepath.Join("/sys/class/net", tap))
	return err == nil
}

// microvmNetRuleset is the nftables ruleset, applied as one atomic
// replacement.
func microvmNetRuleset() string {
	panic("unported: mj-worker/src/microvm/net.rs ruleset")
}

// MicrovmApplyHostPolicy enables forwarding and installs the firewall
// table. Idempotent; called before every launch so a reboot needs no
// separate setup.
func MicrovmApplyHostPolicy() error {
	panic("unported: mj-worker/src/microvm/net.rs apply_host_policy")
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
	panic("unported: mj-worker/src/microvm/net.rs find_route_overlap")
}
