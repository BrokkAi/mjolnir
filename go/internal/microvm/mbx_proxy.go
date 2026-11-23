//go:build linux

// Port of mj-microvm/src/mbx_proxy.rs.
//
// Read-only, TAP-bound mbx cache for one VM. All authorization comes from
// launch arguments, before the proxy opens or fetches any object.

package microvm

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	microvmMbxProxyPort       = 7461
	microvmMbxProxyUser       = "mj-mbx-proxy"
	microvmMbxProxyUnitEngine = "/run/mj-mbx-proxy/engine"
	microvmMbxProxyUnitCache  = "/run/mj-mbx-proxy/cache"
	microvmMbxStagingPrefix   = ".mbx-stage-"
	// microvmMbxMaxJSONBytes is the same JSON response limit as
	// mbx-cache-core's S3 transport.
	microvmMbxMaxJSONBytes = 16 * 1024 * 1024
)

// microvmMbxProxyArgs is `microvm mbx-proxy`: serve the read-only cache for
// one VM.
type microvmMbxProxyArgs struct {
	device      string
	listen      netip.AddrPort
	bucket      string
	prefix      string
	region      *string
	namespaces  []string
	cacheDir    string
	cacheBudget *uint64
}

// microvmMbxCheckArgs is `microvm mbx-check`: probe the configured S3 remote
// with the host's instance role.
type microvmMbxCheckArgs struct {
	bucket    string
	prefix    string
	region    *string
	namespace string
}

// microvmMbxParseRemote reads `--mbx-remote s3://bucket/prefix` and the
// settings that go with it.
func microvmMbxParseRemote(remoteURL string, region *string, namespaces []string, budget *uint64) (MicrovmMbxRemote, error) {
	location, ok := strings.CutPrefix(remoteURL, "s3://")
	if !ok {
		return MicrovmMbxRemote{}, errors.New("--mbx-remote must start with s3://")
	}
	bucket, prefix, _ := strings.Cut(location, "/")
	return microvmMbxRemoteFrom(bucket, prefix, region, namespaces, budget)
}

func microvmMbxRemoteFrom(bucket string, prefix string, region *string, namespaces []string, budget *uint64) (MicrovmMbxRemote, error) {
	if !core.ValidMbxS3Bucket(bucket) {
		return MicrovmMbxRemote{}, fmt.Errorf("invalid S3 bucket %s", microvmDebugString(bucket))
	}
	prefix = strings.Trim(prefix, "/")
	dotSegment := false
	for _, segment := range strings.Split(prefix, "/") {
		dotSegment = dotSegment || segment == "." || segment == ".."
	}
	if dotSegment || strings.Contains(prefix, "%") {
		return MicrovmMbxRemote{}, errors.New("S3 prefix cannot contain standalone '.' or '..' segments or percent escapes")
	}
	if len(namespaces) == 0 {
		return MicrovmMbxRemote{}, errors.New("at least one mbx namespace is required")
	}
	for _, namespace := range namespaces {
		if !microvmMbxValidNamespace(namespace) {
			return MicrovmMbxRemote{}, fmt.Errorf("invalid mbx namespace %s", microvmDebugString(namespace))
		}
	}
	return MicrovmMbxRemote{Bucket: bucket, Prefix: prefix, Region: region, Budget: budget, Namespaces: namespaces}, nil
}

func microvmMbxValidNamespace(namespace string) bool {
	for _, segment := range strings.Split(namespace, "/") {
		if segment == "" || segment == "." || segment == ".." {
			return false
		}
		for i := 0; i < len(segment); i++ {
			if b := segment[i]; !microvmIsASCIIAlphanumeric(b) && b != '.' && b != '_' && b != '-' {
				return false
			}
		}
	}
	return true
}

