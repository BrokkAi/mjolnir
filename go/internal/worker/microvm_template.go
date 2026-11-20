//go:build linux

// Port of mj-worker/src/microvm/template.rs.
//
// The small subset of Go templates that `--format` needs.
//
// The controller's Podman scripts use `{{.State.Running}}`, `{{.Id}}`,
// `{{json .Config.Labels}}` and `{{index .Labels "key"}}`. Anything else is
// refused by name rather than ignored. (Go's text/template is not used: it
// would accept, and render differently, what Rust refuses.)
//
// Values are JSON values as github.com/BrokkAi/acp-go/jsonvalue holds them
// (serde_json::Value with preserve_order): nil, bool, string, int64,
// uint64, float64, []any or jsonvalue.Object.

package worker

// MicrovmTemplateRender renders template against value, followed by a
// newline like Podman.
func MicrovmTemplateRender(template string, value any) (string, error) {
	panic("unported: mj-worker/src/microvm/template.rs render")
}

func microvmTemplateAction(text string, root any) (string, error) {
	panic("unported: mj-worker/src/microvm/template.rs action")
}

func microvmTemplateSplitWords(text string) ([]string, error) {
	panic("unported: mj-worker/src/microvm/template.rs split_words")
}

func microvmTemplateLookup(root any, path string) (any, error) {
	panic("unported: mj-worker/src/microvm/template.rs lookup")
}

func microvmTemplateScalar(value any) (string, error) {
	panic("unported: mj-worker/src/microvm/template.rs scalar")
}
