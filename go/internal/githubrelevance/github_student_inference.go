// Port of mj-github-relevance/src/github_student/inference.rs.

// Tokenization and inference. These entry points run on supervised blocking
// work.

package githubrelevance

import (
	"sync"
)

// measuredRunBytes is the measured peak memory growth of one student
// inference. Measured ORT1.23.0 rank3 graph, fp32 CPU, one intra-op thread:
// 5649-token p95 grew RSS by 458452992B (lean/ort1230-rank3-p95.log). The
// documented 7999-token maximum measured 599478272B growth; use that larger
// observed peak so a maximum-length fan-out also fits. Evidence:
// lean/max-context-memory.log, ORT1.23.0, arena enabled.
const measuredRunBytes uint64 = 599_478_272

// profilePrefix reserves a unique profile file prefix in directory; ORT
// appends its timestamp and .json suffix.
func profilePrefix(directory string) (string, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs profile_prefix")
}

// engine is the loaded student: its CPU session, which runs concurrently,
// and its tokenizer.
type engine struct {
	cpu         *ortSession
	environment *ortEnvironment
	cuda        bool
	tokenizer   hfTokenizer
}

// gpu is a GPU device candidate. Its sessions admit one Run at a time.
type gpu struct {
	device StudentDevice
	// sessionMu guards session (Rust's Mutex<GpuSessions>).
	sessionMu sync.Mutex
	session   gpuSessions
}

// loadEngine is Engine::load: the CPU session of directory's model.onnx on
// the runtime library at runtime, and its tokenizer truncating the session
// segment to 7999 tokens.
func loadEngine(directory, runtime string) (*engine, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs Engine::load")
}

// cpuProbe exercises probe ownership with real ORT inference and no GPU
// dependency (a #[cfg(test)] method in Rust).
func (e *engine) cpuProbe(directory string) (*gpu, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs Engine::cpu_probe")
}

// preferredDevice is the GPU this host would use, false when none.
func (e *engine) preferredDevice() (StudentDevice, bool) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs Engine::preferred_device")
}

func (e *engine) loadGPU(directory string, device StudentDevice) (*gpu, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs Engine::load_gpu")
}

// loadCoreML loads the two static CoreML graphs with their fp16 external
// initializers, accelerated or on the CPU.
func (e *engine) loadCoreML(directory string, accelerated bool) (gpuSessions, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs Engine::load_coreml")
}

// tokens is the pair's token IDs and attention mask.
func (e *engine) tokens(item, session string) ([]int64, []int64, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs Engine::tokens")
}

func (e *engine) scoreTokens(ids, mask []int64) (float64, error) {
	return score(e.cpu, ids, mask)
}

type bucket struct {
	// length is the static graph's input length, 0 for a dynamic graph.
	length  int
	session *ortSession
	// checkPlacement is the provider whose placement the next score
	// confirms from the session profile; "" once confirmed or unchecked.
	checkPlacement string
}

// gpuSessions is GpuSessions: Dynamic is one bucket of length 0, CoreMl one
// bucket per static graph, shortest first.
type gpuSessions struct {
	coreML  bool
	buckets []*bucket
}

func (s *gpuSessions) warmup(ids, mask []int64) error {
	_, err := s.score(ids, mask)
	return err
}

func (s *gpuSessions) score(ids, mask []int64) (float64, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs GpuSessions::score")
}

func scoreGPU(session *ortSession, ids, mask []int64) (float64, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs score_gpu")
}

// parallelism is how many inferences may run at once: the cores, limited
// by available memory for measuredRunBytes each.
func parallelism() (int, error) {
	panic("unported: mj-github-relevance/src/github_student/inference.rs parallelism")
}