// microvmMbxProxyStartUnit starts the VM's proxy as a systemd unit bound to
// the VM's unit.
func microvmMbxProxyStartUnit(layout MicrovmLayout, state MicrovmVmState, link MicrovmLink, boot *MicrovmBootAssets) error {
	remote := state.MbxRemote
	if remote == nil {
		return errors.New("missing proxy configuration")
	}
	cache := layout.MbxCache()
	bootDir := filepath.Join(layout.Boot(), boot.Tag)
	engine := filepath.Join(bootDir, "mbx-proxy-engine")
	// Different VMs may launch concurrently. Serialize only account and
	// directory setup, releasing the lock before systemd waits for IMDS.
	if err := func() error {
		global, err := layout.LockGlobal()
		if err != nil {
			return err
		}
		defer global.Drop()
		getent, err := MicrovmFindTool("getent")
		if err != nil {
			return err
		}
		passwd, err := MicrovmRunUnchecked(getent, []string{"passwd", microvmMbxProxyUser})
		if err != nil {
			return err
		}
		switch passwd.Status.ExitCode() {
		case 0:
		case 2:
			if _, err := MicrovmRunTool("useradd", []string{
				"--system",
				"--user-group",
				"--no-create-home",
				"--shell",
				"/sbin/nologin",
				microvmMbxProxyUser,
			}); err != nil {
				return fmt.Errorf("create the mbx proxy system user: %w", err)
			}
		default:
			return fmt.Errorf("look up %s: %s", microvmMbxProxyUser, microvmLossy(passwd.Stderr))
		}
		if !utf8.ValidString(cache) {
			return errors.New("cache path is not UTF-8")
		}
		if _, err := MicrovmRunTool("install", []string{
			"-d",
			"-m",
			"0700",
			"-o",
			microvmMbxProxyUser,
			"-g",
			microvmMbxProxyUser,
			cache,
		}); err != nil {
			return fmt.Errorf("prepare the mbx cache directory: %w", err)
		}
		// systemd can hide /home before resolving bind mount sources. Stage
		// the actual running engine next to its boot assets, whose
		// reservation the caller holds until systemd has mounted the
		// executable.
		if !microvmIsFile(engine) {
			if err := microvmMbxStageEngine(bootDir, engine); err != nil {
				return err
			}
		}
		return nil
	}(); err != nil {
		return err
	}

	vmUnit := MicrovmUnitName(state.Name)
	unit := fmt.Sprintf("mj-mbx-proxy-%s.service", state.Name)
	args := []string{
		"--quiet",
		"--collect",
		"--unit=" + unit,
		"--property=BindsTo=" + vmUnit,
		"--property=After=" + vmUnit,
		"--property=Type=notify",
		"--property=NotifyAccess=main",
		"--property=User=" + microvmMbxProxyUser,
		"--property=Group=" + microvmMbxProxyUser,
		"--property=CapabilityBoundingSet=CAP_NET_RAW",
		"--property=AmbientCapabilities=CAP_NET_RAW",
		"--property=NoNewPrivileges=yes",
		"--property=ProtectSystem=strict",
		"--property=ProtectHome=yes",
		"--property=PrivateTmp=yes",
		"--property=UMask=0077",
		// The engine and cache are below a root-only directory. Bind only
		// these two paths into the service, preserving the host permissions.
		fmt.Sprintf("--property=BindReadOnlyPaths=%s:%s", engine, microvmMbxProxyUnitEngine),
		fmt.Sprintf("--property=BindPaths=%s:%s", cache, microvmMbxProxyUnitCache),
		"--property=ReadWritePaths=" + microvmMbxProxyUnitCache,
		// systemd-run looks the executable up on the host before the unit's
		// bind mounts exist, so start a host shell that execs the engine
		// inside the unit (same PID, so Type=notify still sees it). The `$$`
		// keeps systemd from expanding the shell's parameters itself.
		"/bin/sh",
		"-c",
		`exec "$$0" "$$@"`,
		microvmMbxProxyUnitEngine,
		"microvm",
		"mbx-proxy",
		"--device",
		link.Tap,
		"--listen",
		netip.AddrPortFrom(link.Host, microvmMbxProxyPort).String(),
		"--bucket=" + remote.Bucket,
		// systemd expands dollar variables in the command unless escaped.
		"--prefix=" + strings.ReplaceAll(remote.Prefix, "$", "$$"),
		"--cache-dir=" + microvmMbxProxyUnitCache,
	}
	if remote.Region != nil {
		args = append(args, "--region="+strings.ReplaceAll(*remote.Region, "$", "$$"))
	}
	if remote.Budget != nil {
		args = append(args, "--cache-budget="+strconv.FormatUint(*remote.Budget, 10))
	}
	for _, namespace := range remote.Namespaces {
		args = append(args, "--namespace="+namespace)
	}
	if _, err := MicrovmRunTool("systemd-run", args); err != nil {
		return fmt.Errorf("start the mbx proxy unit:\n%s: %w", microvmUnitJournal(unit), err)
	}
	return nil
}

