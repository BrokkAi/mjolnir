// Port of the tests in mj-cli/src/api_client.rs.

package cli

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"math/big"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"reflect"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
)

// seenRequests is what a canned server recorded, in order.
type seenRequests struct {
	mu      sync.Mutex
	entries []string
}

func (s *seenRequests) push(entry string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.entries = append(s.entries, entry)
}

func (s *seenRequests) snapshot() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]string{}, s.entries...)
}

func sampleSession(id string) map[string]any {
	return map[string]any{
		"id":           id,
		"workspace_id": "workspace-1",
		"title":        "a session",
		"harness_kind": "claude",
		"profile_id":   "profile-1",
		"target_id":    "target-1",
		"bundle_id":    "bundle-1",
		"state":        "Running",
		"lifecycle":    "live",
		"chat_phase":   "idle",
		"is_idle":      true,
		"has_error":    false,
		"created_at":   "2026-09-11T00:00:00Z",
		"updated_at":   "2026-09-11T00:00:01Z",
	}
}

func writeTestJSON(t *testing.T, w http.ResponseWriter, status int, value any) {
	body, err := json.Marshal(value)
	if err != nil {
		t.Errorf("encode canned response: %v", err)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	if _, err := w.Write(body); err != nil {
		t.Errorf("write canned response: %v", err)
	}
}

// withVersion stamps every response with version, as the Rust tests'
// middleware does; nil stamps nothing.
func withVersion(version *string, next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if version != nil {
			w.Header().Set(server.APIVersionHeader, *version)
		}
		next.ServeHTTP(w, r)
	})
}

func recordAuthorization(seen *seenRequests, r *http.Request) {
	if values := r.Header.Values("Authorization"); len(values) > 0 {
		seen.push(values[0])
	}
}

// serveAPI is a canned server standing in for the daemon: it records what
// the client sent and stamps whichever contract version the test is about.
func serveAPI(t *testing.T, versionHeader *string) (string, *seenRequests) {
	t.Helper()
	seen := &seenRequests{}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/v1/sessions", func(w http.ResponseWriter, r *http.Request) {
		recordAuthorization(seen, r)
		writeTestJSON(t, w, http.StatusOK, map[string]any{"sessions": []any{sampleSession("session-1")}})
	})
	mux.HandleFunc("GET /api/v1/sessions/{session_id}", func(w http.ResponseWriter, r *http.Request) {
		recordAuthorization(seen, r)
		writeTestJSON(t, w, http.StatusOK, sampleSession(r.PathValue("session_id")))
	})
	mux.HandleFunc("GET /api/v1/sessions/{session_id}/files", func(w http.ResponseWriter, r *http.Request) {
		recordAuthorization(seen, r)
		seen.push("path=" + r.URL.Query().Get("path"))
		w.Header().Set("Content-Type", "application/octet-stream")
		w.Header().Set(server.FetchedPathHeader, "%2Fworkspace%2Fdocs%2FREADME.md")
		if _, err := w.Write([]byte("file bytes")); err != nil {
			t.Errorf("write canned file: %v", err)
		}
	})
	mux.HandleFunc("GET /api/v1/sessions/{session_id}/diff", func(w http.ResponseWriter, r *http.Request) {
		recordAuthorization(seen, r)
		query := r.URL.Query()
		if query.Get("json") == "true" {
			seen.push("base=" + query.Get("base"))
			writeTestJSON(t, w, http.StatusOK, map[string]any{
				"diff": "+task work\n", "base": strings.Repeat("a", 40), "head": strings.Repeat("c", 40),
				"head_descends_from_base": false,
			})
			return
		}
		writeTestJSON(t, w, http.StatusConflict, map[string]any{"error": "no session base recorded"})
	})
	listening := httptest.NewServer(withVersion(versionHeader, mux))
	t.Cleanup(listening.Close)
	return listening.URL, seen
}

