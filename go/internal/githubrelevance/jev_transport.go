// Port of mj-github-relevance/src/jev_transport.rs.

// Shared bounded HTTP transport for controller-side Jev classifiers.

package githubrelevance

import (
	"context"
)

// PostBoundedJSON posts requestBody as JSON to endpoint, without following
// redirects and within ten seconds, and decodes the response as untyped JSON
// (a jsonvalue value, Rust's serde_json::Value). Neither the request nor the
// response may exceed responseLimit bytes. fastinoKey, when not nil, is sent
// in core.FastinoKeyHeader. Every response is reported to
// core.ObserveResponse; a refusal (429) with its body. requestContext labels
// a failure to send.
func PostBoundedJSON(ctx context.Context, endpoint string, fastinoKey *string, requestBody []byte, responseLimit int, requestContext string) (any, error) {
	panic("unported: mj-github-relevance/src/jev_transport.rs post_bounded_json")
}
