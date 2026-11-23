// Port of mj-store/src/database/ec2_grants.rs.

// Transactional SQLite persistence for shared EC2 Move ingress grants and
// the daemon's stable instance identifier.

package store

import (
	"strings"
)

// Ec2MoveGrantKeyRow identifies one grant. Rows order by security group,
// CIDR, then port (Rust's derived Ord); see Compare.
type Ec2MoveGrantKeyRow struct {
	SecurityGroupID string
	CidrIP          string
	Port            uint16
}

// Compare orders keys as Rust's derived Ord does.
func (k Ec2MoveGrantKeyRow) Compare(other Ec2MoveGrantKeyRow) int {
	if c := strings.Compare(k.SecurityGroupID, other.SecurityGroupID); c != 0 {
		return c
	}
	if c := strings.Compare(k.CidrIP, other.CidrIP); c != 0 {
		return c
	}
	switch {
	case k.Port < other.Port:
		return -1
	case k.Port > other.Port:
		return 1
	}
	return 0
}

type Ec2MoveGrantRow struct {
	Key   Ec2MoveGrantKeyRow
	State string
	// Users is a set (Rust's BTreeSet); it is stored as a sorted JSON array.
	Users        map[string]struct{}
	AbsentChecks uint32
	AttemptID    uint64
}

// DaemonInstanceID returns the stable daemon instance ID recorded in the
// store, generating and persisting one on first use.
func DaemonInstanceID() (string, error) {
	panic("unported: mj-store/src/database/ec2_grants.rs daemon_instance_id")
}

// WithEc2MoveGrantsTransaction runs an atomic read-modify-write transaction
// over all ec2_move_grants rows: f edits the map, and the rows are written
// back as f left them.
func WithEc2MoveGrantsTransaction[R any](f func(records map[Ec2MoveGrantKeyRow]*Ec2MoveGrantRow) (R, error)) (R, error) {
	panic("unported: mj-store/src/database/ec2_grants.rs with_ec2_move_grants_transaction")
}