func TestDiffMetadataRequestsEncodeTheRevisionAndPreserveResolvedCommits(t *testing.T) {
	url, seen := serveAPI(t, pointer("1"))
	client, err := newAPIClient(url, "secret-token")
	check(t, err)
	body, err := client.diff(t.Context(), "session-1", pointer("HEAD@{1}"), true)
	check(t, err)
	var diff checkpoint.SessionDiff
	check(t, json.Unmarshal([]byte(body), &diff))
	expected := checkpoint.SessionDiff{
		Diff:                 "+task work\n",
		Base:                 strings.Repeat("a", 40),
		Head:                 strings.Repeat("c", 40),
		HeadDescendsFromBase: pointer(false),
	}
	if !reflect.DeepEqual(diff, expected) {
		t.Fatalf("diff = %#v, want %#v", diff, expected)
	}
	if got := seen.snapshot(); !slices.Equal(got, []string{"Bearer secret-token", "base=HEAD@{1}"}) {
		t.Fatalf("seen = %q", got)
	}

	// A worker from before the field answers without it, and the absence
	// reads as "unknown" rather than failing the whole decode.
	var older checkpoint.SessionDiff
	check(t, json.Unmarshal([]byte(`{"diff":"+task work\n","base":"aaaa","head":"cccc"}`), &older))
	if older.HeadDescendsFromBase != nil {
		t.Fatalf("head_descends_from_base = %v, want none", *older.HeadDescendsFromBase)
	}
}

func TestEveryCallCarriesTheBearerTokenAndReadsTheTypedResponse(t *testing.T) {
	url, seen := serveAPI(t, pointer("1"))
	client, err := newAPIClient(url, "secret-token")
	check(t, err)

	sessions, err := client.sessionsInWorkspace(t.Context(), nil, true)
	check(t, err)
	if sessions.Sessions[0].ID != "session-1" {
		t.Fatalf("first session = %q", sessions.Sessions[0].ID)
	}
	session, err := client.sessionIfKnown(t.Context(), "session-2")
	check(t, err)
	if session == nil {
		t.Fatal("the route answers with the session")
	}
	if session.ID != "session-2" {
		t.Fatalf("session = %q", session.ID)
	}
	fetched, err := client.fetch(t.Context(), "session-1", "docs/README.md", false)
	check(t, err)
	defer fetched.Close()
	if fetched.Kind != checkpoint.FetchedKindFile {
		t.Fatalf("kind = %q", fetched.Kind)
	}
	if fetched.Path != "/workspace/docs/README.md" {
		t.Fatalf("path = %q", fetched.Path)
	}
	bytes, err := io.ReadAll(fetched.Reader())
	check(t, err)
	if string(bytes) != "file bytes" {
		t.Fatalf("bytes = %q", bytes)
	}

	entries := seen.snapshot()
	authenticated := 0
	for _, entry := range entries {
		if entry == "Bearer secret-token" {
			authenticated++
		}
	}
	if authenticated != 3 {
		t.Fatalf("every call authenticates: %q", entries)
	}
	if !slices.Contains(entries, "path=docs/README.md") {
		t.Fatalf("the file path reaches the server as a query: %q", entries)
	}

	// A refusal reaches the caller as the reason the API gave it.
	_, err = client.diff(t.Context(), "session-1", nil, false)
	if err == nil || !strings.Contains(err.Error(), "no session base recorded") {
		t.Fatalf("unexpected error: %v", err)
	}
}

