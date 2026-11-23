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
// pieces elsewhere in the port. It calls the OrtApi table of
// ORT_API_VERSION 23 through github.com/ebitengine/purego, without cgo,
// loading the library with dlopen (onnxruntime_dlopen.go) or
// LoadLibraryExW (onnxruntime_windows.go) from the verified path. The
// table indices below are the member positions in struct OrtApi of the ORT
// 1.23.0 C header (onnxruntime_c_api.h); ORT only ever appends members.
//
// Every handle carries a cleanup (runtime.AddCleanup) that releases it once
// unreachable, so a session still used by an in-flight inference outlives
// its replacement as an Arc-held one does in Rust; release frees a handle
// earlier where its owner is unique, such as Run outputs. ORT permits
// concurrent Run on one session; callers serialize sessions whose provider
// does not (DirectML) and the Guardian's session, as Rust does.

package githubrelevance

import (
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"unsafe"

	"github.com/ebitengine/purego"
)

// ortAPIVersion is the OrtApi version bound, ort's api-23.
const ortAPIVersion = 23

// OrtApi member indices (ORT 1.23.0 onnxruntime_c_api.h).
const (
	ortGetErrorCode                                 = 1
	ortGetErrorMessage                              = 2
	ortCreateEnv                                    = 3
	ortEnableTelemetryEvents                        = 5
	ortCreateSession                                = 7
	ortRun                                          = 9
	ortCreateSessionOptions                         = 10
	ortSetSessionExecutionMode                      = 13
	ortEnableProfiling                              = 14
	ortDisableMemPattern                            = 17
	ortDisableCPUMemArena                           = 19
	ortSetIntraOpNumThreads                         = 24
	ortSetInterOpNumThreads                         = 25
	ortSessionGetOutputCount                        = 31
	ortSessionGetOutputName                         = 37
	ortCreateTensorWithDataAsOrtValue               = 49
	ortCreateCPUMemoryInfo                          = 69
	ortGetTensorMutableData                         = 51
	ortGetTensorElementType                         = 60
	ortGetDimensionsCount                           = 61
	ortGetDimensions                                = 62
	ortGetTensorShapeElementCount                   = 64
	ortGetTensorTypeAndShape                        = 65
	ortGetValueType                                 = 67
	ortAllocatorFree                                = 76
	ortGetAllocatorWithDefaultOptions               = 78
	ortReleaseEnv                                   = 92
	ortReleaseStatus                                = 93
	ortReleaseSession                               = 95
	ortReleaseValue                                 = 96
	ortReleaseTensorTypeAndShapeInfo                = 99
	ortReleaseSessionOptions                        = 100
	ortSessionEndProfiling                          = 110
	ortGetExecutionProviderAPI                      = 195
	ortSessionOptionsAppendExecutionProviderCUDAV2  = 204
	ortCreateCUDAProviderOptions                    = 205
	ortUpdateCUDAProviderOptions                    = 206
	ortReleaseCUDAProviderOptions                   = 208
	ortAddExternalInitializers                      = 210
	ortSessionOptionsAppendExecutionProvider        = 216
	ortSessionOptionsSetEpSelectionPolicy           = 305
	ortDMLSessionOptionsAppendExecutionProviderDML2 = 5 // OrtDmlApi member
)

// ORT enumerations used here.
const (
	ortLoggingLevelVerbose = 0
	ortLoggingLevelInfo    = 1
	ortLoggingLevelWarning = 2
	ortLoggingLevelError   = 3
	ortLoggingLevelFatal   = 4

	ortSequential = 0

	ortExecutionProviderDevicePolicyMaxEfficiency = 5

	onnxTypeTensor = 1

	onnxTensorFloat   = 1
	onnxTensorInt64   = 7
	onnxTensorFloat16 = 10
)

