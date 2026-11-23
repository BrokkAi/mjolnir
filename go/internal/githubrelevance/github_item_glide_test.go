// Port of the tests in mj-github-relevance/src/github_item_glide.rs.

package githubrelevance

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"fmt"
	"net"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

//go:embed testdata/services/jev-proxy/tests/fixtures/github-item-aliases600.json
var githubItemAliases600 []byte

// aliases600Evidence is the jev-proxy fixture's evidence.
func aliases600Evidence(t *testing.T) core.GithubItemEvidence {
	t.Helper()
	var fixture struct {
		Evidence json.RawMessage `json:"evidence"`
	}
	if err := json.Unmarshal(githubItemAliases600, &fixture); err != nil {
		t.Fatal(err)
	}
	var evidence core.GithubItemEvidence
	if err := json.Unmarshal(fixture.Evidence, &evidence); err != nil {
		t.Fatal(err)
	}
	return evidence
}

func TestGithubGlideHTTPContract(t *testing.T) {
	evidence := aliases600Evidence(t)
	for _, body := range []string{
		`{"probability":0.1123}`,
		`{}`,
		`{"probability":"0.5"}`,
		`{"probability":-0.1}`,
		`{"probability":1.1}`,
		`{"probability":NaN}`,
		`{"probability":1e999}`,
	} {
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		endpoint := fmt.Sprintf("http://%s/v1/github-item-glide", listener.Addr())
		expected, err := serdeJSON(&evidence)
		if err != nil {
			t.Fatal(err)
		}
		var ownKey *string
		if strings.Contains(body, "0.1123") {
			key := "test-personal-key"
			ownKey = &key
		}
		served := make(chan error, 1)
		go func() {
			served <- func() error {
				stream, err := listener.Accept()
				if err != nil {
					return err
				}
				defer stream.Close()
				var request []byte
				headerEnd := -1
				for headerEnd < 0 {
					buffer := make([]byte, 4096)
					n, err := stream.Read(buffer)
					if n == 0 {
						return fmt.Errorf("connection closed before headers: %v", err)
					}
					request = append(request, buffer[:n]...)
					if end := bytes.Index(request, []byte("\r\n\r\n")); end >= 0 {
						headerEnd = end + 4
					}
				}
				header := string(request[:headerEnd])
				if !strings.HasPrefix(header, "POST /v1/github-item-glide HTTP/1.1") {
					return fmt.Errorf("unexpected request line: %q", header)
				}
				headers := strings.ToLower(header)
				if strings.Contains(headers, "authorization:") {
					return fmt.Errorf("unexpected authorization header: %q", header)
				}
				if strings.Contains(headers, "x-fastino-key: test-personal-key") != (ownKey != nil) {
					return fmt.Errorf("Fastino key header mismatch: %q", header)
				}
				length := -1
				for _, line := range strings.Split(header, "\r\n") {
					if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
						if length, err = strconv.Atoi(strings.TrimSpace(value)); err != nil {
							return err
						}
					}
				}
				if length < 0 {
					return fmt.Errorf("no content length: %q", header)
				}
				for len(request) < headerEnd+length {
					buffer := make([]byte, 4096)
					n, err := stream.Read(buffer)
					if n == 0 {
						return fmt.Errorf("connection closed before body: %v", err)
					}
					request = append(request, buffer[:n]...)
				}
				if !bytes.Equal(request[headerEnd:], expected) {
					return fmt.Errorf("request body differs:\n got %s\nwant %s", request[headerEnd:], expected)
				}
				_, err = fmt.Fprintf(stream, "HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", len(body), body)
				return err
			}()
		}()
		result, err := classifyAt(context.Background(), &evidence, endpoint, ownKey)
		if serveErr := <-served; serveErr != nil {
			t.Fatal(serveErr)
		}
		listener.Close()
		if strings.Contains(body, "0.1123") {
			if err != nil || result != 0.1123 {
				t.Fatalf("%s: %v, %v", body, result, err)
			}
		} else if err == nil {
			t.Fatalf("%s: accepted %v", body, result)
		}
	}
}
