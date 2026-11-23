// Port of mj-github-relevance/src/github_student/inference.rs.

// Tokenization and inference. These entry points run on supervised blocking
// work.

package githubrelevance

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"slices"
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
// appends its timestamp and .json suffix. The reserving file is removed at
// once, as Rust's dropped NamedTempFile is.
func profilePrefix(directory string) (string, error) {
	file, err := os.CreateTemp(directory, "placement-")
	if err != nil {
		return "", err
	}
	path := file.Name()
	file.Close()
	if err := os.Remove(path); err != nil {
		return "", err
	}
	return path, nil
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

// newSingleThreadBuilder is Session::builder with one intra-op and one
// inter-op thread, the student's sessions' common start.
func newSingleThreadBuilder(environment *ortEnvironment) (*ortSessionBuilder, error) {
	builder, err := newOrtSessionBuilder(environment)
	if err != nil {
		return nil, err
	}
	if err := builder.withIntraThreads(1); err != nil {
		return nil, err
	}
	if err := builder.withInterThreads(1); err != nil {
		return nil, err
	}
	return builder, nil
}

// newSequentialBuilder adds the GPU sessions' settings: no memory pattern
// and sequential execution.
func newSequentialBuilder(environment *ortEnvironment) (*ortSessionBuilder, error) {
	builder, err := newSingleThreadBuilder(environment)
	if err != nil {
		return nil, err
	}
	if err := builder.withMemoryPattern(false); err != nil {
		return nil, err
	}
	if err := builder.withParallelExecution(false); err != nil {
		return nil, err
	}
	return builder, nil
}

// loadEngine is Engine::load: the CPU session of directory's model.onnx on
// the runtime library at runtime, and its tokenizer truncating the session
// segment to 7999 tokens.
func loadEngine(directory, runtime string) (*engine, error) {
	environment, err := environment(runtime)
	if err != nil {
		return nil, err
	}
	builder, err := newSingleThreadBuilder(environment)
	if err != nil {
		return nil, err
	}
	cpu, err := builder.commitFromFile(filepath.Join(directory, "model.onnx"))
	if err != nil {
		return nil, err
	}
	tokenizer, err := loadHFTokenizer(filepath.Join(directory, "tokenizer.json"), &hfTruncation{maxLength: 7999})
	if err != nil {
		return nil, err
	}
	return &engine{cpu: cpu, cuda: cudaSelected(), environment: environment, tokenizer: tokenizer}, nil
}

// cpuProbe exercises probe ownership with real ORT inference and no GPU
// dependency (a #[cfg(test)] method in Rust).
func (e *engine) cpuProbe(directory string) (*gpu, error) {
	builder, err := newSingleThreadBuilder(e.environment)
	if err != nil {
		return nil, err
	}
	session, err := builder.commitFromFile(filepath.Join(directory, "model.onnx"))
	if err != nil {
		return nil, err
	}
	return &gpu{device: StudentDeviceCPU, session: gpuSessions{buckets: []*bucket{{session: session}}}}, nil
}

// preferredDevice is the GPU this host would use, false when none.
func (e *engine) preferredDevice() (StudentDevice, bool) {
	switch {
	case directmlSelected():
		return StudentDeviceDirectML, true
	case runtime.GOOS == "darwin" && runtime.GOARCH == "arm64":
		return StudentDeviceCoreML, true
	case e.cuda:
		return StudentDeviceCUDA, true
	}
	return "", false
}

func (e *engine) loadGPU(directory string, device StudentDevice) (*gpu, error) {
	if device == StudentDeviceCoreML {
		sessions, err := e.loadCoreML(directory, true)
		if err != nil {
			return nil, err
		}
		return &gpu{device: device, session: sessions}, nil
	}
	var provider ortExecutionProvider
	switch device {
	case StudentDeviceDirectML:
		provider = ortExecutionProvider{name: ortDirectMLProvider, errorOnFailure: true}
	case StudentDeviceCUDA:
		provider = ortExecutionProvider{name: ortCUDAProvider, errorOnFailure: true}
	default:
		return nil, errors.New("invalid GPU provider")
	}
	builder, err := newSequentialBuilder(e.environment)
	if err != nil {
		return nil, err
	}
	if err := builder.withExecutionProviders(provider, ortExecutionProvider{name: ortCPUProvider}); err != nil {
		return nil, err
	}
	prefix, err := profilePrefix(directory)
	if err != nil {
		return nil, err
	}
	if err := builder.withProfiling(prefix); err != nil {
		return nil, err
	}
	session, err := builder.commitFromFile(filepath.Join(directory, "model.onnx"))
	if err != nil {
		return nil, err
	}
	placement := ortDirectMLProvider
	if device == StudentDeviceCUDA {
		placement = ortCUDAProvider
	}
	return &gpu{device: device, session: gpuSessions{buckets: []*bucket{{session: session, checkPlacement: placement}}}}, nil
}

// coremlInitializer is one external initializer of a static CoreML graph.
type coremlInitializer struct {
	Name     string   `json:"name"`
	Shape    []uint64 `json:"shape"`
	Dtype    string   `json:"dtype"`
	Location string   `json:"location"`
	Offset   uint64   `json:"offset"`
	Length   uint64   `json:"length"`
}

func (i *coremlInitializer) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "name", "shape", "dtype", "location", "offset", "length"); err != nil {
		return err
	}
	type wire coremlInitializer
	return json.Unmarshal(data, (*wire)(i))
}