// ortAPI is the loaded library and its OrtApi function table. Loading is
// process-wide in ORT, and so is this (ort::api()).
type ortAPI struct {
	// library is the dlopen/LoadLibrary handle, never closed.
	library uintptr
	// table is the library's const OrtApi*.
	table unsafe.Pointer
	// getErrorMessage is GetErrorMessage, which returns a char*.
	getErrorMessage func(status uintptr) string
	// allocator is the default CPU allocator (GetAllocatorWithDefaultOptions),
	// owned by ORT and never released.
	allocator unsafe.Pointer
	// memoryInfo describes the Go memory a tensor reads: ort's default
	// MemoryInfo (CPU, device allocator, default memory type), made once.
	memoryInfo unsafe.Pointer
}

// ortLoadedAPI is the API bound by loadOrtEnvironment; tensors and sessions
// use it.
var ortLoadedAPI atomic.Pointer[ortAPI]

// ortLoadMu serializes loading (ort's G_ORT_LIB OnceLock).
var ortLoadMu sync.Mutex

// function is the table's member at index.
func (a *ortAPI) function(index int) uintptr {
	return *(*uintptr)(unsafe.Add(a.table, uintptr(index)*unsafe.Sizeof(uintptr(0))))
}

// call calls the member at index, which returns an OrtStatus*, and turns a
// status into an error carrying ORT's message. Pointer arguments converted
// to uintptr in the call stay alive and in place for its duration.
//
//go:uintptrescapes
func (a *ortAPI) call(index int, args ...uintptr) error {
	status, _, _ := purego.SyscallN(a.function(index), args...)
	return a.statusError(status)
}

// callVoid calls a member returning nothing (the Release functions).
//
//go:uintptrescapes
func (a *ortAPI) callVoid(index int, args ...uintptr) {
	purego.SyscallN(a.function(index), args...)
}

// ortError is ort::Error: its Display is ORT's message alone.
type ortError struct {
	code    int32
	message string
}

func (e *ortError) Error() string { return e.message }

// statusError is Error::result_from_status: nil for a null status,
// otherwise the status's message, releasing the status.
func (a *ortAPI) statusError(status uintptr) error {
	if status == 0 {
		return nil
	}
	code, _, _ := purego.SyscallN(a.function(ortGetErrorCode), status)
	message := a.getErrorMessage(status)
	a.callVoid(ortReleaseStatus, status)
	return &ortError{code: int32(code), message: message}
}

// loadOrtAPI loads the ORT library at path once per process and binds
// ORT_API_VERSION (ort::init_from and ort::api()). A later call with
// another path keeps the first library, as ort's G_ORT_LIB does.
func loadOrtAPI(path string) (*ortAPI, error) {
	ortLoadMu.Lock()
	defer ortLoadMu.Unlock()
	if api := ortLoadedAPI.Load(); api != nil {
		return api, nil
	}
	absolute := path
	if !filepath.IsAbs(path) {
		if executable, err := os.Executable(); err == nil {
			relative := filepath.Join(filepath.Dir(executable), path)
			if _, err := os.Stat(relative); err == nil {
				absolute = relative
			}
		}
	}
	library, err := ortOpenLibrary(absolute)
	if err != nil {
		return nil, fmt.Errorf("failed to load from `%s`: %w", absolute, err)
	}
	getBase, err := ortSymbol(library, "OrtGetApiBase")
	if err != nil {
		return nil, fmt.Errorf("%s does not export `OrtGetApiBase`", absolute)
	}
	var apiBase func() unsafe.Pointer
	purego.RegisterFunc(&apiBase, getBase)
	base := apiBase()
	if base == nil {
		return nil, errors.New("OrtGetApiBase returned null")
	}
	// struct OrtApiBase { GetApi; GetVersionString; }
	var getVersionString func() string
	purego.RegisterFunc(&getVersionString, *(*uintptr)(unsafe.Add(base, unsafe.Sizeof(uintptr(0)))))
	version := getVersionString()
	minor := uint64(0)
	if parts := strings.Split(version, "."); len(parts) > 1 {
		minor, _ = strconv.ParseUint(parts[1], 10, 32)
	}
	if minor < ortAPIVersion {
		return nil, fmt.Errorf("ort 2.0.0-rc.15 is not compatible with the ONNX Runtime binary found at `%s`; expected version >= '1.%d.x', but got '%s'", absolute, ortAPIVersion, version)
	}
	var getAPI func(version uint32) unsafe.Pointer
	purego.RegisterFunc(&getAPI, *(*uintptr)(base))
	table := getAPI(ortAPIVersion)
	if table == nil {
		return nil, errors.New("Failed to initialize ORT API")
	}
	api := &ortAPI{library: library, table: table}
	purego.RegisterFunc(&api.getErrorMessage, api.function(ortGetErrorMessage))
	if err := api.call(ortGetAllocatorWithDefaultOptions, uintptr(unsafe.Pointer(&api.allocator))); err != nil {
		return nil, err
	}
	// OrtDeviceAllocator, OrtMemTypeDefault.
	if err := api.call(ortCreateCPUMemoryInfo, 0, 0, uintptr(unsafe.Pointer(&api.memoryInfo))); err != nil {
		return nil, err
	}
	ortLoadedAPI.Store(api)
	return api, nil
}

