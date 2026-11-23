//go:build linux

// Port of mj-microvm/src/mbx_proxy/tests.rs.

package microvm

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	microvmMbxTestNamespace = "example/project"
	microvmMbxTestOther     = "example/second"
)

var microvmMbxLarge = bytes.Repeat([]byte{0xa5}, 128*1024)

// microvmFakeS3 is the metadata service and an S3 bucket in one server.
type microvmFakeS3 struct {
	t       *testing.T
	mu      sync.Mutex
	objects map[string][]byte
	reads   []string
}

func (f *microvmFakeS3) ServeHTTP(w http.ResponseWriter, request *http.Request) {
	key := request.URL.EscapedPath()
	body, err := io.ReadAll(request.Body)
	if err != nil {
		f.t.Errorf("read the request body: %v", err)
	}
	switch {
	case request.Method == http.MethodPut && key == "/latest/api/token":
		io.WriteString(w, "fixture-token")
		return
	case request.Method == http.MethodGet && key == "/latest/meta-data/iam/security-credentials/":
		io.WriteString(w, "fixture-role")
		return
	case request.Method == http.MethodGet && key == "/latest/meta-data/iam/security-credentials/fixture-role":
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"Code":"Success","AccessKeyId":"fixture-id","SecretAccessKey":"fixture-secret","Token":"fixture-token","Expiration":"2099-01-01T00:00:00Z"}`)
		return
	case request.Method == http.MethodGet && key == "/latest/meta-data/placement/region":
		io.WriteString(w, "us-east-1")
		return
	}
	if !strings.HasPrefix(request.Header.Get("Authorization"), "AWS4-HMAC-SHA256 ") {
		f.t.Errorf("unsigned S3 request %s %s", request.Method, key)
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	switch request.Method {
	case http.MethodPut:
		f.objects[key] = body
		w.Header().Set("ETag", `"fixture-etag"`)
	case http.MethodGet, http.MethodHead:
		f.reads = append(f.reads, key)
		if strings.Contains(key, "/denied/") {
			w.WriteHeader(http.StatusForbidden)
			io.WriteString(w, "<Error><Code>AccessDenied</Code></Error>")
			return
		}
		object, ok := f.objects[key]
		if !ok {
			w.WriteHeader(http.StatusNotFound)
			io.WriteString(w, "<Error><Code>NoSuchKey</Code></Error>")
			return
		}
		w.Header().Set("ETag", `"fixture-etag"`)
		w.Write(object)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (f *microvmFakeS3) put(key string, object []byte) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.objects[key] = object
}

func (f *microvmFakeS3) remove(key string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	delete(f.objects, key)
}

func (f *microvmFakeS3) readCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.reads)
}

func microvmMbxServe(t *testing.T, handler http.Handler) string {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	return server.URL
}

func microvmMbxUpstream(t *testing.T) (*microvmFakeS3, string) {
	t.Helper()
	fake := &microvmFakeS3{t: t, objects: map[string][]byte{}}
	return fake, microvmMbxServe(t, fake)
}

func microvmMbxTestProxy(t *testing.T, endpoint string, prefix string, namespaces []string, cache string, budget *uint64) *microvmMbxProxy {
	t.Helper()
	endpointURL, err := microvmParseAbsoluteURL(endpoint)
	if err != nil {
		t.Fatal(err)
	}
	region := "us-east-1"
	remote, err := microvmMbxRemoteFrom("bucket", prefix, &region, append([]string{}, namespaces...), budget)
	if err != nil {
		t.Fatal(err)
	}
	token := "fixture-token"
	store, err := microvmNewMbxS3(remote, "us-east-1", microvmNewInstanceRoleCredentials(endpointURL), microvmTemporaryCredentials{
		Credentials: microvmS3Credentials{AccessKeyID: "fixture-id", SecretAccessKey: "fixture-secret", SessionToken: &token},
		ExpiresAt:   time.Now().Add(time.Hour),
	}, endpointURL)
	if err != nil {
		t.Fatal(err)
	}
	proxy := microvmNewMbxProxy(remote, cache)
	proxy.store = store
	return proxy
}

func microvmMbxBlobPath(digest microvmMbxDigest) string {
	return fmt.Sprintf("/v1/blobs/%s/%s/%d", digest.Algorithm, digest.Hash, digest.Size)
}

// microvmMbxClient is an HTTP client that never goes through a proxy.
var microvmMbxClient = &http.Client{Transport: &http.Transport{Proxy: nil}}

// microvmMbxRequest sends a request and returns its status and body.
func microvmMbxRequest(t *testing.T, method string, target string, namespace string, body []byte) (int, []byte) {
	t.Helper()
	request, err := http.NewRequest(method, target, bytes.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	if namespace != "" {
		request.Header.Set(microvmMbxNamespaceHeader, namespace)
	}
	response, err := microvmMbxClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	data, err := io.ReadAll(response.Body)
	if err != nil {
		t.Fatal(err)
	}
	return response.StatusCode, data
}

// Hard-won: #1495: publishing can exceed the host cache budget between maintenance runs.
func TestMbxProxyTrimsAfterPublishingOverBudget(t *testing.T) {
	fake, endpoint := microvmMbxUpstream(t)
	cache := t.TempDir()
	budget := uint64(len(microvmMbxLarge))
	proxy := microvmMbxTestProxy(t, endpoint, "cache", []string{microvmMbxTestNamespace}, cache, &budget)
	url := microvmMbxServe(t, proxy)
	for _, object := range [][]byte{microvmMbxLarge, bytes.Repeat([]byte{0x9b}, len(microvmMbxLarge))} {
		path := microvmMbxBlobPath(microvmMbxDigestBlake3(object))
		fake.put("/bucket/cache/"+microvmMbxTestNamespace+path, object)
		status, body := microvmMbxRequest(t, http.MethodGet, url+path, microvmMbxTestNamespace, nil)
		if status != http.StatusOK || !bytes.Equal(body, object) {
			t.Fatalf("status %d, %d bytes", status, len(body))
		}
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		proxy.trimMu.Lock()
		running := proxy.trimProgress.running
		proxy.trimMu.Unlock()
		if !running {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("cache trim finished")
		}
		time.Sleep(time.Millisecond)
	}
	entries, err := os.ReadDir(cache)
	if err != nil {
		t.Fatal(err)
	}
	var retained uint64
	for _, entry := range entries {
		info, err := entry.Info()
		if err != nil {
			t.Fatal(err)
		}
		retained += uint64(info.Size())
	}
	if retained != budget {
		t.Fatalf("retained %d bytes, budget %d", retained, budget)
	}
}

const microvmMbxCheckFixture = "MJ_TEST_MBX_CHECK_FIXTURE"

func TestMbxProxyProtocolBoundary(t *testing.T) {
	if os.Getenv(microvmMbxCheckFixture) != "" {
		status, err := MicrovmCLIMain([]string{
			"mbx-check",
			"--bucket",
			"bucket",
			"--prefix",
			"denied",
			"--namespace",
			microvmMbxTestNamespace,
		})
		if err != nil {
			// Rust unwraps the result: a panic, not the check's status.
			fmt.Fprintln(os.Stderr, err)
			os.Exit(101)
		}
		os.Exit(status)
	}

	fake, endpoint := microvmMbxUpstream(t)
	cache := t.TempDir()
	proxy := microvmMbxTestProxy(t, endpoint, "cache", []string{microvmMbxTestNamespace, microvmMbxTestOther}, cache, nil)
	denied := microvmMbxTestProxy(t, endpoint, "denied", []string{microvmMbxTestNamespace}, cache, nil)
	command := exec.Command(os.Args[0], "-test.run=^TestMbxProxyProtocolBoundary$", "-test.count=1")
	command.Env = append(os.Environ(),
		microvmMbxCheckFixture+"=1",
		"AWS_EC2_METADATA_DISABLED=false",
		"AWS_EC2_METADATA_SERVICE_ENDPOINT="+endpoint,
		"MBX_REMOTE_S3_ENDPOINT="+endpoint,
	)
	check, err := core.RunWithInput(command, nil)
	if err != nil {
		t.Fatal(err)
	}
	if code := check.Status.ExitCode(); code != 1 {
		t.Fatalf("mbx-check exited %d: %s%s", code, check.Stdout, check.Stderr)
	}
	if !slices.Contains(microvmLines(string(check.Stdout)),
		"role cannot read bucket/denied: AccessDenied (the role needs s3:GetObject on the prefix and s3:ListBucket on the bucket for the prefix)") {
		t.Fatalf("mbx-check printed:\n%s", check.Stdout)
	}
	deniedURL := microvmMbxServe(t, denied)
	url := microvmMbxServe(t, proxy)
	digest := microvmMbxDigestBlake3(microvmMbxLarge)
	path := microvmMbxBlobPath(digest)
	key := "/bucket/cache/" + microvmMbxTestNamespace + path

	for _, method := range []string{http.MethodPut, http.MethodPost} {
		if status, _ := microvmMbxRequest(t, method, url+path, "", microvmMbxLarge); status != http.StatusMethodNotAllowed {
			t.Fatalf("%s answered %d", method, status)
		}
	}
	if status, _ := microvmMbxRequest(t, http.MethodGet, url+"/unknown", "", nil); status != http.StatusMethodNotAllowed {
		t.Fatalf("an unknown path answered %d", status)
	}
	if status, _ := microvmMbxRequest(t, http.MethodGet, url+path, "", nil); status != http.StatusForbidden {
		t.Fatalf("no namespace answered %d", status)
	}
	if status, _ := microvmMbxRequest(t, http.MethodGet, url+path, "foreign/repository", nil); status != http.StatusForbidden {
		t.Fatalf("a foreign namespace answered %d", status)
	}
	if status, _ := microvmMbxRequest(t, http.MethodGet, url+path, "../project", nil); status != http.StatusForbidden {
		t.Fatalf("a dot-dot namespace answered %d", status)
	}
	if reads := fake.readCount(); reads != 1 {
		t.Fatalf("only Doctor should have reached S3: %d reads", reads)
	}
	status, body := microvmMbxRequest(t, http.MethodGet, url+"/v1/capabilities", microvmMbxTestNamespace, nil)
	if status != http.StatusOK {
		t.Fatalf("capabilities answered %d", status)
	}
	var caps microvmMbxCapabilities
	if err := json.Unmarshal(body, &caps); err != nil {
		t.Fatal(err)
	}
	if caps.Protocol.Major != microvmMbxProtocolVersion || caps.Features != (microvmMbxCapabilityFeatures{}) || len(caps.Compressors) != 0 {
		t.Fatalf("capabilities %s", body)
	}
	status, body = microvmMbxRequest(t, http.MethodGet, deniedURL+path, microvmMbxTestNamespace, nil)
	if status != http.StatusBadGateway || !strings.Contains(string(body), "AccessDenied") {
		t.Fatalf("denied read answered %d: %s", status, body)
	}
	if status, _ := microvmMbxRequest(t, http.MethodGet, url+path, microvmMbxTestNamespace, nil); status != http.StatusNotFound {
		t.Fatalf("an absent object answered %d", status)
	}

	fake.put(key, bytes.Repeat([]byte{0x5a}, len(microvmMbxLarge)))
	status, body = microvmMbxRequest(t, http.MethodGet, url+path, microvmMbxTestNamespace, nil)
	if status != http.StatusBadGateway || !strings.Contains(string(body), "failed digest verification") {
		t.Fatalf("a mismatched blob answered %d: %s", status, body)
	}
	if entries, err := os.ReadDir(cache); err != nil || len(entries) != 0 {
		t.Fatalf("cache holds %d entries: %v", len(entries), err)
	}

	fake.put(key, microvmMbxLarge)
	status, body = microvmMbxRequest(t, http.MethodGet, url+path, microvmMbxTestNamespace, nil)
	if status != http.StatusOK || !bytes.Equal(body, microvmMbxLarge) {
		t.Fatalf("status %d, %d bytes", status, len(body))
	}
	fake.remove(key)
	if status, _ := microvmMbxRequest(t, http.MethodGet, url+path, microvmMbxTestOther, nil); status != http.StatusNotFound {
		t.Fatalf("same digest in a different namespace answered %d", status)
	}
	status, body = microvmMbxRequest(t, http.MethodGet, url+path, microvmMbxTestNamespace, nil)
	if status != http.StatusOK || !bytes.Equal(body, microvmMbxLarge) {
		t.Fatalf("the cached blob: status %d, %d bytes", status, len(body))
	}
}

func microvmMbxExecute(t *testing.T, command *exec.Cmd) core.ProcessOutput {
	t.Helper()
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

func microvmMbxChecked(t *testing.T, command *exec.Cmd) string {
	t.Helper()
	output := microvmMbxExecute(t, command)
	if !output.Status.Success() {
		t.Fatalf("%v: %s\n%s", command.Args, output.Stdout, output.Stderr)
	}
	return microvmLossy(output.Stdout)
}

// Rust ignores this test by default: it needs root with network and mount
// namespace privileges. Run it with MJ_TEST_MBX_DEVICE_BINDING=1 under
// `sudo -E`.
func TestMbxProxyDeviceBinding(t *testing.T) {
	const child = "MJ_TEST_MBX_BINDING_NETNS"
	if os.Getenv(child) == "" {
		if os.Getenv("MJ_TEST_MBX_DEVICE_BINDING") == "" {
			t.Skip("needs root with network and mount namespace privileges; set MJ_TEST_MBX_DEVICE_BINDING=1 and run with sudo -E")
		}
		if !MicrovmIsRoot() {
			t.Fatal("run this test with sudo -E")
		}
		command := exec.Command("unshare", "--net", "--mount", "--", os.Args[0], "-test.run=^TestMbxProxyDeviceBinding$", "-test.count=1", "-test.v")
		command.Env = append(os.Environ(), child+"=1")
		microvmMbxChecked(t, command)
		return
	}

	id := os.Getpid()
	links := []string{fmt.Sprintf("mbx-a-%d", id), fmt.Sprintf("mbx-b-%d", id)}
	defer func() {
		for _, name := range links {
			output, err := core.RunWithInput(exec.Command("ip", "netns", "delete", name), nil)
			if err != nil || !output.Status.Success() {
				fmt.Fprintf(os.Stderr, "delete test network namespace %s: %s%v\n", name, output.Stderr, err)
			}
		}
	}()
	microvmMbxChecked(t, exec.Command("ip", "link", "set", "lo", "up"))
	for index, ns := range links {
		dev := fmt.Sprintf("mjt%d", index)
		host := 1 + 4*index
		guest := host + 1
		for _, args := range [][]string{
			{"netns", "add", ns},
			{"link", "add", dev, "type", "veth", "peer", "name", "guest", "netns", ns},
			{"addr", "add", fmt.Sprintf("198.18.0.%d/30", host), "dev", dev},
			{"link", "set", dev, "up"},
			{"-n", ns, "addr", "add", fmt.Sprintf("198.18.0.%d/30", guest), "dev", "guest"},
			{"-n", ns, "link", "set", "guest", "up"},
			{"-n", ns, "route", "add", "default", "via", fmt.Sprintf("198.18.0.%d", host)},
		} {
			microvmMbxChecked(t, exec.Command("ip", args...))
		}
	}

	cache := t.TempDir()
	fake, endpoint := microvmMbxUpstream(t)
	path := microvmMbxBlobPath(microvmMbxDigestBlake3(microvmMbxLarge))
	for index, namespace := range []string{microvmMbxTestNamespace, microvmMbxTestOther} {
		fake.put("/bucket/cache/"+namespace+path, microvmMbxLarge)
		listener, err := microvmMbxBind(fmt.Sprintf("mjt%d", index),
			netip.AddrPortFrom(netip.AddrFrom4([4]byte{198, 18, 0, byte(1 + 4*index)}), microvmMbxProxyPort))
		if err != nil {
			t.Fatal(err)
		}
		app := microvmMbxTestProxy(t, endpoint, "cache", []string{namespace}, cache, nil)
		server := &http.Server{Handler: app}
		t.Cleanup(func() { server.Close() })
		go server.Serve(listener)
	}
	fetch := func(netns string, host int, namespace string, source string) core.ProcessOutput {
		args := []string{
			"netns", "exec", netns, "curl", "-fsS", "--max-time", "5", "--noproxy", "*",
			"-H", fmt.Sprintf("%s: %s", microvmMbxNamespaceHeader, namespace),
			fmt.Sprintf("http://198.18.0.%d:%d%s", host, microvmMbxProxyPort, path),
		}
		if source != "" {
			args = append(args, "--interface", source)
		}
		return microvmMbxExecute(t, exec.Command("ip", args...))
	}
	if output := fetch(links[0], 1, microvmMbxTestNamespace, ""); !bytes.Equal(output.Stdout, microvmMbxLarge) {
		t.Fatalf("A's own proxy: %s", output.Stderr)
	}
	if output := fetch(links[1], 5, microvmMbxTestOther, ""); !bytes.Equal(output.Stdout, microvmMbxLarge) {
		t.Fatalf("B's own proxy: %s", output.Stderr)
	}
	if denied := fetch(links[0], 5, microvmMbxTestOther, ""); denied.Status.ExitCode() != 7 {
		t.Fatalf("A reached B's proxy: %s", denied.Stderr)
	}
	// Add B's source to A, then arrange the return route on the isolated
	// host so a real TCP handshake can complete despite the spoofed source.
	microvmMbxChecked(t, exec.Command("ip", "-n", links[0], "addr", "add", "198.18.0.6/32", "dev", "guest"))
	microvmMbxChecked(t, exec.Command("ip", "route", "add", "198.18.0.6/32", "dev", "mjt0"))
	if output := fetch(links[0], 1, microvmMbxTestNamespace, "198.18.0.6"); !bytes.Equal(output.Stdout, microvmMbxLarge) {
		t.Fatalf("A's own proxy from a spoofed source: %s", output.Stderr)
	}
	if spoofed := fetch(links[0], 5, microvmMbxTestOther, "198.18.0.6"); spoofed.Status.ExitCode() != 7 {
		t.Fatalf("a spoofed source reached B's proxy: %s", spoofed.Stderr)
	}
}

// Rust ignores this test by default: it needs the pinned mbx client. Run it
// with MJ_TEST_MBX_BIN=/absolute/path/to/mbx, the release in
// core.MbxVersion (the same artifact Mjolnir stages for hosts).
func TestMbxProxyRealClient(t *testing.T) {
	binary, ok := os.LookupEnv("MJ_TEST_MBX_BIN")
	if !ok {
		t.Skip("requires the pinned mbx client; set MJ_TEST_MBX_BIN")
	}
	if version := strings.TrimSpace(microvmMbxChecked(t, exec.Command(binary, "--version"))); version != "mbx "+core.MbxVersion {
		t.Fatalf("mbx version %q", version)
	}

	fake, endpoint := microvmMbxUpstream(t)
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	fixture, err := os.MkdirTemp(cwd, ".tmp")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(fixture) })
	workspace := filepath.Join(fixture, "project")
	microvmMustMkdirAll(t, filepath.Join(workspace, "src"))
	microvmMustWrite(t, filepath.Join(workspace, "Cargo.toml"),
		[]byte("[package]\nname = \"mbx_proxy_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n"))
	// CI seeds a checked-in lockfile. Generate its fixture form before the
	// first build so both runs compute the same task identity.
	microvmMustWrite(t, filepath.Join(workspace, "Cargo.lock"), []byte("# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"mbx_proxy_probe\"\nversion = \"0.1.0\"\n"))
	microvmMustWrite(t, filepath.Join(workspace, "src/main.rs"), []byte("fn main() { std::io::Write::write_all(&mut std::io::stdout(), include_bytes!(\"fixture.bin\")).unwrap(); }\n"))
	microvmMustWrite(t, filepath.Join(workspace, "src/fixture.bin"), microvmMbxLarge)
	microvmMustMkdirAll(t, filepath.Join(fixture, "proxy"))
	proxy := microvmMbxServe(t, microvmMbxTestProxy(t, endpoint, "cache", []string{microvmMbxTestNamespace}, filepath.Join(fixture, "proxy"), nil))

	build := func(phase string, remote string, mode string) string {
		command := exec.Command(binary, "build", "--offline")
		command.Dir = workspace
		var environment []string
		for _, entry := range os.Environ() {
			name, _, _ := strings.Cut(entry, "=")
			switch {
			case strings.HasPrefix(name, "MBX_"), strings.HasPrefix(name, "AWS_"),
				name == "CARGO_TARGET_DIR", name == "RUSTC_WRAPPER", name == "RUSTC_WORKSPACE_WRAPPER":
				continue
			}
			environment = append(environment, entry)
		}
		environment = append(environment,
			"XDG_CONFIG_HOME="+filepath.Join(fixture, "config"),
			"MBX_CACHE_DIR="+filepath.Join(fixture, phase+"-cache"),
			"MBX_TARGET_ROOT="+filepath.Join(fixture, "targets"),
			"MBX_REMOTE_URL="+remote,
			"MBX_REMOTE_NAMESPACE="+microvmMbxTestNamespace,
			"MBX_REMOTE_MODE="+mode,
			"CI=true",
		)
		if phase == "seed" {
			environment = append(environment,
				"MBX_REMOTE_TRUST_WRITES=true",
				"MBX_REMOTE_S3_ENDPOINT="+endpoint,
				"MBX_REMOTE_S3_REGION=us-east-1",
				"AWS_ACCESS_KEY_ID=fixture-id",
				"AWS_SECRET_ACCESS_KEY=fixture-secret",
			)
		}
		command.Env = environment
		output := microvmMbxExecute(t, command)
		log := microvmLossy(output.Stdout) + microvmLossy(output.Stderr)
		if !output.Status.Success() {
			t.Fatalf("%s: %s", phase, log)
		}
		return log
	}

	seedLog := build("seed", "s3://bucket/cache", "read-write")
	fake.mu.Lock()
	objectCount := len(fake.objects)
	largeBlob := false
	for path, object := range fake.objects {
		largeBlob = largeBlob || strings.Contains(path, "/blobs/") && len(object) > 65536
	}
	fake.mu.Unlock()
	if objectCount == 0 || !largeBlob {
		t.Fatalf("seed stored %d objects, a large blob: %v\n%s", objectCount, largeBlob, seedLog)
	}
	// The new target directory must start cold as well as the new CAS.
	target := filepath.Join(workspace, "target")
	if info, err := os.Lstat(target); err != nil {
		t.Fatal(err)
	} else if info.Mode()&os.ModeSymlink != 0 {
		err = os.Remove(target)
		if err != nil {
			t.Fatal(err)
		}
	} else if err := os.RemoveAll(target); err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(filepath.Join(fixture, "targets")); err != nil {
		t.Fatal(err)
	}
	fake.mu.Lock()
	fake.reads = nil
	fake.mu.Unlock()
	readLog := build("read", proxy, "read-only")
	fmt.Fprintln(os.Stderr, readLog)
	fake.mu.Lock()
	defer fake.mu.Unlock()
	actionRead, largeRead := false, false
	for _, path := range fake.reads {
		actionRead = actionRead || strings.Contains(path, "/action-results/")
		largeRead = largeRead || strings.Contains(path, "/blobs/") && len(fake.objects[path]) > 65536
	}
	if !actionRead {
		t.Fatalf("no action result read through the proxy\n%s", readLog)
	}
	if !largeRead {
		keys := make([]string, 0, len(fake.objects))
		for key := range fake.objects {
			keys = append(keys, key)
		}
		t.Fatalf("%s\nseed: %s\nS3 reads: %q\nS3 objects: %q", readLog, seedLog, fake.reads, keys)
	}
	if len(fake.objects) != objectCount {
		t.Fatalf("the read-only build stored objects: %d, then %d", objectCount, len(fake.objects))
	}
	hit := false
	for _, line := range microvmLines(readLog) {
		hit = hit || strings.HasPrefix(line, "mbx[cache]: object cache: 1 hits, 0 misses, 1 prefetched;") && strings.Contains(line, "0 B uploaded")
	}
	if !hit {
		t.Fatalf("no cache hit reported\n%s", readLog)
	}
}
