// Port of mj-github-relevance/src/github_student/tests.rs.

package githubrelevance

import (
	"bufio"
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"net/http/httptest"
	"os"
	"path"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

//go:embed testdata/tests/fixtures/github-student/parity.jsonl
var studentParityJSONL string

//go:embed testdata/tests/fixtures/github-student/overflow.json
var studentOverflowJSON []byte

type studentPair struct {
	Identity      json.RawMessage `json:"identity"`
	Item          string          `json:"item"`
	Session       string          `json:"session"`
	InputIDs      []int64         `json:"input_ids"`
	AttentionMask []int64         `json:"attention_mask"`
	PythonFP32    float64         `json:"python_fp32"`
}

func (p *studentPair) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "identity", "item", "session", "input_ids", "attention_mask", "python_fp32"); err != nil {
		return err
	}
	type wire studentPair
	return json.Unmarshal(data, (*wire)(p))
}

func studentPairs(t *testing.T) []studentPair {
	t.Helper()
	var pairs []studentPair
	scanner := bufio.NewScanner(strings.NewReader(studentParityJSONL))
	scanner.Buffer(nil, 1<<26)
	for scanner.Scan() {
		var pair studentPair
		if err := json.Unmarshal(scanner.Bytes(), &pair); err != nil {
			t.Fatal(err)
		}
		pairs = append(pairs, pair)
	}
	return pairs
}

func studentOverflow(t *testing.T) studentPair {
	t.Helper()
	var overflow studentPair
	if err := json.Unmarshal(studentOverflowJSON, &overflow); err != nil {
		t.Fatal(err)
	}
	return overflow
}

func shortestPair(pairs []studentPair) studentPair {
	return slices.MinFunc(pairs, func(a, b studentPair) int { return len(a.InputIDs) - len(b.InputIDs) })
}

func studentTestCache(t *testing.T) string {
	t.Helper()
	cache, ok := os.LookupEnv("MJ_GITHUB_STUDENT_TEST_CACHE")
	if !ok {
		t.Fatal("missing verified model/native cache; see tests/fixtures/github-student/README.md")
	}
	if _, prepare := os.LookupEnv("MJ_GITHUB_STUDENT_PREPARE"); prepare {
		if err := os.MkdirAll(cache, 0o777); err != nil {
			t.Fatal(err)
		}
		return cache
	}
	if info, err := os.Stat(filepath.Join(cache, "github-student", pinnedManifest().Revision)); err != nil || !info.IsDir() {
		t.Fatalf("missing pinned model revision in %s", cache)
	}
	if info, err := os.Stat(filepath.Join(cache, "github-student-native")); err != nil || !info.IsDir() {
		t.Fatalf("missing pinned native runtime in %s", cache)
	}
	return cache
}

func waitForStudent(t *testing.T, student *Student, predicate func(StudentState) bool) StudentState {
	t.Helper()
	student.inner.mu.Lock()
	_, dormant := student.inner.machine.state.(stateDormant)
	student.inner.mu.Unlock()
	if dormant {
		if _, err := student.scoreSegments(context.Background(), "first real classification request", "session"); err == nil {
			t.Fatal("a dormant student must not score")
		}
	}
	deadline := time.After(120 * time.Second)
	for {
		changed, stop := student.inner.changed.Subscribe()
		state := student.State()
		if predicate(state) {
			stop()
			return state
		}
		select {
		case <-changed:
			stop()
		case <-deadline:
			stop()
			t.Fatalf("student state transition did not finish: %s", state)
		}
	}
}

func band(score float64) int {
	switch {
	case score < 0.0716:
		return 0
	case score < 0.2830:
		return 1
	}
	return 2
}

func readyEngine(t *testing.T, student *Student) *ready {
	t.Helper()
	student.inner.mu.Lock()
	defer student.inner.mu.Unlock()
	current, ok := student.inner.machine.state.(stateReady)
	if !ok {
		t.Fatal("student not ready")
	}
	return current.ready
}