// ortEnvironment is ort's Environment: the process's OrtEnv.
type ortEnvironment struct {
	api *ortAPI
	// env is the OrtEnv*.
	env unsafe.Pointer
}

// ortDefaultLogLevel is ort's default_log_level: ORT_LOG chooses, ERROR
// otherwise. Without ort's tracing feature, ORT's own logger writes to
// standard error at this level.
func ortDefaultLogLevel() uintptr {
	switch os.Getenv("ORT_LOG") {
	case "fatal":
		return ortLoggingLevelFatal
	case "error":
		return ortLoggingLevelError
	case "warning":
		return ortLoggingLevelWarning
	case "info":
		return ortLoggingLevelInfo
	case "verbose":
		return ortLoggingLevelVerbose
	}
	return ortLoggingLevelError
}

// loadOrtEnvironment loads the ORT library at path, binds its API and
// creates the environment (ort::init_from(path).build(): the "default"
// environment with telemetry left enabled).
func loadOrtEnvironment(path string) (*ortEnvironment, error) {
	api, err := loadOrtAPI(path)
	if err != nil {
		return nil, err
	}
	name := cString("default")
	var env unsafe.Pointer
	if err := api.call(ortCreateEnv, ortDefaultLogLevel(), uintptr(unsafe.Pointer(&name[0])), uintptr(unsafe.Pointer(&env))); err != nil {
		return nil, err
	}
	if env == nil {
		return nil, errors.New("CreateEnv returned null")
	}
	if err := api.call(ortEnableTelemetryEvents, uintptr(env)); err != nil {
		api.callVoid(ortReleaseEnv, uintptr(env))
		return nil, err
	}
	environment := &ortEnvironment{api: api, env: env}
	runtime.AddCleanup(environment, func(env unsafe.Pointer) { api.callVoid(ortReleaseEnv, uintptr(env)) }, env)
	return environment, nil
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

// newOrtSessionBuilder is SessionBuilder::new: new options preferring
// efficient devices, as ort's api-22 builder sets them.
func newOrtSessionBuilder(environment *ortEnvironment) (*ortSessionBuilder, error) {
	api := environment.api
	var options unsafe.Pointer
	if err := api.call(ortCreateSessionOptions, uintptr(unsafe.Pointer(&options))); err != nil {
		return nil, err
	}
	if options == nil {
		return nil, errors.New("CreateSessionOptions returned null")
	}
	builder := &ortSessionBuilder{environment: environment, options: options}
	runtime.AddCleanup(builder, func(options unsafe.Pointer) { api.callVoid(ortReleaseSessionOptions, uintptr(options)) }, options)
	// ort ignores a failure here.
	_ = api.call(ortSessionOptionsSetEpSelectionPolicy, uintptr(options), ortExecutionProviderDevicePolicyMaxEfficiency)
	return builder, nil
}

func (b *ortSessionBuilder) api() *ortAPI { return b.environment.api }

func (b *ortSessionBuilder) withIntraThreads(n int) error {
	defer runtime.KeepAlive(b)
	return b.api().call(ortSetIntraOpNumThreads, uintptr(b.options), uintptr(int32(n)))
}

func (b *ortSessionBuilder) withInterThreads(n int) error {
	defer runtime.KeepAlive(b)
	return b.api().call(ortSetInterOpNumThreads, uintptr(b.options), uintptr(int32(n)))
}

// withMemoryPattern is only ever asked to disable the pattern here.
func (b *ortSessionBuilder) withMemoryPattern(enabled bool) error {
	if enabled {
		return errors.New("memory pattern enabling is not bound")
	}
	defer runtime.KeepAlive(b)
	return b.api().call(ortDisableMemPattern, uintptr(b.options))
}

// withParallelExecution is only ever asked for sequential execution here.
func (b *ortSessionBuilder) withParallelExecution(enabled bool) error {
	if enabled {
		return errors.New("parallel execution is not bound")
	}
	defer runtime.KeepAlive(b)
	return b.api().call(ortSetSessionExecutionMode, uintptr(b.options), ortSequential)
}

// withExecutionProviders is apply_execution_providers: register each
// provider in order; a failure is an error only for a provider that asks
// for one, and is otherwise logged and skipped.
func (b *ortSessionBuilder) withExecutionProviders(providers ...ortExecutionProvider) error {
	defer runtime.KeepAlive(b)
	for _, provider := range providers {
		// ort reports a skipped provider through tracing, which Mjolnir
		// builds it without, so the skip is silent there too.
		if err := b.register(provider); err != nil && provider.errorOnFailure {
			return err
		}
	}
	return nil
}

// register is each ort::ep provider's ExecutionProvider::register.
func (b *ortSessionBuilder) register(provider ortExecutionProvider) error {
	api := b.api()
	switch provider.name {
	case ortCPUProvider:
		// ort::ep::CPU::default() leaves the arena allocator off.
		return api.call(ortDisableCPUMemArena, uintptr(b.options))
	case ortCUDAProvider:
		var cuda unsafe.Pointer
		if err := api.call(ortCreateCUDAProviderOptions, uintptr(unsafe.Pointer(&cuda))); err != nil {
			return err
		}
		defer api.callVoid(ortReleaseCUDAProviderOptions, uintptr(cuda))
		keys, values, pinned := optionArrays(provider.options)
		defer pinned.Unpin()
		if err := api.call(ortUpdateCUDAProviderOptions, uintptr(cuda), keys, values, uintptr(len(provider.options))); err != nil {
			return err
		}
		return api.call(ortSessionOptionsAppendExecutionProviderCUDAV2, uintptr(b.options), uintptr(cuda))
	case ortDirectMLProvider:
		name := cString("DML")
		var dml unsafe.Pointer
		if err := api.call(ortGetExecutionProviderAPI, uintptr(unsafe.Pointer(&name[0])), 0, uintptr(unsafe.Pointer(&dml))); err != nil || dml == nil {
			return errors.New("DirectML is not supported in this build of ONNX Runtime")
		}
		// OrtDmlDeviceOptions { Preference: Default, Filter: Gpu }.
		deviceOptions := &[2]uint32{0, 1}
		append2 := *(*uintptr)(unsafe.Add(dml, ortDMLSessionOptionsAppendExecutionProviderDML2*unsafe.Sizeof(uintptr(0))))
		status, _, _ := purego.SyscallN(append2, uintptr(b.options), uintptr(unsafe.Pointer(deviceOptions)))
		runtime.KeepAlive(deviceOptions)
		return api.statusError(status)
	default:
		name := cString(provider.name)
		keys, values, pinned := optionArrays(provider.options)
		defer pinned.Unpin()
		return api.call(ortSessionOptionsAppendExecutionProvider, uintptr(b.options), uintptr(unsafe.Pointer(&name[0])),
			keys, values, uintptr(len(provider.options)))
	}
}

// withProfiling enables profiling into files starting with prefix.
func (b *ortSessionBuilder) withProfiling(prefix string) error {
	defer runtime.KeepAlive(b)
	path := ortPathChars(prefix)
	return b.api().call(ortEnableProfiling, uintptr(b.options), uintptr(path.pointer()))
}

func (b *ortSessionBuilder) withExternalInitializer(name string, value *ortValue) error {
	defer runtime.KeepAlive(b)
	defer runtime.KeepAlive(value)
	text := cString(name)
	var pinner runtime.Pinner
	defer pinner.Unpin()
	pinner.Pin(&text[0])
	names := []uintptr{uintptr(unsafe.Pointer(&text[0]))}
	values := []uintptr{uintptr(value.value)}
	if err := b.api().call(ortAddExternalInitializers, uintptr(b.options), uintptr(unsafe.Pointer(&names[0])), uintptr(unsafe.Pointer(&values[0])), 1); err != nil {
		return err
	}
	b.initializers = append(b.initializers, value)
	return nil
}

// commitFromFile is SessionBuilder::commit_from_file: the session of the
// model at path, with its output names.
func (b *ortSessionBuilder) commitFromFile(path string) (*ortSession, error) {
	defer runtime.KeepAlive(b)
	if _, err := os.Stat(path); err != nil {
		return nil, fmt.Errorf("File at `%s` does not exist", path)
	}
	api := b.api()
	model := ortPathChars(path)
	var handle unsafe.Pointer
	if err := api.call(ortCreateSession, uintptr(b.environment.env), uintptr(model.pointer()), uintptr(b.options), uintptr(unsafe.Pointer(&handle))); err != nil {
		return nil, err
	}
	runtime.KeepAlive(b.environment)
	if handle == nil {
		return nil, errors.New("Session creation failed with unknown error")
	}
	session := &ortSession{api: api, session: handle, retained: slices.Clone(b.initializers), environment: b.environment}
	// The cleanup's argument holds the retained values, so their own
	// cleanups (which free the memory ORT reads) cannot run before the
	// session is released.
	session.cleanup = runtime.AddCleanup(session, func(owned ortSessionHandle) {
		api.callVoid(ortReleaseSession, uintptr(owned.handle))
		runtime.KeepAlive(owned.retained)
	}, ortSessionHandle{handle: handle, retained: session.retained})
	var count uintptr
	if err := api.call(ortSessionGetOutputCount, uintptr(handle), uintptr(unsafe.Pointer(&count))); err != nil {
		session.release()
		return nil, err
	}
	for i := uintptr(0); i < count; i++ {
		var name unsafe.Pointer
		if err := api.call(ortSessionGetOutputName, uintptr(handle), i, uintptr(api.allocator), uintptr(unsafe.Pointer(&name))); err != nil {
			session.release()
			return nil, err
		}
		session.outputNames = append(session.outputNames, goString(name))
		api.call(ortAllocatorFree, uintptr(api.allocator), uintptr(name))
	}
	return session, nil
}

// ortSession is ort's Session.
type ortSession struct {
	api *ortAPI
	// session is the OrtSession*.
	session unsafe.Pointer
	// retained are values the session reads for its lifetime (external
	// initializers).
	retained []*ortValue
	// environment outlives the session, as ort's session holds it.
	environment *ortEnvironment
	// outputNames are the model's outputs in order.
	outputNames []string
	cleanup     runtime.Cleanup
	released    atomic.Bool
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
		if value != nil {
			value.release()
		}
	}
}