// A 429 is sent before the daemon admits anything, so the client sends the
// request again until it is admitted, and gives up with the daemon's own
// reason once its time runs out.
// Hard-won: b6e05b62: a 429 refusal for an unaccepted lifecycle request made sequential session creation fail.
func TestABusyDaemonIsRetriedUntilItAdmitsTheRequest(t *testing.T) {
	var refusals struct {
		sync.Mutex
		left uint64
	}
	refusals.left = 2
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/v1/sessions/{session_id}/prompt", func(w http.ResponseWriter, r *http.Request) {
		refusals.Lock()
		defer refusals.Unlock()
		if refusals.left > 0 {
			refusals.left--
			writeTestJSON(t, w, http.StatusTooManyRequests, map[string]any{
				"error":           "at its concurrent action limit",
				"running_actions": 4,
				"action_limit":    4,
			})
			return
		}
		writeTestJSON(t, w, http.StatusAccepted, map[string]any{"turn_id": 7})
	})
	listening := httptest.NewServer(withVersion(pointer("1"), mux))
	defer listening.Close()
	client, err := newAPIClient(listening.URL, "secret-token")
	check(t, err)
	var notices seenRequests
	client.waitNotice = notices.push
	client.busyRetry = busyRetry{
		firstDelay: 10 * time.Millisecond,
		maxDelay:   20 * time.Millisecond,
		limit:      5 * time.Second,
	}

	response, err := client.prompt(t.Context(), "session-1", "hello")
	check(t, err)
	if response.TurnID != 7 {
		t.Fatalf("turn_id = %d", response.TurnID)
	}
	refusals.Lock()
	left := refusals.left
	refusals.Unlock()
	if left != 0 {
		t.Fatalf("refusals left = %d", left)
	}
	// Two refusals were retried, and the person saw one line about them.
	if got := notices.snapshot(); !slices.Equal(got, []string{
		"Waiting for the daemon: 4 of 4 actions are in use (sessions still starting). Retrying until one frees up.",
	}) {
		t.Fatalf("notices = %q", got)
	}
	if got := waitingNotice(nil); got != "Waiting for the daemon: all of its actions are in use (sessions still starting). Retrying until one frees up." {
		t.Fatalf("notice without counts = %q", got)
	}

	refusals.Lock()
	refusals.left = ^uint64(0)
	refusals.Unlock()
	client.busyRetry.limit = 50 * time.Millisecond
	_, err = client.prompt(t.Context(), "session-1", "hello")
	if err == nil || !strings.Contains(err.Error(), "concurrent action limit") {
		t.Fatalf("unexpected error: %v", err)
	}
}

// A suspend answers with how many sub-agents it stops and a warning for the
// ones still at work. An older daemon answers with no body, which reads as
// no warning; it still gets the acknowledgement it requires.
func TestASuspendReadsTheSubAgentWarningAndAnEmptyAnswerAsNone(t *testing.T) {
	warning, err := json.Marshal(map[string]any{
		"session_id":                "session-1",
		"stopped_subagents":         2,
		"subagents_not_handed_back": 1,
		"warning":                   "1 sub-agent has not handed back; suspending stops it",
	})
	check(t, err)
	var answers struct {
		sync.Mutex
		stack []string
	}
	answers.stack = []string{"", string(warning)}
	bodies := &seenRequests{}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/v1/sessions/{session_id}/suspend", func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read suspend body: %v", err)
		}
		bodies.push(string(body))
		answers.Lock()
		answer := answers.stack[len(answers.stack)-1]
		answers.stack = answers.stack[:len(answers.stack)-1]
		answers.Unlock()
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		w.WriteHeader(http.StatusAccepted)
		if _, err := w.Write([]byte(answer)); err != nil {
			t.Errorf("write suspend answer: %v", err)
		}
	})
	listening := httptest.NewServer(withVersion(pointer("1"), mux))
	defer listening.Close()
	client, err := newAPIClient(listening.URL, "secret-token")
	check(t, err)

	warned, err := client.suspend(t.Context(), "session-1", &core.SuspendConsent{})
	check(t, err)
	if warned.StoppedSubagents != 2 || warned.SubagentsNotHandedBack != 1 {
		t.Fatalf("warned = %+v", warned)
	}
	if warned.Warning == nil || *warned.Warning != "1 sub-agent has not handed back; suspending stops it" {
		t.Fatalf("warning = %v", warned.Warning)
	}
	older, err := client.suspend(t.Context(), "session-1", &core.SuspendConsent{
		DiscardUntracked: core.UntrackedConsent{Kind: core.ConsentAny},
	})
	check(t, err)
	if older.SessionID != "session-1" {
		t.Fatalf("session_id = %q", older.SessionID)
	}
	if older.Warning != nil {
		t.Fatalf("warning = %q, want none", *older.Warning)
	}
	sent := bodies.snapshot()
	var first, second map[string]any
	check(t, json.Unmarshal([]byte(sent[0]), &first))
	if first["discard_untracked"] != "None" || first["abort_operations"] != "None" || first["without_checkpoint"] != false {
		t.Fatalf("first body = %v", first)
	}
	check(t, json.Unmarshal([]byte(sent[1]), &second))
	if second["discard_untracked"] != "Any" || second["abort_operations"] != "None" || second["without_checkpoint"] != false {
		t.Fatalf("second body = %v", second)
	}
}