func TestPythonParityAndSixteenConcurrentCallsShareOneLoadedSession(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "requires the verified pinned model/runtime cache; missing assets fail explicitly")
	targets.InstallRustlsCryptoProvider()
	student := NewStudent(studentTestCache(t), false, func(StudentNotice) {
		t.Error("CPU-only scorer must not select a GPU")
	})
	ctx := context.Background()
	state := waitForStudent(t, student, func(s StudentState) bool { _, loading := s.(StudentStateLoading); return !loading })
	if state != (StudentStateReady{}) {
		t.Fatalf("%s", state)
	}
	cases := studentPairs(t)
	var serial []float64
	engine := readyEngine(t, student).engine
	for _, pair := range cases {
		ids, mask, err := engine.tokens(pair.Item, pair.Session)
		if err != nil {
			t.Fatal(err)
		}
		if !slices.Equal(ids, pair.InputIDs) {
			t.Fatalf("%s: first divergence is token IDs", pair.Identity)
		}
		if !slices.Equal(mask, pair.AttentionMask) {
			t.Fatalf("%s: first divergence is masks", pair.Identity)
		}
		score, err := student.scoreSegments(ctx, pair.Item, pair.Session)
		if err != nil {
			t.Fatal(err)
		}
		if math.Abs(score-pair.PythonFP32) > 0.005 {
			t.Fatalf("%s: score %v vs Python %v", pair.Identity, score, pair.PythonFP32)
		}
		near := math.Abs(pair.PythonFP32-0.0716) <= 0.005 || math.Abs(pair.PythonFP32-0.2830) <= 0.005
		if band(score) != band(pair.PythonFP32) && !near {
			t.Fatalf("%s: routing changed outside near-cut margin", pair.Identity)
		}
		serial = append(serial, score)
	}
	overflow := studentOverflow(t)
	ids, mask, err := engine.tokens(overflow.Item, overflow.Session)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(ids, overflow.InputIDs) {
		t.Fatal("overflow: first divergence is token IDs")
	}
	if !slices.Equal(mask, overflow.AttentionMask) {
		t.Fatal("overflow: first divergence is masks")
	}
	score, err := student.scoreSegments(ctx, overflow.Item, overflow.Session)
	if err != nil {
		t.Fatal(err)
	}
	if math.Abs(score-overflow.PythonFP32) > 0.005 {
		t.Fatalf("overflow score %v vs Python %v", score, overflow.PythonFP32)
	}
	concurrent := make([]float64, 16)
	errs := make([]error, 16)
	var wait sync.WaitGroup
	for i, pair := range cases[:16] {
		wait.Go(func() { concurrent[i], errs[i] = student.scoreSegments(ctx, pair.Item, pair.Session) })
	}
	wait.Wait()
	for i := range concurrent {
		if errs[i] != nil {
			t.Fatal(errs[i])
		}
		if math.Float64bits(concurrent[i]) != math.Float64bits(serial[i]) {
			t.Fatalf("%s: serial/concurrent bits differ", cases[i].Identity)
		}
	}
	if err := student.Shutdown(ctx); err != nil {
		t.Fatal(err)
	}
}

func copyTree(t *testing.T, source, destination string) {
	t.Helper()
	if err := os.MkdirAll(destination, 0o777); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(source)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		to := filepath.Join(destination, entry.Name())
		if entry.IsDir() {
			copyTree(t, filepath.Join(source, entry.Name()), to)
		} else if err := copyFile(filepath.Join(source, entry.Name()), to); err != nil {
			t.Fatal(err)
		}
	}
}