// run is ORT's Run with no run options: the named outputs, or every output
// of the session in its order when outputNames is nil. An output ORT left
// null is nil.
func (s *ortSession) run(inputs []ortInput, outputNames []string) (ortOutputs, error) {
	defer runtime.KeepAlive(s)
	if outputNames == nil {
		outputNames = s.outputNames
	}
	var pinner runtime.Pinner
	defer pinner.Unpin()
	inputNames := make([]uintptr, len(inputs))
	inputValues := make([]uintptr, len(inputs))
	for i, input := range inputs {
		name := cString(input.name)
		pinner.Pin(&name[0])
		inputNames[i] = uintptr(unsafe.Pointer(&name[0]))
		inputValues[i] = uintptr(input.value.value)
	}
	names := make([]uintptr, len(outputNames))
	for i, output := range outputNames {
		name := cString(output)
		pinner.Pin(&name[0])
		names[i] = uintptr(unsafe.Pointer(&name[0]))
	}
	outputs := make([]unsafe.Pointer, len(outputNames))
	err := s.api.call(ortRun, uintptr(s.session), 0,
		uintptr(slicePointer(inputNames)), uintptr(slicePointer(inputValues)), uintptr(len(inputs)),
		uintptr(slicePointer(names)), uintptr(len(outputNames)), uintptr(slicePointer(outputs)))
	for _, input := range inputs {
		runtime.KeepAlive(input.value)
	}
	runtime.KeepAlive(inputNames)
	runtime.KeepAlive(inputValues)
	runtime.KeepAlive(names)
	runtime.KeepAlive(outputs)
	values := make([]*ortValue, len(outputs))
	for i, output := range outputs {
		if output != nil {
			values[i] = s.api.ownValue(output, nil, nil)
		}
	}
	if err != nil {
		ortOutputs{values: values}.release()
		return ortOutputs{}, err
	}
	return ortOutputs{names: outputNames, values: values}, nil
}