// microvmMbxStageEngine copies the running engine to engine, through a
// temporary file in bootDir.
func microvmMbxStageEngine(bootDir string, engine string) error {
	source, err := os.Open("/proc/self/exe")
	if err != nil {
		return fmt.Errorf("open the running mbx proxy engine: %w", microvmIOError(err))
	}
	defer source.Close()
	staged, err := os.CreateTemp(bootDir, ".tmp")
	if err != nil {
		return microvmIOError(err)
	}
	published := false
	defer func() {
		if !published {
			staged.Close()
			os.Remove(staged.Name())
		}
	}()
	if _, err := io.Copy(staged, source); err != nil {
		return fmt.Errorf("stage the mbx proxy engine: %w", microvmIOError(err))
	}
	if err := staged.Chmod(0o555); err != nil {
		return microvmIOError(err)
	}
	if err := os.Rename(staged.Name(), engine); err != nil {
		return fmt.Errorf("publish the mbx proxy engine: %w", microvmIOError(err))
	}
	published = true
	return staged.Close()
}

// microvmMbxTrimCache trims the host's mbx cache to budget (the automatic
// budget when nil) and returns the bytes freed.
func microvmMbxTrimCache(layout MicrovmLayout, budget *uint64) (uint64, error) {
	return microvmMbxTrim(layout.MbxCache(), budget)
}

// microvmMbxProxyRun serves the proxy until the listener fails.
func microvmMbxProxyRun(args microvmMbxProxyArgs) (int, error) {
	remote, err := microvmMbxRemoteFrom(args.bucket, args.prefix, args.region, args.namespaces, args.cacheBudget)
	if err != nil {
		return 0, err
	}
	// Readiness covers only the listener. Credentials are fetched on the
	// first request, so an IMDS hiccup or a missing role never keeps the VM
	// from starting; each request then fails with the reason instead.
	listener, err := microvmMbxBind(args.device, args.listen)
	if err != nil {
		return 0, err
	}
	proxy := microvmNewMbxProxy(remote, args.cacheDir)
	if err := microvmMbxNotifyReady(); err != nil {
		return 0, err
	}
	if err := http.Serve(listener, proxy); err != nil {
		return 0, fmt.Errorf("serve mbx proxy: %w", err)
	}
	return 0, nil
}

// microvmMbxCheck probes the remote and prints what it found; the status is
// 0 when the role can read it.
func microvmMbxCheck(args microvmMbxCheckArgs) (int, error) {
	remote, err := microvmMbxRemoteFrom(args.bucket, args.prefix, args.region, []string{args.namespace}, nil)
	if err != nil {
		return 0, err
	}
	message, code := microvmMbxCheckRemote(remote)
	fmt.Println(message)
	return code, nil
}

func microvmMbxCheckRemote(remote MicrovmMbxRemote) (string, int) {
	store, err := microvmMbxS3Connect(remote)
	switch {
	case err != nil:
	case store == nil:
		return "no instance role attached", 1
	default:
		err = microvmMbxCheckAccess(store, remote.Namespaces[0])
	}
	return microvmMbxCheckResult(remote, err)
}

func microvmMbxCheckAccess(store *microvmMbxS3, namespace string) error {
	route := fmt.Sprintf("/v1/action-results/blake3/%s/0", strings.Repeat("0", 64))
	response, err := store.get(namespace, route)
	if response != nil {
		response.Body.Close()
	}
	return err
}