// loadCoreML loads the two static CoreML graphs with their fp16 external
// initializers, accelerated or on the CPU. It registers actual fp16 values,
// not file-backed OrtValues: ORT 1.23's CoreML EP otherwise hits ENOTDIR
// while mapping external weights.
func (e *engine) loadCoreML(directory string, accelerated bool) (gpuSessions, error) {
	weights, err := os.ReadFile(filepath.Join(directory, "weights.fp16.data"))
	if err != nil {
		return gpuSessions{}, err
	}
	shared := map[string]*ortValue{}
	var buckets []*bucket
	for _, n := range []int{6144, 8192} {
		graph := fmt.Sprintf("model-coreml-%d.onnx", n)
		index := slices.IndexFunc(pinnedManifest().Files, func(file artifact) bool { return file.Name == graph })
		if index < 0 {
			return gpuSessions{}, errors.New("missing CoreML graph pin")
		}
		cache := filepath.Join(directory, "coreml", pinnedManifest().Files[index].SHA256)
		if err := os.MkdirAll(cache, 0o777); err != nil {
			return gpuSessions{}, err
		}
		builder, err := newSequentialBuilder(e.environment)
		if err != nil {
			return gpuSessions{}, err
		}
		data, err := os.ReadFile(filepath.Join(directory, fmt.Sprintf("initializers-%d.json", n)))
		if err != nil {
			return gpuSessions{}, err
		}
		var initializers []coremlInitializer
		if err := json.Unmarshal(data, &initializers); err != nil {
			return gpuSessions{}, err
		}
		for _, initializer := range initializers {
			if initializer.Dtype != "float16" || initializer.Location != "weights.fp16.data" {
				return gpuSessions{}, errors.New("unexpected external initializer type/location")
			}
			key := fmt.Sprintf("%d:%d:%v", initializer.Offset, initializer.Length, initializer.Shape)
			tensor, ok := shared[key]
			if !ok {
				end := initializer.Offset + initializer.Length
				if end < initializer.Offset {
					return gpuSessions{}, errors.New("initializer offset overflow")
				}
				if end > uint64(len(weights)) {
					return gpuSessions{}, errors.New("initializer outside pinned weights")
				}
				bytes := weights[initializer.Offset:end]
				if len(bytes)%2 != 0 {
					return gpuSessions{}, errors.New("odd fp16 initializer length")
				}
				values := make([]uint16, len(bytes)/2)
				for i := range values {
					values[i] = binary.LittleEndian.Uint16(bytes[2*i:])
				}
				shape := make([]int64, len(initializer.Shape))
				for i, dimension := range initializer.Shape {
					shape[i] = int64(dimension)
				}
				if tensor, err = newOrtTensorFloat16(shape, values); err != nil {
					return gpuSessions{}, err
				}
				shared[key] = tensor
			}
			if err := builder.withExternalInitializer(initializer.Name, tensor); err != nil {
				return gpuSessions{}, err
			}
		}
		placement := ""
		if accelerated {
			coreml := ortExecutionProvider{
				name: ortCoreMLProvider,
				options: []ortOption{
					{key: "ModelFormat", value: "MLProgram"},
					{key: "MLComputeUnits", value: "CPUAndGPU"},
					{key: "RequireStaticInputShapes", value: "1"},
					{key: "AllowLowPrecisionAccumulationOnGPU", value: "0"},
					{key: "ModelCacheDirectory", value: cache},
				},
				errorOnFailure: true,
			}
			if err := builder.withExecutionProviders(coreml, ortExecutionProvider{name: ortCPUProvider}); err != nil {
				return gpuSessions{}, err
			}
			prefix, err := profilePrefix(cache)
			if err != nil {
				return gpuSessions{}, err
			}
			if err := builder.withProfiling(prefix); err != nil {
				return gpuSessions{}, err
			}
			placement = ortCoreMLProvider
		}
		session, err := builder.commitFromFile(filepath.Join(directory, graph))
		if err != nil {
			return gpuSessions{}, err
		}
		buckets = append(buckets, &bucket{length: n, session: session, checkPlacement: placement})
	}
	return gpuSessions{coreML: true, buckets: buckets}, nil
}