// endProfiling ends the session's profile and returns the file it wrote.
func (s *ortSession) endProfiling() (string, error) {
	defer runtime.KeepAlive(s)
	var path unsafe.Pointer
	if err := s.api.call(ortSessionEndProfiling, uintptr(s.session), uintptr(s.api.allocator), uintptr(unsafe.Pointer(&path))); err != nil {
		return "", err
	}
	defer s.api.call(ortAllocatorFree, uintptr(s.api.allocator), uintptr(path))
	return goString(path), nil
}

// ortSessionHandle is what a session's cleanup releases.
type ortSessionHandle struct {
	handle   unsafe.Pointer
	retained []*ortValue
}

// release frees the session now. Its retained values may be shared with
// other sessions (the CoreML graphs share initializers, as Rust shares
// them by Arc), so they are freed by their own cleanups once unreachable.
func (s *ortSession) release() {
	if !s.released.CompareAndSwap(false, true) {
		return
	}
	s.cleanup.Stop()
	s.api.callVoid(ortReleaseSession, uintptr(s.session))
	runtime.KeepAlive(s)
}

// ortValue is an OrtValue tensor.
type ortValue struct {
	api *ortAPI
	// value is the OrtValue*.
	value unsafe.Pointer
	// data is Go memory ORT reads for the value's lifetime, nil when ORT
	// owns the data (Run outputs). pinner pins it until the value is
	// released.
	data     any
	pinner   *runtime.Pinner
	cleanup  runtime.Cleanup
	released atomic.Bool
}

