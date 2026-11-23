// Port of mj-github-relevance/src/jev_transport.rs.

// Shared bounded HTTP transport for controller-side Jev classifiers.

package githubrelevance

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PostBoundedJSON posts requestBody as JSON to endpoint, without following
// redirects and within ten seconds, and decodes the response as untyped JSON
// (a jsonvalue value, Rust's serde_json::Value). Neither the request nor the
// response may exceed responseLimit bytes. fastinoKey, when not nil, is sent
// in core.FastinoKeyHeader. Every response is reported to
// core.ObserveResponse; a refusal (429) with its body. requestContext labels
// a failure to send.
func PostBoundedJSON(ctx context.Context, endpoint string, fastinoKey *string, requestBody []byte, responseLimit int, requestContext string) (any, error) {
	if len(requestBody) > responseLimit {
		return nil, errors.New("oversized Jev request")
	}
	client := &http.Client{
		Timeout:       10 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(requestBody))
	if err != nil {
		return nil, err
	}
	request.Header.Set("Content-Type", "application/json")
	if fastinoKey != nil {
		value, err := core.FastinoHeader(*fastinoKey)
		if err != nil {
			return nil, err
		}
		request.Header.Set(core.FastinoKeyHeader, value)
	}
	response, err := client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", requestContext, err)
	}
	defer response.Body.Close()
	status := response.StatusCode
	if status != http.StatusTooManyRequests {
		core.ObserveResponse(status, nil, nil)
		if err := errorForStatus(response); err != nil {
			return nil, err
		}
	}
	var retryAfter *string
	if values := response.Header.Values("Retry-After"); len(values) > 0 {
		retryAfter = &values[0]
	}
	if response.ContentLength > int64(responseLimit) {
		return nil, errors.New("oversized Jev response")
	}
	body, err := io.ReadAll(io.LimitReader(response.Body, int64(responseLimit)+1))
	if err != nil {
		return nil, err
	}
	if len(body) > responseLimit {
		return nil, errors.New("oversized Jev response")
	}
	if status == http.StatusTooManyRequests {
		core.ObserveResponse(status, retryAfter, body)
		if err := errorForStatus(response); err != nil {
			return nil, err
		}
	}
	return jsonvalue.Parse(body)
}

// errorForStatus is reqwest's error_for_status: a client or server error
// status as an error naming it and the response's URL.
func errorForStatus(response *http.Response) error {
	var kind string
	switch {
	case response.StatusCode >= 400 && response.StatusCode < 500:
		kind = "client"
	case response.StatusCode >= 500 && response.StatusCode < 600:
		kind = "server"
	default:
		return nil
	}
	status := response.Status
	if status == "" {
		status = fmt.Sprintf("%d %s", response.StatusCode, http.StatusText(response.StatusCode))
	}
	message := fmt.Sprintf("HTTP status %s error (%s)", kind, status)
	if response.Request != nil && response.Request.URL != nil {
		message += fmt.Sprintf(" for url (%s)", response.Request.URL)
	}
	return errors.New(message)
}