func microvmMbxCheckResult(remote MicrovmMbxRemote, err error) (string, int) {
	if err == nil {
		return "remote cache readable", 0
	}
	hint := ""
	if microvmMbxAccessDenied(err) {
		hint = " (the role needs s3:GetObject on the prefix and s3:ListBucket on the bucket for the prefix)"
	}
	return fmt.Sprintf("role cannot read %s/%s: %s%s", remote.Bucket, remote.Prefix, microvmMbxDiagnostic(err), hint), 1
}

// microvmMbxBind listens on listen through device only, so a VM can reach
// only its own proxy.
func microvmMbxBind(device string, listen netip.AddrPort) (net.Listener, error) {
	fd, err := unix.Socket(unix.AF_INET, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, unix.IPPROTO_TCP)
	if err != nil {
		return nil, microvmIOError(err)
	}
	socket := os.NewFile(uintptr(fd), "mbx-proxy")
	defer socket.Close()
	if err := unix.SetsockoptString(fd, unix.SOL_SOCKET, unix.SO_BINDTODEVICE, device); err != nil {
		return nil, fmt.Errorf("bind mbx proxy to %s: %w", device, microvmIOError(err))
	}
	if err := unix.SetsockoptInt(fd, unix.SOL_SOCKET, unix.SO_REUSEADDR, 1); err != nil {
		return nil, microvmIOError(err)
	}
	if err := unix.Bind(fd, &unix.SockaddrInet4{Port: int(listen.Port()), Addr: listen.Addr().As4()}); err != nil {
		return nil, fmt.Errorf("listen on %s via %s: %w", listen, device, microvmIOError(err))
	}
	// Linux caps this to its configured somaxconn.
	if err := unix.Listen(fd, unix.SOMAXCONN); err != nil {
		return nil, microvmIOError(err)
	}
	listener, err := net.FileListener(socket)
	if err != nil {
		return nil, fmt.Errorf("register mbx proxy listener: %w", err)
	}
	return listener, nil
}

// microvmMbxNotifyReady tells systemd the proxy is listening.
func microvmMbxNotifyReady() error {
	name, ok := os.LookupEnv("NOTIFY_SOCKET")
	if !ok {
		return errors.New("missing systemd NOTIFY_SOCKET")
	}
	// A leading `@` names an abstract socket, which Go spells the same way.
	conn, err := net.DialUnix("unixgram", nil, &net.UnixAddr{Name: name, Net: "unixgram"})
	if err != nil {
		return fmt.Errorf("notify systemd that the mbx proxy is listening: %w", microvmIOError(err))
	}
	defer conn.Close()
	if _, err := conn.Write([]byte("READY=1")); err != nil {
		return fmt.Errorf("notify systemd that the mbx proxy is listening: %w", microvmIOError(err))
	}
	return nil
}

type microvmMbxProxy struct {
	remote MicrovmMbxRemote
	cache  string
	budget uint64

	trimMu       sync.Mutex
	trimProgress microvmMbxTrimProgress

	// storeMu guards store, connected on first use; a failed connection is
	// retried by the next request.
	storeMu sync.Mutex
	store   *microvmMbxS3
}

type microvmMbxTrimProgress struct {
	published uint64
	running   bool
}

type microvmMbxKind int

const (
	microvmMbxKindBlob microvmMbxKind = iota
	microvmMbxKindAction
	microvmMbxKindManifest
)

func (k microvmMbxKind) asStr() string {
	switch k {
	case microvmMbxKindBlob:
		return "blobs"
	case microvmMbxKindAction:
		return "action-results"
	}
	return "action-manifests"
}

func (k microvmMbxKind) mediaType() string {
	switch k {
	case microvmMbxKindBlob:
		return microvmMbxBlobMediaType
	case microvmMbxKindAction:
		return microvmMbxActionResultMediaType
	}
	return microvmMbxTaskActionManifestMedia
}