// ortValueHandle is what a value's cleanup releases: the OrtValue, then
// the pin on the Go memory it reads.
type ortValueHandle struct {
	handle unsafe.Pointer
	pinner *runtime.Pinner
}

func (h ortValueHandle) release(api *ortAPI) {
	api.callVoid(ortReleaseValue, uintptr(h.handle))
	if h.pinner != nil {
		h.pinner.Unpin()
	}
}

// ownValue takes ownership of an OrtValue* reading data, pinned by pinner
// (both nil for a value ORT owns, such as a Run output).
func (a *ortAPI) ownValue(handle unsafe.Pointer, data any, pinner *runtime.Pinner) *ortValue {
	value := &ortValue{api: a, value: handle, data: data, pinner: pinner}
	value.cleanup = runtime.AddCleanup(value, func(owned ortValueHandle) { owned.release(a) }, ortValueHandle{handle: handle, pinner: pinner})
	return value
}

// newOrtTensor is Tensor::from_array: a tensor of elementType and shape
// over the Go memory at data (size bytes, the first element of owner),
// which ORT reads in place for the value's lifetime, as ort's tensors read
// their Rust vectors. ORT requires an external initializer's buffer to be
// owned by the caller in just this way.
func newOrtTensor(elementType uintptr, shape []int64, owner any, data unsafe.Pointer, size uintptr) (*ortValue, error) {
	api := ortLoadedAPI.Load()
	if api == nil {
		return nil, errors.New("ONNX Runtime is not loaded")
	}
	for _, dimension := range shape {
		if dimension < 0 {
			return nil, fmt.Errorf("invalid tensor dimension %d", dimension)
		}
	}
	var dims unsafe.Pointer
	if len(shape) > 0 {
		dims = unsafe.Pointer(&shape[0])
	}
	pinner := new(runtime.Pinner)
	if data != nil {
		pinner.Pin(data)
	}
	var handle unsafe.Pointer
	if err := api.call(ortCreateTensorWithDataAsOrtValue, uintptr(api.memoryInfo), uintptr(data), size,
		uintptr(dims), uintptr(len(shape)), elementType, uintptr(unsafe.Pointer(&handle))); err != nil {
		pinner.Unpin()
		return nil, err
	}
	runtime.KeepAlive(shape)
	return api.ownValue(handle, owner, pinner), nil
}

