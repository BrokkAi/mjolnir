// Port of mj-server/src/server/api.rs (only the contract constants so far).

package server

// APIVersionHeader is the response header naming the contract version this
// server speaks. A client that understands only version 1 can refuse
// anything else without parsing a body it may not recognize.
const APIVersionHeader = "mj-api-version"

// APIVersion is the major version of the `/api/v1` contract.
const APIVersion = "1"

// DefaultWaitSecs is how long a wait blocks when the caller names no
// timeout, and MaxWaitSecs the ceiling it may ask for. Both are generous: a
// turn routinely runs for minutes, and the caller is a program that
// reconnects rather than a person holding a page.
const (
	DefaultWaitSecs uint64 = 600
	MaxWaitSecs     uint64 = 3_600
)