// microvmMbxObject is the object a request path names.
func microvmMbxObject(path string) (microvmMbxKind, microvmMbxDigest, bool) {
	rest, ok := strings.CutPrefix(path, "/v1/")
	if !ok {
		return 0, microvmMbxDigest{}, false
	}
	parts := strings.Split(rest, "/")
	if len(parts) != 4 {
		return 0, microvmMbxDigest{}, false
	}
	var kind microvmMbxKind
	switch parts[0] {
	case "blobs":
		kind = microvmMbxKindBlob
	case "action-results":
		kind = microvmMbxKindAction
	case "action-manifests":
		kind = microvmMbxKindManifest
	default:
		return 0, microvmMbxDigest{}, false
	}
	algorithm, hash, sizeText := parts[1], parts[2], parts[3]
	if kind != microvmMbxKindBlob && algorithm != "blake3" {
		return 0, microvmMbxDigest{}, false
	}
	for i := 0; i < len(sizeText); i++ {
		if sizeText[i] < '0' || sizeText[i] > '9' {
			return 0, microvmMbxDigest{}, false
		}
	}
	size, err := microvmParseUint(sizeText, 64)
	if err != nil {
		return 0, microvmMbxDigest{}, false
	}
	digest := microvmMbxDigest{Algorithm: algorithm, Hash: hash, Size: size}
	if digest.validate() != nil {
		return 0, microvmMbxDigest{}, false
	}
	return kind, digest, true
}

// microvmMbxPlainText writes a text response as axum writes a &str body.
func microvmMbxPlainText(w http.ResponseWriter, status int, text string) {
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.Header().Set("Content-Length", strconv.Itoa(len(text)))
	w.WriteHeader(status)
	io.WriteString(w, text)
}

func (p *microvmMbxProxy) ServeHTTP(w http.ResponseWriter, request *http.Request) {
	path := request.URL.EscapedPath()
	capabilities := path == "/v1/capabilities"
	kind, digest, isObject := microvmMbxObject(path)
	if request.Method != http.MethodGet || request.URL.RawQuery != "" || request.URL.ForceQuery || (!capabilities && !isObject) {
		w.Header().Set("Allow", "GET")
		microvmMbxPlainText(w, http.StatusMethodNotAllowed, "read-only mbx cache")
		return
	}
	namespace, ok := microvmMbxNamespace(request.Header, p.remote)
	if !ok {
		microvmMbxPlainText(w, http.StatusForbidden, "mbx namespace is not allowed")
		return
	}
	if !isObject {
		document := microvmMbxNewCapabilities(microvmMbxProtocolVersion, 0)
		document.DigestAlgorithms = []string{"blake3", "sha256"}
		body, err := microvmMarshalJSON(document)
		if err != nil {
			panic(fmt.Sprintf("encode the mbx capabilities: %v", err))
		}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Content-Length", strconv.Itoa(len(body)))
		w.Write(body)
		return
	}
	response, err := p.read(namespace, kind, digest)
	switch {
	case err != nil:
		message := microvmMbxDiagnostic(err)
		fmt.Fprintf(os.Stderr, "mbx cache read for %s: %s\n", namespace, message)
		microvmMbxPlainText(w, http.StatusBadGateway, message)
	case response == nil:
		microvmMbxPlainText(w, http.StatusNotFound, "object absent from S3")
	default:
		response.write(w)
	}
}

func microvmMbxNamespace(headers http.Header, remote MicrovmMbxRemote) (string, bool) {
	values := headers.Values(microvmMbxNamespaceHeader)
	if len(values) != 1 || !microvmMbxValidNamespace(values[0]) {
		return "", false
	}
	for _, allowed := range remote.Namespaces {
		if allowed == values[0] {
			return values[0], true
		}
	}
	return "", false
}

// microvmMbxResponse is a successful object read: a file (a cached or
// staged blob) or a JSON record.
type microvmMbxResponse struct {
	contentType string
	etag        string
	file        *os.File
	size        uint64
	body        []byte
}

