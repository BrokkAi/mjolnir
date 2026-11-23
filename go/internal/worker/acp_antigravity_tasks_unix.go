// Port of mj-worker/src/acp/antigravity_tasks.rs: the Unix file identity
// and the Stop-hook digest sample.

//go:build unix

package worker

import (
	"fmt"
	"os"
	"syscall"
)

// antigravitySummaryIdentity is the summary database's device and inode.
func antigravitySummaryIdentity(meta os.FileInfo) [2]uint64 {
	stat, ok := meta.Sys().(*syscall.Stat_t)
	if !ok {
		return [2]uint64{}
	}
	return [2]uint64{uint64(stat.Dev), uint64(stat.Ino)}
}

// ReadSummaryRowDigest samples the committed row while Antigravity is
// waiting for its Stop hook; nil without a row.
func ReadSummaryRowDigest(home string, conversationID string) (*[32]byte, error) {
	if err := antigravityValidateConversationID(conversationID); err != nil {
		return nil, err
	}
	path := antigravitySummaryDBPath(home)
	exists, err := pathTryExists(path)
	if err != nil {
		return nil, fmt.Errorf("locate Antigravity summary database: %w", err)
	}
	if !exists {
		return nil, nil
	}
	database, err := antigravityOpenReadonly(path)
	if err != nil {
		return nil, err
	}
	defer database.close()
	row, err := antigravityReadSummaryRow(database.conn, conversationID)
	if err != nil || row == nil {
		return nil, err
	}
	digest := row.digest()
	return &digest, nil
}
