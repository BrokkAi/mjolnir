// Port of mj-store/src/database/quota_cache.rs.

package store

import (
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

func LoadQuotaCache(identity string) (*client.ProfileQuota, error) {
	panic("unported: mj-store/src/database/quota_cache.rs load_quota_cache")
}

// quotaCacheLoad is the cached reset windows for identity (Rust's
// quota_cache::load).
func quotaCacheLoad(conn Queryer, identity string) (*client.ProfileQuota, error) {
	panic("unported: mj-store/src/database/quota_cache.rs load")
}

// LoadQuotaHistorySince is the probes saved for one configuration of a
// profile at or after since, newest first, failed ones included.
func LoadQuotaHistorySince(identity, profileID string, since uint64) ([]client.ProfileQuota, error) {
	panic("unported: mj-store/src/database/quota_cache.rs load_quota_history_since")
}

// quotaCacheHistorySince is Rust's quota_cache::history_since.
func quotaCacheHistorySince(conn Queryer, identity, profileID string, since uint64) ([]client.ProfileQuota, error) {
	panic("unported: mj-store/src/database/quota_cache.rs history_since")
}

// SaveQuotaReport saves a fresh probe and, on success, its reset cache in one
// transaction. Called on a blocking task through the only database writer.
func SaveQuotaReport(identity string, report *client.ProfileQuota) error {
	panic("unported: mj-store/src/database/quota_cache.rs save_quota_report")
}

// quotaCacheSaveReport is Rust's quota_cache::save_report.
func quotaCacheSaveReport(conn *Connection, identity string, report *client.ProfileQuota) error {
	panic("unported: mj-store/src/database/quota_cache.rs save_report")
}