// checkedLength is the element count of shape, which must hold length
// values (ort's Tensor::from_array check).
func checkedLength(shape []int64, length int) error {
	count := int64(1)
	for _, dimension := range shape {
		count *= dimension
	}
	if count != int64(length) {
		return fmt.Errorf("cannot create a tensor from shape %v and %d elements", shape, length)
	}
	return nil
}

// newOrtTensorInt64 is Tensor::from_array((shape, data)) for int64 data.
func newOrtTensorInt64(shape []int64, data []int64) (*ortValue, error) {
	if err := checkedLength(shape, len(data)); err != nil {
		return nil, err
	}
	return newOrtTensor(onnxTensorInt64, shape, data, slicePointer(data), uintptr(len(data))*8)
}

// newOrtTensorFloat16 is Tensor::from_array((shape, data)) for IEEE 754
// half-precision values given as their bits.
func newOrtTensorFloat16(shape []int64, bits []uint16) (*ortValue, error) {
	if err := checkedLength(shape, len(bits)); err != nil {
		return nil, err
	}
	return newOrtTensor(onnxTensorFloat16, shape, bits, slicePointer(bits), uintptr(len(bits))*2)
}

// ortTensorInfo is a tensor's element type, shape and element count
// (GetTensorTypeAndShape).
type ortTensorInfo struct {
	elementType int32
	shape       []int64
	count       int
}

func (v *ortValue) tensorInfo() (ortTensorInfo, error) {
	defer runtime.KeepAlive(v)
	api := v.api
	var info unsafe.Pointer
	if err := api.call(ortGetTensorTypeAndShape, uintptr(v.value), uintptr(unsafe.Pointer(&info))); err != nil {
		return ortTensorInfo{}, err
	}
	if info == nil {
		return ortTensorInfo{}, errors.New("ORT returned no tensor shape")
	}
	defer api.callVoid(ortReleaseTensorTypeAndShapeInfo, uintptr(info))
	var elementType int32
	if err := api.call(ortGetTensorElementType, uintptr(info), uintptr(unsafe.Pointer(&elementType))); err != nil {
		return ortTensorInfo{}, err
	}
	var dimensions uintptr
	if err := api.call(ortGetDimensionsCount, uintptr(info), uintptr(unsafe.Pointer(&dimensions))); err != nil {
		return ortTensorInfo{}, err
	}
	shape := make([]int64, dimensions)
	if dimensions > 0 {
		if err := api.call(ortGetDimensions, uintptr(info), uintptr(unsafe.Pointer(&shape[0])), dimensions); err != nil {
			return ortTensorInfo{}, err
		}
	}
	var count uintptr
	if err := api.call(ortGetTensorShapeElementCount, uintptr(info), uintptr(unsafe.Pointer(&count))); err != nil {
		return ortTensorInfo{}, err
	}
	return ortTensorInfo{elementType: elementType, shape: shape, count: int(count)}, nil
}

