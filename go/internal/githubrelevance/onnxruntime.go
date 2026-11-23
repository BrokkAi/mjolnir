// ONNX Runtime binding for this package, in place of Rust's ort crate.
//
// Rust uses ort 2.0.0-rc.15 with load-dynamic and api-23: the ONNX Runtime
// 1.23.0 shared library that github_student_runtime.go downloads and
// verifies is loaded at run time from its cache path. The established Go
// binding for that API version, github.com/yalue/onnxruntime_go v1.25.0,
// cannot enable or end session profiling or register external
// initializers, which the GPU placement check and the CoreML graphs need,
// and later releases bind newer API versions that ORT 1.23.0 refuses. This
// file is therefore a small binding of the ORT C API covering exactly what
// the package calls, as other Mjolnir-local helpers stand in for library
// pieces elsewhere in the port. It binds the OrtApi table of
// ORT_API_VERSION 23 through cgo against the ORT 1.23.0 C header, loading
// the library with dlopen (LoadLibraryExW on Windows) from the verified
// path.
//
// Every handle carries a cleanup (runtime.AddCleanup) that releases it once
// unreachable, so a session still used by an in-flight inference outlives
// its replacement as an Arc-held one does in Rust; release frees a handle
// earlier where its owner is unique, such as Run outputs. ORT permits
// concurrent Run on one session; callers serialize sessions whose provider
// does not (DirectML) and the Guardian's session, as Rust does.

package githubrelevance

import (
	"sync/atomic"
	"unsafe"
)

// ortAPIVersion is the OrtApi version bound, ort's api-23.
const ortAPIVersion = 23

// ortAPI is the loaded library and its OrtApi function table. Loading is
// process-wide in ORT, and so is this (ort::api()).
type ortAPI struct {
	// library is the dlopen/LoadLibrary handle, never closed.
	library unsafe.Pointer
	// table is the library's const OrtApi*.
	table unsafe.Pointer
}

// ortLoadedAPI is the API bound by loadOrtEnvironment; tensors and sessions
// use it.
var ortLoadedAPI atomic.Pointer[ortAPI]

// ortEnvironment is ort's Environment: the process's OrtEnv.
type ortEnvironment struct {
	api *ortAPI
	// env is the OrtEnv*.
	env unsafe.Pointer
}

// loadOrtEnvironment loads the ORT library at path, binds its API and
// creates the environment (ort::init_from(path).build()).
func loadOrtEnvironment(path string) (*ortEnvironment, error) {
	panic("unported: mj-github-relevance ort binding loadOrtEnvironment")
}

// ORT's execution provider names, as session profiles report them.
const (
	ortCUDAProvider     = "CUDAExecutionProvider"
	ortDirectMLProvider = "DmlExecutionProvider"
	ortCoreMLProvider   = "CoreMLExecutionProvider"
	ortCPUProvider      = "CPUExecutionProvider"
)

// ortOption is one provider option, as ORT's provider option keys name it
// (CoreML's "ModelFormat", CUDA's "use_tf32").
type ortOption struct {
	key   string
	value string
}

// ortExecutionProvider is one provider registration (ort::ep::*::build()).
type ortExecutionProvider struct {
	name    string
	options []ortOption
	// errorOnFailure makes a failed registration an error instead of a
	// skipped provider (ort's error_on_failure).
	errorOnFailure bool
}

// ortSessionBuilder is ort's SessionBuilder: session options being set.
type ortSessionBuilder struct {
	environment *ortEnvironment
	// options is the OrtSessionOptions*.
	options unsafe.Pointer
	// initializers are the external initializers registered so far; the
	// committed session keeps them alive, as ORT requires.
	initializers []*ortValue
}

func newOrtSessionBuilder(environment *ortEnvironment) (*ortSessionBuilder, error) {
	panic("unported: mj-github-relevance ort binding newOrtSessionBuilder")
}

func (b *ortSessionBuilder) withIntraThreads(n int) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_intra_threads")
}

func (b *ortSessionBuilder) withInterThreads(n int) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_inter_threads")
}

func (b *ortSessionBuilder) withMemoryPattern(enabled bool) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_memory_pattern")
}

func (b *ortSessionBuilder) withParallelExecution(enabled bool) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_parallel_execution")
}

func (b *ortSessionBuilder) withExecutionProviders(providers ...ortExecutionProvider) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_execution_providers")
}

// withProfiling enables profiling into files starting with prefix.
func (b *ortSessionBuilder) withProfiling(prefix string) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_profiling")
}

func (b *ortSessionBuilder) withExternalInitializer(name string, value *ortValue) error {
	panic("unported: mj-github-relevance ort binding SessionBuilder::with_external_initializer")
}

func (b *ortSessionBuilder) commitFromFile(path string) (*ortSession, error) {
	panic("unported: mj-github-relevance ort binding SessionBuilder::commit_from_file")
}

// ortSession is ort's Session.
type ortSession struct {
	api *ortAPI
	// session is the OrtSession*.
	session unsafe.Pointer
	// retained are values the session reads for its lifetime (external
	// initializers).
	retained []*ortValue
}

type ortInput struct {
	name  string
	value *ortValue
}

// ortOutputs is a Run's outputs, by position and by name.
type ortOutputs struct {
	names  []string
	values []*ortValue
}

func (o ortOutputs) get(name string) (*ortValue, bool) {
	for i, candidate := range o.names {
		if candidate == name {
			return o.values[i], true
		}
	}
	return nil, false
}

// release frees the outputs now.
func (o ortOutputs) release() {
	for _, value := range o.values {
		value.release()
	}
}

// run is ORT's Run with no run options: the named outputs, or every output
// of the session in its order when outputNames is nil.
func (s *ortSession) run(inputs []ortInput, outputNames []string) (ortOutputs, error) {
	panic("unported: mj-github-relevance ort binding Session::run")
}

// endProfiling ends the session's profile and returns the file it wrote.
func (s *ortSession) endProfiling() (string, error) {
	panic("unported: mj-github-relevance ort binding Session::end_profiling")
}

// release frees the session now; its retained values go with it.
func (s *ortSession) release() {
	panic("unported: mj-github-relevance ort binding Session release")
}

// ortValue is an OrtValue tensor.
type ortValue struct {
	api *ortAPI
	// value is the OrtValue*.
	value unsafe.Pointer
	// data is Go memory ORT reads for the value's lifetime, nil when ORT
	// owns the data.
	data any
}

// newOrtTensorInt64 is Tensor::from_array((shape, data)) for int64 data.
func newOrtTensorInt64(shape []int64, data []int64) (*ortValue, error) {
	panic("unported: mj-github-relevance ort binding Tensor::<i64>::from_array")
}

// newOrtTensorFloat16 is Tensor::from_array((shape, data)) for IEEE 754
// half-precision values given as their bits.
func newOrtTensorFloat16(shape []int64, bits []uint16) (*ortValue, error) {
	panic("unported: mj-github-relevance ort binding Tensor::<f16>::from_array")
}

// tensorFloat32 is try_extract_tensor::<f32>: the tensor's shape and a copy
// of its data, an error for a tensor of another element type.
func (v *ortValue) tensorFloat32() ([]int64, []float32, error) {
	panic("unported: mj-github-relevance ort binding Value::try_extract_tensor::<f32>")
}

// release frees the value now.
func (v *ortValue) release() {
	panic("unported: mj-github-relevance ort binding Value release")
}