// Hard-won: #1513: failed CPU controls left the GPU benchmark reservation stuck.
func TestOwnerReusesLoadedCPUAcrossHealthAndPreferenceChanges(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "requires verified pinned model/runtime assets for actual ORT loader recovery")
	targets.InstallRustlsCryptoProvider()
	cache := studentTestCache(t)
	root, err := os.MkdirTemp("", "github-student-owner-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(root) })
	copyTree(t, filepath.Join(cache, "github-student-native"), filepath.Join(root, "github-student-native"))
	var requests atomic.Int64
	modelRoot := filepath.Join(cache, "github-student", pinnedManifest().Revision)
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		requests.Add(1)
		file, err := os.Open(filepath.Join(modelRoot, path.Base(request.URL.Path)))
		if err != nil {
			t.Logf("student model fixture read failed: %v", err)
			writer.WriteHeader(http.StatusNotFound)
			return
		}
		defer file.Close()
		io.Copy(writer, file)
	}))
	defer server.Close()
	var noticesMu sync.Mutex
	var notices []StudentNotice
	loadFailures := new(atomic.Int64)
	loadFailures.Store(1)
	student := startStudent(root, false, func(notice StudentNotice) {
		noticesMu.Lock()
		notices = append(notices, notice)
		noticesMu.Unlock()
	}, preparationOptions{
		modelBase:     server.URL,
		modelManifest: pinnedManifest().clone(),
		loadFailures:  loadFailures,
	})
	ctx := context.Background()
	time.Sleep(time.Millisecond)
	if requests.Load() != 0 {
		t.Fatal("constructor and status must not prepare")
	}
	if _, err := os.Stat(filepath.Join(root, "github-student")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("constructor and status must not prepare")
	}
	if student.State() != (StudentStateLoading{}) {
		t.Fatalf("%s", student.State())
	}
	var wait sync.WaitGroup
	var admitted atomic.Int64
	for range 16 {
		wait.Go(func() {
			if _, err := student.scoreSegments(ctx, "item", "session"); err == nil {
				admitted.Add(1)
			}
		})
	}
	wait.Wait()
	if admitted.Load() != 0 {
		t.Fatal("an unprepared student must not score")
	}
	failure := waitForStudent(t, student, func(s StudentState) bool { _, failed := s.(StudentStateFailed); return failed })
	if failed, ok := failure.(StudentStateFailed); !ok || failed.Kind != StudentFailureKindLoad {
		t.Fatalf("%s", failure)
	}
	state := waitForStudent(t, student, func(s StudentState) bool { return s == (StudentStateReady{}) })
	if state != (StudentStateReady{}) {
		t.Fatalf("%s", state)
	}
	if got, want := requests.Load(), int64(len(pinnedManifest().selected(false).Files)); got != want {
		t.Fatalf("retry must reuse verified assets; callers must not independently prepare: %d requests, want %d", got, want)
	}
	current := readyEngine(t, student)
	engine, permits := current.engine, current.permits
	permits <- struct{}{}
	student.inner.completed(engine, 0, errors.New("recorded inference failure"))
	if student.State() != (StudentStateReady{}) {
		t.Fatalf("%s", student.State())
	}
	student.inner.completed(engine, 0, errors.New("second failure while unhealthy"))
	student.inner.mu.Lock()
	if current, ok := student.inner.machine.state.(stateReady); !ok || !current.ready.failing || current.ready.engine != engine {
		student.inner.mu.Unlock()
		t.Fatal("lost readiness or health")
	}
	student.inner.mu.Unlock()
	student.inner.completed(engine, 0.5, nil)
	noticesMu.Lock()
	failing, recovered := 0, 0
	for _, notice := range notices {
		switch notice.(type) {
		case StudentNoticeInferenceFailing:
			failing++
		case StudentNoticeInferenceRecovered:
			recovered++
		}
	}
	noticesMu.Unlock()
	if failing != 1 || recovered != 1 {
		t.Fatalf("%d failing and %d recovered notices", failing, recovered)
	}
	student.SetUseGPU(true)
	student.SetUseGPU(false)
	student.inner.mu.Lock()
	if current, ok := student.inner.machine.state.(stateReady); !ok || current.ready.failing || current.ready.engine != engine {
		student.inner.mu.Unlock()
		t.Fatal("lost readiness")
	} else if _, cpu := current.ready.device.(deviceCPU); !cpu {
		student.inner.mu.Unlock()
		t.Fatalf("device %#v", current.ready.device)
	}
	student.inner.mu.Unlock()
	<-permits
	// Use a real CPU-backed session as the device candidate: this tests the
	// owner's probe/error path on every host without probing a local GPU.
	directory := filepath.Join(root, "github-student", pinnedManifest().Revision)
	candidate, err := engine.cpuProbe(directory)
	if err != nil {
		t.Fatal(err)
	}
	pending := func() bool {
		student.inner.mu.Lock()
		defer student.inner.mu.Unlock()
		current, ok := student.inner.machine.state.(stateReady)
		if !ok {
			t.Fatal("lost readiness")
		}
		reserved, ok := current.ready.device.(devicePending)
		return ok && reserved.gpu == candidate
	}
	student.inner.mu.Lock()
	if current, ok := student.inner.machine.state.(stateReady); ok {
		current.ready.device = devicePending{gpu: candidate}
	}
	student.inner.mu.Unlock()
	pair := shortestPair(studentPairs(t))
	oversizedItem := strings.Repeat("repository ", 9000)
	if _, err := student.scoreSegments(ctx, oversizedItem, pair.Session); err == nil {
		t.Fatal("an oversized pair must not score")
	}
	if !pending() {
		t.Fatal("tokenization errors must release the reservation without choosing a device")
	}
	student.inner.probeControlFailures.Store(1)
	_, err = student.scoreSegments(ctx, pair.Item, pair.Session)
	var inference StudentErrorInference
	if !errors.As(err, &inference) || !strings.Contains(inference.Cause, "CPU-control") {
		t.Fatalf("error %v", err)
	}
	if !pending() {
		t.Fatal("a failed CPU control must leave the decision pending for the next real pair")
	}
	if readyEngine(t, student).engine != engine {
		t.Fatal("the engine changed")
	}
	score, err := student.scoreSegments(ctx, pair.Item, pair.Session)
	if err != nil {
		t.Fatal(err)
	}
	if math.Abs(score-pair.PythonFP32) > 0.005 {
		t.Fatalf("score %v vs Python %v", score, pair.PythonFP32)
	}
	student.inner.mu.Lock()
	if current, ok := student.inner.machine.state.(stateReady); !ok {
		student.inner.mu.Unlock()
		t.Fatal("lost readiness")
	} else {
		switch current.ready.device.(type) {
		case deviceGPU, deviceCPU:
		default:
			student.inner.mu.Unlock()
			t.Fatalf("the later successful probe must resolve its reservation: %#v", current.ready.device)
		}
		if current.ready.failing {
			student.inner.mu.Unlock()
			t.Fatal("the student is failing")
		}
	}
	student.inner.mu.Unlock()
	if got, want := requests.Load(), int64(len(pinnedManifest().selected(false).Files)); got != want {
		t.Fatalf("%d requests, want %d", got, want)
	}
	if err := student.Shutdown(ctx); err != nil {
		t.Fatal(err)
	}
}

