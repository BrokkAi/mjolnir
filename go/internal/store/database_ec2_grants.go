// Port of mj-store/src/database/ec2_grants.rs.

// Transactional SQLite persistence for shared EC2 Move ingress grants and
// the daemon's stable instance identifier.

package store

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	return submitDatabaseWrite("daemon_instance_id", func(connection *Connection) (string, error) {
		tx, err := connection.Begin()
		if err != nil {
			return "", err
		}
		defer tx.Rollback()
		var existing string
		found, err := valuesOptional(tx.QueryRow("SELECT instance_id FROM daemon_instance_metadata WHERE singleton = 1").Scan(&existing))
		if err != nil {
			return "", err
		}
		if found {
			if err := tx.Commit(); err != nil {
				return "", err
			}
			return existing, nil
		}
		id, err := core.NewWorkspaceID()
		if err != nil {
			return "", err
		}
		now := core.FormatChronoRFC3339(time.Now())
		if _, err := tx.Exec(`INSERT INTO daemon_instance_metadata(singleton, instance_id, created_at)
             VALUES (1, ?1, ?2)`, id, now); err != nil {
			return "", err
		}
		if err := tx.Commit(); err != nil {
			return "", err
		}
		return id, nil
	})
}

// WithEc2MoveGrantsTransaction runs an atomic read-modify-write transaction
// over all ec2_move_grants rows: f edits the map, and the rows are written
// back as f left them.
func WithEc2MoveGrantsTransaction[R any](f func(records map[Ec2MoveGrantKeyRow]*Ec2MoveGrantRow) (R, error)) (R, error) {
	return submitDatabaseWrite("ec2_move_grants_tx", func(connection *Connection) (R, error) {
		var zero R
		tx, err := connection.Begin()
		if err != nil {
			return zero, err
		}
		defer tx.Rollback()
		records := map[Ec2MoveGrantKeyRow]*Ec2MoveGrantRow{}
		rows, err := tx.Query(`SELECT security_group_id, cidr_ip, port, state, users_json, absent_checks, attempt_id
                 FROM ec2_move_grants`)
		if err != nil {
			return zero, err
		}
		for rows.Next() {
			var sg, cidr, state, usersJSON string
			var port, absentChecks, attemptID int64
			if err := rows.Scan(&sg, &cidr, &port, &state, &usersJSON, &absentChecks, &attemptID); err != nil {
				rows.Close()
				return zero, err
			}
			if port < 0 || port > math.MaxUint16 {
				rows.Close()
				return zero, fmt.Errorf("invalid ec2_move_grants port: out of range integral type conversion attempted")
			}
			key := Ec2MoveGrantKeyRow{SecurityGroupID: sg, CidrIP: cidr, Port: uint16(port)}
			var userList *[]string
			if err := json.Unmarshal([]byte(usersJSON), &userList); err != nil || userList == nil {
				rows.Close()
				if err == nil {
					err = errors.New("invalid type: null, expected a sequence")
				}
				return zero, fmt.Errorf("decode ec2_move_grants.users_json: %w", err)
			}
			users := make(map[string]struct{}, len(*userList))
			for _, user := range *userList {
				users[user] = struct{}{}
			}
			if absentChecks < 0 || absentChecks > math.MaxUint32 {
				rows.Close()
				return zero, fmt.Errorf("invalid ec2_move_grants absent_checks: out of range integral type conversion attempted")
			}
			if attemptID < 0 {
				rows.Close()
				return zero, fmt.Errorf("invalid ec2_move_grants attempt_id: out of range integral type conversion attempted")
			}
			records[key] = &Ec2MoveGrantRow{
				Key:          key,
				State:        state,
				Users:        users,
				AbsentChecks: uint32(absentChecks),
				AttemptID:    uint64(attemptID),
			}
		}
		if err := rows.Err(); err != nil {
			rows.Close()
			return zero, err
		}
		if err := rows.Close(); err != nil {
			return zero, err
		}
		beforeKeys := make([]Ec2MoveGrantKeyRow, 0, len(records))
		for key := range records {
			beforeKeys = append(beforeKeys, key)
		}
		slices.SortFunc(beforeKeys, Ec2MoveGrantKeyRow.Compare)
		result, err := f(records)
		if err != nil {
			return zero, err
		}
		now := core.FormatChronoRFC3339(time.Now())
		for _, key := range beforeKeys {
			if _, kept := records[key]; !kept {
				if _, err := tx.Exec(`DELETE FROM ec2_move_grants
                     WHERE security_group_id = ?1 AND cidr_ip = ?2 AND port = ?3`,
					key.SecurityGroupID, key.CidrIP, int64(key.Port)); err != nil {
					return zero, err
				}
			}
		}
		keys := make([]Ec2MoveGrantKeyRow, 0, len(records))
		for key := range records {
			keys = append(keys, key)
		}
		slices.SortFunc(keys, Ec2MoveGrantKeyRow.Compare)
		for _, key := range keys {
			row := records[key]
			users := make([]string, 0, len(row.Users))
			for user := range row.Users {
				users = append(users, user)
			}
			slices.Sort(users)
			usersJSON, err := databaseMarshalJSON(users)
			if err != nil {
				return zero, err
			}
			if row.AttemptID > math.MaxInt64 {
				return zero, fmt.Errorf("ec2_move_grants attempt_id overflow: out of range integral type conversion attempted")
			}
			if _, err := tx.Exec(`INSERT INTO ec2_move_grants(
                     security_group_id, cidr_ip, port, state, users_json, absent_checks, attempt_id, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(security_group_id, cidr_ip, port) DO UPDATE SET
                     state = excluded.state,
                     users_json = excluded.users_json,
                     absent_checks = excluded.absent_checks,
                     attempt_id = excluded.attempt_id,
                     updated_at = excluded.updated_at`,
				key.SecurityGroupID, key.CidrIP, int64(key.Port), row.State, string(usersJSON),
				int64(row.AbsentChecks), int64(row.AttemptID), now); err != nil {
				return zero, err
			}
		}
		if err := tx.Commit(); err != nil {
			return zero, err
		}
		return result, nil
	})
}
