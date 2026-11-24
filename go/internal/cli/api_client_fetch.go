// Port of mj-cli/src/api_client/fetch.rs.
//
// Disk-backed downloads. Retrying a GET after daemon replacement is safe;
// only a complete download can be published or written to stdout.

package cli

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
)

// fetchTransportError marks a failure reading the fetched body, which with
// a failed send (a *url.Error) is what reqwest calls a connect, request,
// body or decode error: the transport, not the daemon's answer, failed.
type fetchTransportError struct{ err error }

func (e fetchTransportError) Error() string { return e.err.Error() }
func (e fetchTransportError) Unwrap() error { return e.err }

// isFetchTransportFailure is whether err's chain holds a transport failure.
func isFetchTransportFailure(err error) bool {
	var sent *url.Error
	var body fetchTransportError
	return errors.As(err, &sent) || errors.As(err, &body)
}

// fetch copies a path from the session's target, following a daemon
// replacement: after a handoff or a broken transport to a daemon that was
// replaced, it reconnects and downloads again.
func (c *apiClient) fetch(ctx context.Context, session, path string, list bool) (*checkpoint.FetchedPath, error) {
	var replacement *apiClient
	for {
		current := c
		if replacement != nil {
			current = replacement
		}
		fetched, err := current.fetchOnce(ctx, session, path, list)
		if err == nil {
			return fetched, nil
		}
		transportFailed := isFetchTransportFailure(err)
		explicitShutdown := isDaemonHandoff(err)
		if !explicitShutdown && (!transportFailed || current.instance == nil) {
			return nil, err
		}
		// Reconnect first. API admission refuses every fresh request after
		// shutdown, so this probe cannot return a shutting-down old daemon.
		next, connectErr := apiClientConnectAfterHandoff(ctx)
		if connectErr != nil {
			return nil, connectErr
		}
		if !explicitShutdown && sameInstance(next.instance, current.instance) {
			return nil, err
		}
		c.waitNotice("The daemon connection was interrupted during fetch; reconnecting and retrying the download.")
		replacement = next
	}
}

// sameInstance compares two optional daemon instances as Rust's Option ==.
func sameInstance(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// fetchOnce downloads path into a spool file and checks it arrived whole.
func (c *apiClient) fetchOnce(ctx context.Context, session, path string, list bool) (*checkpoint.FetchedPath, error) {
	request := newAPIRequest(http.MethodGet, c.url("/sessions/"+session+"/files")).
		withQuery([2]string{"path", path})
	if list {
		request.withQuery([2]string{"list", "true"})
	}
	response, err := c.send(ctx, request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	var kind checkpoint.FetchedKind
	contentType := response.Header.Values("Content-Type")
	switch {
	case len(contentType) > 0 && contentType[0] == "application/octet-stream":
		kind = checkpoint.FetchedKindFile
	case len(contentType) > 0 && contentType[0] == "application/zip":
		kind = checkpoint.FetchedKindDirectory
	case len(contentType) > 0 && contentType[0] == "application/json" && list:
		kind = checkpoint.FetchedKindListing
	default:
		other := "None"
		if len(contentType) > 0 {
			other = "Some(" + core.RustDebugString(core.LossyUTF8([]byte(contentType[0]))) + ")"
		}
		return nil, fmt.Errorf("the daemon answered a fetch with content type %s", other)
	}
	var resolved string
	found := false
	if values := response.Header.Values(server.FetchedPathHeader); len(values) > 0 {
		resolved, found = server.DecodeFetchedPath([]byte(values[0]))
	}
	if !found {
		return nil, errors.New("the daemon did not say where the path resolved; it predates `mj fetch`")
	}
	expected := response.ContentLength
	file, err := checkpoint.CreateFetchSpool()
	if err != nil {
		return nil, err
	}
	received, err := spoolFetchedBody(file, response.Body)
	if err != nil {
		file.Close()
		return nil, err
	}
	if expected >= 0 && uint64(received) != uint64(expected) {
		file.Close()
		return nil, fmt.Errorf("incomplete fetch download: expected %d bytes, received %d", expected, received)
	}
	return checkpoint.FetchedPathFromFile(kind, resolved, file)
}

// spoolFetchedBody copies the body into the spool chunk by chunk, telling a
// read failure (the transport) from a write failure (the spool).
func spoolFetchedBody(file io.Writer, body io.Reader) (int64, error) {
	buffer := make([]byte, 64*1024)
	var received int64
	for {
		read, readErr := body.Read(buffer)
		if read > 0 {
			if _, err := file.Write(buffer[:read]); err != nil {
				return received, fmt.Errorf("spool fetched payload: %w", err)
			}
			received += int64(read)
		}
		if readErr == io.EOF {
			return received, nil
		}
		if readErr != nil {
			return received, fmt.Errorf("read the fetched path: %w", fetchTransportError{err: readErr})
		}
	}
}