// Two independently exported static graphs must agree with Python,
// including only_second overflow. On a Mac this also proves real CoreML
// placement.
func TestCoremlExternalInitializersPaddingAndPythonParity(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "downloads optional CoreML graphs; runs real static graph inference")
	targets.InstallRustlsCryptoProvider()
	root := studentTestCache(t)
	client := &http.Client{}
	ctx := context.Background()
	directory, err := prepare(ctx, filepath.Join(root, "github-student"), pinnedManifest().selected(true), "https://huggingface.co", client)
	if err != nil {
		t.Fatal(err)
	}
	runtimeLibrary, err := prepareSelected(ctx, filepath.Join(root, "github-student-native"), false, func(StudentNotice) {}, client)
	if err != nil {
		t.Fatal(err)
	}
	engine, err := loadEngine(directory, runtimeLibrary)
	if err != nil {
		t.Fatal(err)
	}
	accelerated := runtime.GOOS == "darwin" && runtime.GOARCH == "arm64"
	model, err := engine.loadCoreML(directory, accelerated)
	if err != nil {
		t.Fatal(err)
	}
	cases := studentPairs(t)
	for _, pair := range []studentPair{shortestPair(cases), studentOverflow(t)} {
		score, err := model.score(pair.InputIDs, pair.AttentionMask)
		if err != nil {
			t.Fatal(err)
		}
		if math.Abs(score-pair.PythonFP32) > 0.005 {
			t.Fatalf("%s: %v vs %v", pair.Identity, score, pair.PythonFP32)
		}
		if band(score) != band(pair.PythonFP32) && math.Abs(pair.PythonFP32-0.0716) > 0.005 && math.Abs(pair.PythonFP32-0.283) > 0.005 {
			t.Fatalf("%s: routing changed", pair.Identity)
		}
	}
}