func TestApiProbeSucceedsWithoutReadingAToken(t *testing.T) {
	url, seen := serveAPI(t, pointer("1"))
	plain, err := httpClient(nil, nil)
	check(t, err)
	check(t, probeAPI(t.Context(), plain, url))
	if got := seen.snapshot(); len(got) != 0 {
		t.Fatalf("the support probe must not need authentication: %q", got)
	}
	url, _ = serveAPI(t, pointer("2"))
	if err := probeAPI(t.Context(), plain, url); err == nil || !strings.Contains(err.Error(), "version 2") {
		t.Fatalf("unexpected error: %v", err)
	}
	empty := httptest.NewServer(http.NewServeMux())
	defer empty.Close()
	if err := probeAPI(t.Context(), plain, empty.URL); err == nil || !strings.Contains(err.Error(), "mj daemon restart") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestAResponseWithoutThisContractVersionIsRefused(t *testing.T) {
	url, _ := serveAPI(t, nil)
	client, err := newAPIClient(url, "secret-token")
	check(t, err)
	_, err = client.sessionsInWorkspace(t.Context(), nil, true)
	if err == nil || !strings.Contains(err.Error(), server.APIVersionHeader) {
		t.Fatalf("unexpected error: %v", err)
	}

	url, _ = serveAPI(t, pointer("2"))
	client, err = newAPIClient(url, "secret-token")
	check(t, err)
	_, err = client.sessionsInWorkspace(t.Context(), nil, true)
	if err == nil || !strings.Contains(err.Error(), "version 2") {
		t.Fatalf("unexpected error: %v", err)
	}
}

// selfSignedCertificate is rcgen's self-signed certificate for 127.0.0.1
// with a P-256 key, marked `CA:TRUE` when certificateAuthority, as
// `openssl req -x509` makes by default and as the lab does.
func selfSignedCertificate(t *testing.T, certificateAuthority bool) (tls.Certificate, []byte) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	check(t, err)
	template := &x509.Certificate{
		SerialNumber: big.NewInt(1),
		Subject:      pkix.Name{CommonName: "rcgen self signed cert"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(24 * time.Hour),
		IPAddresses:  []net.IP{net.ParseIP("127.0.0.1")},
	}
	if certificateAuthority {
		template.IsCA = true
		template.BasicConstraintsValid = true
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	check(t, err)
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	check(t, err)
	certificatePEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	keyPEM := pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER})
	pair, err := tls.X509KeyPair(certificatePEM, keyPEM)
	check(t, err)
	return pair, certificatePEM
}

// serveHTTPS serves the API's unauthenticated probe over HTTPS with a
// generated self-signed certificate; it returns the URL, the certificate's
// pin, the listener's address and what the server saw.
func serveHTTPS(t *testing.T, authority string, certificateAuthority bool) (string, string, netip.AddrPort, *seenRequests) {
	t.Helper()
	certificate, certificatePEM := selfSignedCertificate(t, certificateAuthority)
	pin, err := server.ServedCertificateSha256(certificatePEM)
	check(t, err)
	seen := &seenRequests{}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/v1/sessions", func(w http.ResponseWriter, r *http.Request) {
		authorization := ""
		if values := r.Header.Values("Authorization"); len(values) > 0 {
			authorization = values[0]
		}
		seen.push("host=" + r.Host + ";authorization=" + authorization)
		if authorization == "" {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		writeTestJSON(t, w, http.StatusOK, map[string]any{"sessions": []any{sampleSession("session-1")}})
	})
	address := "127.0.0.1:0"
	if strings.HasPrefix(authority, "[") {
		address = "[::1]:0"
	}
	listener, err := net.Listen("tcp", address)
	check(t, err)
	listening := httptest.NewUnstartedServer(withVersion(pointer(server.APIVersion), mux))
	listening.Listener.Close()
	listening.Listener = listener
	listening.TLS = &tls.Config{Certificates: []tls.Certificate{certificate}}
	listening.StartTLS()
	t.Cleanup(listening.Close)
	local := netip.MustParseAddrPort(listener.Addr().String())
	return fmt.Sprintf("https://%s:%d", authority, local.Port()), pin, local, seen
}

