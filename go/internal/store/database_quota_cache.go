// Port of mj-store/src/database/quota_cache.rs.

package store

import (
	"database/sql"
	"encoding/json"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

func LoadQuotaCache(identity string) (*client.ProfileQuota, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return quotaCacheLoad(connection, identity)
}

// quotaCacheLoad is the cached reset windows for identity (Rust's
// quota_cache::load).
func quotaCacheLoad(conn Queryer, identity string) (*client.ProfileQuota, error) {
	var body string
	found, err := valuesOptional(conn.QueryRow("SELECT body FROM quota_reset_cache WHERE identity = ?1", identity).Scan(&body))
	if err != nil || !found {
		return nil, err
	}
	var quota client.ProfileQuota
	if err := json.Unmarshal([]byte(body), &quota); err != nil {
		return nil, fmt.Errorf("decode cached quota resets: %w", err)
	}
	return &quota, nil
}

// LoadQuotaHistorySince is the probes saved for one configuration of a
// profile at or after since, newest first, failed ones included.
func LoadQuotaHistorySince(identity, profileID string, since uint64) ([]client.ProfileQuota, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return quotaCacheHistorySince(connection, identity, profileID, since)
}

// quotaCacheHistorySince is Rust's quota_cache::history_since.
func quotaCacheHistorySince(conn Queryer, identity, profileID string, since uint64) ([]client.ProfileQuota, error) {
	if since > 1<<63-1 {
		return nil, fmt.Errorf("integer %d is too large to fit into an SQLite INTEGER", since)
	}
	bodies, err := valuesQueryMap(conn, `SELECT body FROM quota_history
             WHERE profile_id = ?1 AND sampled_at_epoch_seconds >= ?2 AND identity = ?3
             ORDER BY sampled_at_epoch_seconds DESC, id DESC`, []any{profileID, int64(since), identity},
		func(rows *sql.Rows) (string, error) {
			var body string
			err := rows.Scan(&body)
			return body, err
		})
	if err != nil {
		return nil, err
	}
	history := make([]client.ProfileQuota, 0, len(bodies))
	for _, body := range bodies {
		var quota client.ProfileQuota
		if err := json.Unmarshal([]byte(body), &quota); err != nil {
			return nil, fmt.Errorf("decode stored quota probe: %w", err)
		}
		history = append(history, quota)
	}
	return history, nil
}

// SaveQuotaReport saves a fresh probe and, on success, its reset cache in one
// transaction. Called on a blocking task through the only database writer.
func SaveQuotaReport(identity string, report *client.ProfileQuota) error {
	// The writer runs the job before this returns, so the caller's report
	// stays unchanged while it does.
	_, err := submitDatabaseWrite("save_quota_report", func(connection *Connection) (struct{}, error) {
		return struct{}{}, quotaCacheSaveReport(connection, identity, report)
	})
	return err
}

// quotaCacheSaveReport is Rust's quota_cache::save_report.
func quotaCacheSaveReport(conn *Connection, identity string, report *client.ProfileQuota) error {
	transaction, err := conn.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	body, err := databaseMarshalJSON(report)
	if err != nil {
		return err
	}
	if report.RefreshedAtEpochSeconds > 1<<63-1 {
		return fmt.Errorf("integer %d is too large to fit into an SQLite INTEGER", report.RefreshedAtEpochSeconds)
	}
	// Preserve the observation before the reset cache fills missing fields.
	// Distinct probes can have identical values and timestamps.
	if _, err := transaction.Exec(`INSERT INTO quota_history(identity, profile_id, harness, sampled_at_epoch_seconds, body)
         VALUES (?1, ?2, ?3, ?4, ?5)`,
		identity, report.ProfileID, report.Harness.ID(), int64(report.RefreshedAtEpochSeconds), string(body)); err != nil {
		return err
	}
	if report.Error != nil {
		return transaction.Commit()
	}
	merged := *report
	merged.Windows = append([]client.QuotaWindow(nil), report.Windows...)
	if !report.IsUsagePriced() {
		previous, err := quotaCacheLoad(transaction, identity)
		if err != nil {
			return err
		}
		if previous != nil {
			client.MergeResetWindows(&merged.Windows, previous.Windows)
		}
	}
	mergedBody, err := databaseMarshalJSON(&merged)
	if err != nil {
		return err
	}
	if _, err := transaction.Exec("INSERT INTO quota_reset_cache(identity, body) VALUES (?1, ?2) ON CONFLICT(identity) DO UPDATE SET body = excluded.body",
		identity, string(mergedBody)); err != nil {
		return err
	}
	return transaction.Commit()
}