// tokens is the pair's token IDs and attention mask.
func (e *engine) tokens(item, session string) ([]int64, []int64, error) {
	encoded, err := e.tokenizer.encode(item, &session, true)
	if err != nil {
		return nil, nil, fmt.Errorf("tokenizing GitHub relevance pair: %w", err)
	}
	ids := make([]int64, len(encoded.ids))
	for i, id := range encoded.ids {
		ids[i] = int64(id)
	}
	mask := make([]int64, len(encoded.attentionMask))
	for i, value := range encoded.attentionMask {
		mask[i] = int64(value)
	}
	return ids, mask, nil
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
	var chosen *bucket
	if !s.coreML {
		chosen = s.buckets[0]
	} else {
		index := slices.IndexFunc(s.buckets, func(b *bucket) bool { return len(ids) <= b.length })
		if index < 0 {
			return 0, errors.New("pair exceeds CoreML bucket")
		}
		chosen = s.buckets[index]
		ids = resized(ids, chosen.length)
		mask = resized(mask, chosen.length)
	}
	score, err := scoreGPU(chosen.session, ids, mask)
	if err != nil {
		return 0, err
	}
	if provider := chosen.checkPlacement; provider != "" {
		profile, err := chosen.session.endProfiling()
		if err != nil {
			return 0, err
		}
		parsed := func() error {
			data, err := os.ReadFile(profile)
			if err != nil {
				return err
			}
			var events []any
			if err := json.Unmarshal(data, &events); err != nil {
				return err
			}
			for _, event := range events {
				object, _ := event.(map[string]any)
				args, _ := object["args"].(map[string]any)
				if placed, _ := args["provider"].(string); placed == provider {
					return nil
				}
			}
			return fmt.Errorf("%s graph ran wholly on CPU", provider)
		}()
		if err := os.Remove(profile); err != nil {
			return 0, err
		}
		if parsed != nil {
			return 0, parsed
		}
		chosen.checkPlacement = ""
	}
	return score, nil
}

// resized is Vec::resize with zeros, on a copy.
func resized(values []int64, length int) []int64 {
	out := make([]int64, length)
	copy(out, values)
	return out
}

func scoreGPU(session *ortSession, ids, mask []int64) (float64, error) {
	n := len(ids)
	if len(mask) != n {
		return 0, errors.New("student mask length mismatch")
	}
	idsTensor, err := newOrtTensorInt64([]int64{1, int64(n)}, ids)
	if err != nil {
		return 0, err
	}
	defer idsTensor.release()
	maskTensor, err := newOrtTensorInt64([]int64{1, int64(n)}, mask)
	if err != nil {
		return 0, err
	}
	defer maskTensor.release()
	outputs, err := session.run([]ortInput{{name: "input_ids", value: idsTensor}, {name: "attention_mask", value: maskTensor}}, nil)
	if err != nil {
		return 0, err
	}
	defer outputs.release()
	if len(outputs.values) == 0 || outputs.values[0] == nil {
		return 0, errors.New("student returned 0 logits")
	}
	_, logits, err := outputs.values[0].tensorFloat32()
	if err != nil {
		return 0, err
	}
	if len(logits) != 1 {
		return 0, fmt.Errorf("student returned %d logits", len(logits))
	}
	if !isFinite32(logits[0]) {
		return 0, errors.New("student returned nonfinite logit")
	}
	probability := 1.0 / (1.0 + math.Exp(-float64(logits[0])))
	if math.IsNaN(probability) || math.IsInf(probability, 0) {
		return 0, errors.New("student returned nonfinite probability")
	}
	return probability, nil
}

// parallelism is how many inferences may run at once: the cores, limited
// by available memory for measuredRunBytes each.
func parallelism() (int, error) {
	cores := runtime.NumCPU()
	available, err := availableMemory()
	if err != nil {
		return 0, err
	}
	memoryRuns := available / measuredRunBytes
	if memoryRuns == 0 {
		return 0, errors.New("insufficient available memory for one measured student inference")
	}
	if memoryRuns > uint64(math.MaxInt) {
		return 0, errors.New("out of range integral type conversion attempted")
	}
	return min(cores, int(memoryRuns)), nil
}