func (r *microvmMbxResponse) write(w http.ResponseWriter) {
	w.Header().Set("Content-Type", r.contentType)
	if r.etag != "" {
		w.Header().Set("ETag", r.etag)
	}
	if r.file != nil {
		defer r.file.Close()
		w.Header().Set("Content-Length", strconv.FormatUint(r.size, 10))
		w.WriteHeader(http.StatusOK)
		// A failed copy leaves a short body, which the client sees against
		// Content-Length; there is no status left to report it with.
		io.Copy(w, r.file)
		return
	}
	w.Header().Set("Content-Length", strconv.Itoa(len(r.body)))
	w.WriteHeader(http.StatusOK)
	w.Write(r.body)
}

func microvmNewMbxProxy(remote MicrovmMbxRemote, cache string) *microvmMbxProxy {
	budget := uint64(0)
	if remote.Budget != nil {
		budget = *remote.Budget
	} else {
		budget = microvmMbxCacheBudget(cache)
	}
	return &microvmMbxProxy{remote: remote, cache: cache, budget: budget}
}

func (p *microvmMbxProxy) recordPublish(size uint64) {
	// A scan every budget/20 published bytes bounds the overshoot between
	// scans to 5% of the budget per proxy, plus in-flight writes, without
	// scanning the directory on every write.
	threshold := p.budget / 20
	p.trimMu.Lock()
	progress := &p.trimProgress
	progress.published = microvmSaturatingAdd(progress.published, size)
	if progress.running || progress.published <= threshold {
		p.trimMu.Unlock()
		return
	}
	progress.published = 0
	progress.running = true
	p.trimMu.Unlock()
	go func() {
		budget := p.budget
		for {
			if _, err := microvmMbxTrim(p.cache, &budget); err != nil {
				fmt.Fprintf(os.Stderr, "trim the mbx host cache: %s\n", err)
			}
			p.trimMu.Lock()
			if progress.published <= threshold {
				progress.running = false
				p.trimMu.Unlock()
				return
			}
			// Catch writes that crossed the threshold during the scan even
			// if there are no more requests after this one.
			progress.published = 0
			p.trimMu.Unlock()
		}
	}()
}

func microvmSaturatingAdd(a, b uint64) uint64 {
	if sum := a + b; sum >= a {
		return sum
	}
	return ^uint64(0)
}

// microvmMbxStaged is a staging file in the cache (or a temporary)
// directory: Rust's NamedTempFile.
type microvmMbxStaged struct {
	file *os.File
	path string
}

func microvmMbxStagingFile(directory string) (*microvmMbxStaged, error) {
	file, err := os.CreateTemp(directory, microvmMbxStagingPrefix+"*")
	if err != nil {
		return nil, microvmIOError(err)
	}
	return &microvmMbxStaged{file: file, path: file.Name()}, nil
}

// discard closes and removes the staging file.
func (s *microvmMbxStaged) discard() {
	s.file.Close()
	os.Remove(s.path)
}

// intoFile removes the staging file's name and keeps it open, as an
// unnamed temporary file.
func (s *microvmMbxStaged) intoFile() *os.File {
	os.Remove(s.path)
	return s.file
}

// publish names the staged file entry unless another proxy already
// published it, and returns it open for reading.
func (p *microvmMbxProxy) publish(staged *microvmMbxStaged, entry string, size uint64) (*os.File, error) {
	err := microvmPersistNoClobber(staged.path, entry)
	switch {
	case err == nil:
		p.recordPublish(size)
		return staged.file, nil
	case errors.Is(err, syscall.EEXIST):
		return staged.intoFile(), nil
	case errors.Is(err, syscall.ENOSPC):
		fmt.Fprintln(os.Stderr, "mbx host cache is full; serving the object without caching it")
		return staged.intoFile(), nil
	}
	staged.discard()
	return nil, fmt.Errorf("publish mbx cache entry %s: %w", entry, microvmIOError(err))
}

