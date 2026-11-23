// Port of the SQL scripts in mj-store/src/database/ that the migrations in
// database/schema.rs run (baseline.sql, environments.sql, web_push.sql,
// web_push_entries.sql, alert_events.sql). The scripts are byte-identical
// copies under data/database/, because the Go and Rust builds share one
// schema and its migration history.

package store

import _ "embed"

// baselineSQL is the schema an empty store is created with before it is
// brought forward through the later migrations (database/baseline.sql).
//
//go:embed data/database/baseline.sql
var baselineSQL string

// environmentsSQL creates the environment tables of revision 122
// (database/environments.sql).
//
//go:embed data/database/environments.sql
var environmentsSQL string

// webPushSQL creates the standalone push tables of revision 127
// (database/web_push.sql).
//
//go:embed data/database/web_push.sql
var webPushSQL string

// webPushEntriesSQL creates the push ledger of revision 138
// (database/web_push_entries.sql).
//
//go:embed data/database/web_push_entries.sql
var webPushEntriesSQL string

// alertEventsSQL creates the alert ledger of revision 139
// (database/alert_events.sql).
//
//go:embed data/database/alert_events.sql
var alertEventsSQL string