// Hard-won: 374b1915: the CLI rejected the viewer certificate that its daemon publishes and pins.
func TestTheCliReachesAViewerServingASelfSignedCaCertificateByItsPin(t *testing.T) {
	url, pin, _, _ := serveHTTPS(t, "127.0.0.1", true)
	plain, err := httpClient(nil, nil)
	check(t, err)
	err = probeAPI(t.Context(), plain, url)
	// The platform verifier words the rejection per platform: rustls says
	// "invalid peer certificate", crypto/tls says it failed to verify the
	// certificate.
	if err == nil || !strings.Contains(err.Error(), "failed to verify certificate") {
		t.Fatalf("unexpected error: %v", err)
	}
	pinned, err := httpClient(&pin, nil)
	check(t, err)
	check(t, probeAPI(t.Context(), pinned, url))
}

func TestTheCliReachesAViewerServingASelfSignedLeafCertificateByItsPin(t *testing.T) {
	url, pin, _, _ := serveHTTPS(t, "127.0.0.1", false)
	pinned, err := httpClient(&pin, nil)
	check(t, err)
	check(t, probeAPI(t.Context(), pinned, url))
}

func TestTheCliRefusesACertificateOtherThanThePinnedOne(t *testing.T) {
	url, _, _, _ := serveHTTPS(t, "127.0.0.1", true)
	other := strings.Repeat("0", 64)
	pinned, err := httpClient(&other, nil)
	check(t, err)
	if err := probeAPI(t.Context(), pinned, url); err == nil {
		t.Fatal("the probe accepted a certificate other than the pinned one")
	}
}

// Hard-won: #1440: CLI API requests used public DNS for an advertised Tailscale hostname instead of the local daemon listener.
func TestPinnedApiRequestsRouteAnUnresolvableAuthorityToTheLocalSocket(t *testing.T) {
	const authority = "viewer-does-not-resolve.invalid"
	viewerURL, pin, address, seen := serveHTTPS(t, authority, false)
	routedURL, resolver, err := routeViewerToLocalSocket(viewerURL, address)
	check(t, err)
	routed, err := url.Parse(routedURL)
	check(t, err)
	if routed.Hostname() != authority {
		t.Fatalf("routed host = %q", routed.Hostname())
	}
	if routed.Port() != fmt.Sprint(address.Port()) {
		t.Fatalf("routed port = %q", routed.Port())
	}
	proxyListener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	proxyURL, err := url.Parse("http://" + proxyListener.Addr().String())
	check(t, err)
	var proxyWasUsed atomic.Bool
	proxyDone := make(chan struct{})
	go func() {
		defer close(proxyDone)
		accepted := make(chan struct{})
		go func() {
			if connection, err := proxyListener.Accept(); err == nil {
				connection.Close()
				close(accepted)
			}
		}()
		select {
		case <-accepted:
			proxyWasUsed.Store(true)
		case <-time.After(300 * time.Millisecond):
		}
		proxyListener.Close()
	}()
	routedHTTP, err := buildHTTPClient(&pin, resolver, http.ProxyURL(proxyURL))
	check(t, err)
	check(t, probeAPI(t.Context(), routedHTTP, routedURL))
	client := apiClientWithHTTP(routedURL, "secret-token", routedHTTP)
	response, err := client.sessionsInWorkspace(t.Context(), nil, true)
	check(t, err)
	if response.Sessions[0].ID != "session-1" {
		t.Fatalf("session = %q", response.Sessions[0].ID)
	}
	<-proxyDone
	if proxyWasUsed.Load() {
		t.Fatal("the API request went through the proxy")
	}
	if got := seen.snapshot(); !slices.Equal(got, []string{
		fmt.Sprintf("host=%s:%d;authorization=", authority, address.Port()),
		fmt.Sprintf("host=%s:%d;authorization=Bearer secret-token", authority, address.Port()),
	}) {
		t.Fatalf("seen = %q", got)
	}

	wrongPin := strings.Repeat("0", 64)
	wrongHTTP, err := httpClient(&wrongPin, resolver)
	check(t, err)
	if err := probeAPI(t.Context(), wrongHTTP, routedURL); err == nil {
		t.Fatal("a wrong pin was accepted")
	}
}