// microvmPersistNoClobber renames from to to unless to exists, as the
// tempfile crate's persist_noclobber does: renameat2 with RENAME_NOREPLACE,
// or a hard link and an unlink where the filesystem has no such rename.
func microvmPersistNoClobber(from, to string) error {
	err := unix.Renameat2(unix.AT_FDCWD, from, unix.AT_FDCWD, to, unix.RENAME_NOREPLACE)
	if err == nil || !errors.Is(err, unix.ENOSYS) && !errors.Is(err, unix.EINVAL) {
		return err
	}
	if err := unix.Link(from, to); err != nil {
		return err
	}
	_ = unix.Unlink(from)
	return nil
}

func (p *microvmMbxProxy) connectedStore() (*microvmMbxS3, error) {
	p.storeMu.Lock()
	defer p.storeMu.Unlock()
	if p.store != nil {
		return p.store, nil
	}
	store, err := microvmMbxS3Connect(p.remote)
	if err != nil {
		return nil, err
	}
	if store == nil {
		return nil, errors.New("no instance role attached")
	}
	p.store = store
	return store, nil
}

// entry is the cache file for an object. Lengths and segment boundaries are
// preserved, and the guest's namespace and prefix never become filesystem
// path components.
func (p *microvmMbxProxy) entry(namespace string, kind microvmMbxKind, digest microvmMbxDigest) string {
	identity, err := jsonvalue.Marshal([]any{
		p.remote.Bucket,
		p.remote.Prefix,
		namespace,
		kind.asStr(),
		jsonvalue.Object{
			{Key: "algorithm", Value: digest.Algorithm},
			{Key: "hash", Value: digest.Hash},
			{Key: "size", Value: digest.Size},
		},
	})
	if err != nil {
		panic(fmt.Sprintf("serialize cache identity: %v", err))
	}
	sum := sha256.Sum256(identity)
	return filepath.Join(p.cache, microvmImageHex(sum[:]))
}

func (p *microvmMbxProxy) read(namespace string, kind microvmMbxKind, digest microvmMbxDigest) (*microvmMbxResponse, error) {
	entry := p.entry(namespace, kind, digest)
	if kind != microvmMbxKindManifest {
		file, err := os.Open(entry)
		switch {
		case err == nil:
			info, err := file.Stat()
			if err != nil {
				file.Close()
				return nil, microvmIOError(err)
			}
			now := unix.NsecToTimespec(time.Now().UnixNano())
			if err := unix.UtimesNanoAt(unix.AT_FDCWD, entry, []unix.Timespec{now, {Nsec: unix.UTIME_OMIT}}, 0); err != nil {
				fmt.Fprintf(os.Stderr, "update mbx cache access time for %s: %s\n", entry, microvmIOError(err))
			}
			return &microvmMbxResponse{contentType: kind.mediaType(), file: file, size: uint64(info.Size())}, nil
		case !errors.Is(err, os.ErrNotExist):
			return nil, fmt.Errorf("open mbx cache entry: %w", microvmIOError(err))
		}
	}
	key := fmt.Sprintf("/v1/%s/%s/%s/%d", kind.asStr(), digest.Algorithm, digest.Hash, digest.Size)
	store, err := p.connectedStore()
	if err != nil {
		return nil, err
	}
	upstream, err := store.get(namespace, key)
	if err != nil || upstream == nil {
		return nil, err
	}
	if kind == microvmMbxKindBlob {
		staged, err := microvmMbxStageBlob(upstream, digest, p.cache)
		var file *os.File
		switch {
		case err == nil:
			if file, err = p.publish(staged, entry, digest.Size); err != nil {
				return nil, err
			}
		case errors.Is(err, syscall.ENOSPC):
			fmt.Fprintln(os.Stderr, "mbx host cache is full; serving the blob without caching it")
			// The partially consumed response cannot be rewound. The unit's
			// private temp directory is writable and may be on a different
			// filesystem from the cache.
			retry, err := store.get(namespace, key)
			if err != nil || retry == nil {
				return nil, err
			}
			staged, err := microvmMbxStageBlob(retry, digest, os.TempDir())
			if err != nil {
				return nil, err
			}
			file = staged.intoFile()
		default:
			return nil, err
		}
		return &microvmMbxResponse{contentType: kind.mediaType(), file: file, size: digest.Size}, nil
	}
	etags := upstream.Header.Values("ETag")
	body, err := microvmMbxReadBody(upstream, microvmMbxMaxJSONBytes)
	if err != nil {
		return nil, err
	}
	if kind == microvmMbxKindManifest {
		if len(etags) == 0 || !microvmMbxStrongETag(etags[0]) {
			return nil, errors.New("S3 action manifest has no strong ETag")
		}
		etag := etags[0]
		var manifest microvmMbxTaskActionManifest
		if err := manifest.UnmarshalJSON(body); err != nil {
			return nil, fmt.Errorf("decode action manifest: %w", err)
		}
		if !manifest.validate() || manifest.selectorDigest() != digest || string(manifest.canonicalJSON()) != string(body) {
			return nil, errors.New("S3 action manifest does not match its canonical selector")
		}
		return &microvmMbxResponse{contentType: kind.mediaType(), etag: etag, body: body}, nil
	}

	var action microvmMbxActionResult
	if err := action.UnmarshalJSON(body); err != nil {
		return nil, fmt.Errorf("decode action result: %w", err)
	}
	if action.Version != 1 || action.Action != digest {
		return nil, errors.New("S3 action result does not match the requested action")
	}
	if err := p.cacheRecord(entry, body); err != nil {
		if !errors.Is(err, syscall.ENOSPC) {
			return nil, err
		}
		fmt.Fprintln(os.Stderr, "mbx host cache is full; serving the action result without caching it")
	}
	return &microvmMbxResponse{contentType: kind.mediaType(), body: body}, nil
}