// ortElementTypeName is ort's TensorElementType Display.
func ortElementTypeName(elementType int32) string {
	switch elementType {
	case onnxTensorFloat:
		return "f32"
	case onnxTensorInt64:
		return "i64"
	case onnxTensorFloat16:
		return "f16"
	}
	return fmt.Sprintf("element type %d", elementType)
}

// float32Data copies count float32 values out of the tensor.
func (v *ortValue) float32Data(count int) ([]float32, error) {
	defer runtime.KeepAlive(v)
	var data unsafe.Pointer
	if err := v.api.call(ortGetTensorMutableData, uintptr(v.value), uintptr(unsafe.Pointer(&data))); err != nil {
		return nil, err
	}
	if count == 0 {
		return []float32{}, nil
	}
	if data == nil {
		return nil, errors.New("ORT returned no tensor data")
	}
	return append([]float32(nil), unsafe.Slice((*float32)(data), count)...), nil
}

// tensorFloat32 is try_extract_tensor::<f32>: the tensor's shape and a copy
// of its data, an error for a tensor of another element type.
func (v *ortValue) tensorFloat32() ([]int64, []float32, error) {
	var valueType int32
	err := v.api.call(ortGetValueType, uintptr(v.value), uintptr(unsafe.Pointer(&valueType)))
	runtime.KeepAlive(v)
	if err != nil {
		return nil, nil, err
	}
	if valueType != onnxTypeTensor {
		return nil, nil, errors.New("Cannot extract a Tensor<f32> from a non-tensor value")
	}
	info, err := v.tensorInfo()
	if err != nil {
		return nil, nil, err
	}
	if info.elementType != onnxTensorFloat {
		return nil, nil, fmt.Errorf("Cannot extract Tensor<f32> from Tensor<%s>", ortElementTypeName(info.elementType))
	}
	data, err := v.float32Data(info.count)
	if err != nil {
		return nil, nil, err
	}
	return info.shape, data, nil
}

// release frees the value now.
func (v *ortValue) release() {
	if !v.released.CompareAndSwap(false, true) {
		return
	}
	v.cleanup.Stop()
	ortValueHandle{handle: v.value, pinner: v.pinner}.release(v.api)
	runtime.KeepAlive(v)
}

// optionArrays is ExecutionProviderOptions::to_ffi: the keys and values as
// pinned C string arrays, in the options' order.
func optionArrays(options []ortOption) (keys, values uintptr, pinner *runtime.Pinner) {
	pinner = new(runtime.Pinner)
	if len(options) == 0 {
		return 0, 0, pinner
	}
	keyPointers := make([]uintptr, len(options))
	valuePointers := make([]uintptr, len(options))
	for i, option := range options {
		key := cString(option.key)
		value := cString(option.value)
		pinner.Pin(&key[0])
		pinner.Pin(&value[0])
		keyPointers[i] = uintptr(unsafe.Pointer(&key[0]))
		valuePointers[i] = uintptr(unsafe.Pointer(&value[0]))
	}
	pinner.Pin(&keyPointers[0])
	pinner.Pin(&valuePointers[0])
	return uintptr(unsafe.Pointer(&keyPointers[0])), uintptr(unsafe.Pointer(&valuePointers[0])), pinner
}

// cString is text as a NUL-terminated C string.
func cString(text string) []byte {
	return append([]byte(text), 0)
}

// goString copies the NUL-terminated C string at pointer.
func goString(pointer unsafe.Pointer) string {
	if pointer == nil {
		return ""
	}
	length := 0
	for *(*byte)(unsafe.Add(pointer, length)) != 0 {
		length++
	}
	return string(unsafe.Slice((*byte)(pointer), length))
}

// slicePointer is the address of a slice's first element, nil when empty.
func slicePointer[T any](values []T) unsafe.Pointer {
	if len(values) == 0 {
		return nil
	}
	return unsafe.Pointer(&values[0])
}

// isFinite32 reports whether a float32 is neither infinite nor NaN.
func isFinite32(value float32) bool {
	return !math.IsInf(float64(value), 0) && !math.IsNaN(float64(value))
}