func TestPinnedApiRequestReachesAnIpv6ListenerAdvertisedAsWildcard(t *testing.T) {
	probe, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Skip("this host cannot listen on the IPv6 loopback, as the Rust test returns early")
	}
	probe.Close()
	viewerURL, pin, address, _ := serveHTTPS(t, "[::]", false)
	routedURL, resolver, err := routeViewerToLocalSocket(viewerURL, address)
	check(t, err)
	if !strings.HasPrefix(routedURL, "https://[::1]:") {
		t.Fatalf("routed URL = %q", routedURL)
	}
	if resolver != nil {
		t.Fatalf("resolver = %+v, want none", *resolver)
	}
	pinned, err := httpClient(&pin, resolver)
	check(t, err)
	check(t, probeAPI(t.Context(), pinned, routedURL))
	specificURL, resolver, err := routeViewerToLocalSocket(fmt.Sprintf("https://[::1]:%d", address.Port()), address)
	check(t, err)
	if resolver != nil {
		t.Fatalf("resolver = %+v, want none", *resolver)
	}
	pinned, err = httpClient(&pin, resolver)
	check(t, err)
	check(t, probeAPI(t.Context(), pinned, specificURL))
	if _, _, err := routeViewerToLocalSocket(fmt.Sprintf("https://[::2]:%d", address.Port()), address); err == nil {
		t.Fatal("a viewer address other than the listener's was routed")
	}
}

func TestApiProbeDoesNotFollowRedirects(t *testing.T) {
	var visited atomic.Int64
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/v1/sessions", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Location", "/visited")
		w.WriteHeader(http.StatusFound)
	})
	mux.HandleFunc("GET /visited", func(w http.ResponseWriter, r *http.Request) {
		visited.Add(1)
		w.WriteHeader(http.StatusOK)
	})
	listening := httptest.NewServer(mux)
	defer listening.Close()

	plain, err := httpClient(nil, nil)
	check(t, err)
	if err := probeAPI(t.Context(), plain, listening.URL); err == nil {
		t.Fatal("the probe accepted a redirect")
	}
	if got := visited.Load(); got != 0 {
		t.Fatalf("visited = %d", got)
	}
}

func TestRecoveryControlsPreserveCommandIdsInHttpRequests(t *testing.T) {
	var requests struct {
		sync.Mutex
		bodies []any
	}
	handler := func(w http.ResponseWriter, r *http.Request) {
		var body any
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Errorf("decode request body: %v", err)
		}
		requests.Lock()
		requests.bodies = append(requests.bodies, body)
		requests.Unlock()
		w.Header().Set(server.APIVersionHeader, server.APIVersion)
		writeTestJSON(t, w, http.StatusOK, map[string]any{"turn_id": 17})
	}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/v1/sessions/s1/prompt", handler)
	mux.HandleFunc("POST /api/v1/sessions/s1/queued-prompts/clear", handler)
	listening := httptest.NewServer(mux)
	defer listening.Close()
	client, err := newAPIClient(listening.URL, "test")
	check(t, err)
	clear, err := client.promptWithID(t.Context(), "s1", "/clear", pointer("attempt-1-clear"))
	check(t, err)
	queue, err := client.clearQueue(t.Context(), "s1", pointer("attempt-1-queue"))
	check(t, err)
	if clear.TurnID != 17 || queue.TurnID != 17 {
		t.Fatalf("turn ids = %d, %d", clear.TurnID, queue.TurnID)
	}
	expected := []any{
		map[string]any{"text": "/clear", "command_id": "attempt-1-clear"},
		map[string]any{"command_id": "attempt-1-queue"},
	}
	requests.Lock()
	defer requests.Unlock()
	if !reflect.DeepEqual(requests.bodies, expected) {
		t.Fatalf("requests = %v, want %v", requests.bodies, expected)
	}
}