func TestActualGPUPythonParityAndWarmLatency(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "requires a host GPU; deliberately fails if its real EP cannot run")
	targets.InstallRustlsCryptoProvider()
	root := studentTestCache(t)
	client := &http.Client{}
	ctx := context.Background()
	runtimeLibrary, err := prepareSelected(ctx, filepath.Join(root, "github-student-native"), true, func(notice StudentNotice) {
		fmt.Fprintf(os.Stderr, "%#v\n", notice)
	}, client)
	if err != nil {
		t.Fatal(err)
	}
	initial, err := prepare(ctx, filepath.Join(root, "github-student"), pinnedManifest().selected(false), "https://huggingface.co", client)
	if err != nil {
		t.Fatal(err)
	}
	engine, err := loadEngine(initial, runtimeLibrary)
	if err != nil {
		t.Fatal(err)
	}
	device, ok := engine.preferredDevice()
	if !ok {
		t.Fatal("host GPU libraries unavailable")
	}
	directory, err := prepare(ctx, filepath.Join(root, "github-student"), pinnedManifest().selected(device == StudentDeviceCoreML), "https://huggingface.co", client)
	if err != nil {
		t.Fatal(err)
	}
	candidate, err := engine.loadGPU(directory, device)
	if err != nil {
		t.Fatal(err)
	}
	candidate.sessionMu.Lock()
	defer candidate.sessionMu.Unlock()
	session := &candidate.session
	cases := studentPairs(t)
	if err := session.warmup(cases[0].InputIDs, cases[0].AttentionMask); err != nil {
		t.Fatal(err)
	}
	var cpuSeconds, gpuSeconds, maxError float64
	for index, pair := range cases {
		start := time.Now()
		score, err := session.score(pair.InputIDs, pair.AttentionMask)
		if err != nil {
			t.Fatal(err)
		}
		gpuSeconds += time.Since(start).Seconds()
		maxError = math.Max(maxError, math.Abs(score-pair.PythonFP32))
		if math.Abs(score-pair.PythonFP32) > 0.005 {
			t.Fatalf("%s: %v vs %v", pair.Identity, score, pair.PythonFP32)
		}
		if band(score) != band(pair.PythonFP32) && math.Abs(pair.PythonFP32-0.0716) > 0.005 && math.Abs(pair.PythonFP32-0.283) > 0.005 {
			t.Fatalf("%s: routing changed", pair.Identity)
		}
		if index < 5 {
			start := time.Now()
			if _, err := engine.scoreTokens(pair.InputIDs, pair.AttentionMask); err != nil {
				t.Fatal(err)
			}
			cpuSeconds += time.Since(start).Seconds()
		}
	}
	fmt.Fprintf(os.Stderr, "device=%s pairs=%d max_error=%v warm_gpu_mean_seconds=%v cpu_first5_mean_seconds=%v\n",
		device, len(cases), maxError, gpuSeconds/float64(len(cases)), cpuSeconds/5)
}