// cacheRecord publishes a verified record into the cache.
func (p *microvmMbxProxy) cacheRecord(entry string, body []byte) error {
	staged, err := microvmMbxStagingFile(p.cache)
	if err != nil {
		return err
	}
	if _, err := staged.file.Write(body); err != nil {
		staged.discard()
		return microvmIOError(err)
	}
	file, err := p.publish(staged, entry, uint64(len(body)))
	if err != nil {
		return err
	}
	return file.Close()
}

func microvmMbxStrongETag(value string) bool {
	inner, ok := strings.CutPrefix(value, `"`)
	if !ok || len(value) < 2 {
		return false
	}
	inner, ok = strings.CutSuffix(inner, `"`)
	if !ok || len(inner) < 1 || len(inner) > 256 {
		return false
	}
	for i := 0; i < len(inner); i++ {
		if b := inner[i]; b < '!' || b > '~' || b == '"' {
			return false
		}
	}
	return true
}

// microvmMbxStageBlob writes the upstream blob to a staging file in
// directory and verifies it against digest; it closes the response.
func microvmMbxStageBlob(upstream *http.Response, digest microvmMbxDigest, directory string) (*microvmMbxStaged, error) {
	defer upstream.Body.Close()
	staged, err := microvmMbxStagingFile(directory)
	if err != nil {
		return nil, err
	}
	var written uint64
	buffer := make([]byte, 64*1024)
	for {
		count, readErr := upstream.Body.Read(buffer)
		if count > 0 {
			written += uint64(count)
			if written > digest.Size {
				staged.discard()
				return nil, errors.New("S3 blob exceeds the declared size")
			}
			if _, err := staged.file.Write(buffer[:count]); err != nil {
				staged.discard()
				return nil, microvmIOError(err)
			}
		}
		if readErr == io.EOF {
			break
		}
		if readErr != nil {
			staged.discard()
			return nil, readErr
		}
	}
	matches, err := digest.matchesFile(staged.path)
	if err != nil {
		staged.discard()
		return nil, err
	}
	if !matches {
		staged.discard()
		return nil, errors.New("S3 blob failed digest verification")
	}
	if _, err := staged.file.Seek(0, io.SeekStart); err != nil {
		staged.discard()
		return nil, microvmIOError(err)
	}
	return staged, nil
}
