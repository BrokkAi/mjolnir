// Port of mj-github-relevance/src/github_student/concurrent.rs.

// Concurrent CPU Run adapter. No session configuration is changed after
// loading.
//
// Rust calls ORT's raw C API here (its Output and Shape guards release the
// per-call output and its metadata, and checked turns a status into an
// error) because ort's safe Run needs exclusive access. The Go binding in
// onnxruntime.go owns those releases and status checks, and its run is the
// raw Run, so only score remains.

package githubrelevance

import (
	"errors"
	"math"
)

// score is the student's probability for one tokenized pair on a CPU
// session. ORT explicitly permits concurrent Run on one immutable session
// (https://onnxruntime.ai/docs/reference/high-level-design.html). Only CPU
// sessions enter this adapter; DirectML sessions are run one at a time
// under their gpu's lock. Tensors, names and outputs are per call; nothing
// else is mutated.
func score(session *ortSession, ids, mask []int64) (float64, error) {
	n := len(ids)
	if n != len(mask) {
		return 0, errors.New("student token/mask length mismatch")
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
	outputs, err := session.run([]ortInput{{name: "input_ids", value: idsTensor}, {name: "attention_mask", value: maskTensor}}, []string{"logits"})
	if err != nil {
		return 0, err
	}
	defer outputs.release()
	output := outputs.values[0]
	if output == nil {
		return 0, errors.New("ORT returned no student logit")
	}
	info, err := output.tensorInfo()
	if err != nil {
		return 0, err
	}
	if info.count != 1 || info.elementType != onnxTensorFloat {
		return 0, errors.New("student output must contain one fp32 logit")
	}
	data, err := output.float32Data(1)
	if err != nil {
		return 0, err
	}
	logit := data[0]
	if !isFinite32(logit) {
		return 0, errors.New("student returned nonfinite logit")
	}
	return 1.0 / (1.0 + math.Exp(-float64(logit))), nil
}
