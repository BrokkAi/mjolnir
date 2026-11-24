// Port of mj-server/src/server/api/files.rs (only the wire items so far).

package server

// WriteFileResponse is what `PUT /sessions/{id}/files` answers.
type WriteFileResponse struct {
	Path  string `json:"path"`
	Bytes uint64 `json:"bytes"`
}

// FetchedPathHeader is the response header naming where a fetched path
// resolved on the target. It is form-urlencoded, because a path need not be
// valid header text.
const FetchedPathHeader = "x-mj-fetched-path"

// DecodeFetchedPath is the target path a FetchedPathHeader value names, or
// false when it names none.
//
// The value is one form-urlencoded token with no `=` or `&` left in it, so
// it parses as a single key.
func DecodeFetchedPath(value []byte) (string, bool) {
	panic("unported: mj-server/src/server/api/files.rs decode_fetched_path")
}
