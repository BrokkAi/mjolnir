// Differential tests against the Rust crate through the oracle
// (go/port/oracle/src/clap_oracle.rs).

package clap

import (
	"encoding/json"
	"os"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// specCases yields the generated commands with their argument vectors and
// renderings. Live runs (MJ_ORACLE=live) compare ten times as many as the
// recorded corpus holds.
func specCases(yield func(any)) {
	commands := 160
	if os.Getenv("MJ_ORACLE") == "live" {
		commands = 1600
	}
	for seed := 0; seed < commands; seed++ {
		g := newSpecGen(uint64(seed) + 1)
		spec, model := g.genCommand("prog", 0)
		env := g.genEnv()
		var argvs [][]string
		for i := 0; i < 18; i++ {
			if i%3 == 0 {
				argvs = append(argvs, g.genValidArgv(model))
			} else {
				argvs = append(argvs, g.genArgv(model))
			}
		}
		yield(map[string]any{"op": "runs", "cmd": spec, "env": env, "argvs": argvs})
		if seed%4 == 0 {
			for _, what := range []string{"help", "long_help", "usage", "version"} {
				yield(map[string]any{"op": "render", "cmd": spec, "env": env, "path": []string{}, "what": what})
			}
			if len(model.subs) > 0 {
				path := []string{model.subs[0].name}
				yield(map[string]any{"op": "render", "cmd": spec, "env": env, "path": path, "what": "help"})
				yield(map[string]any{"op": "render", "cmd": spec, "env": env, "path": path, "what": "usage"})
			}
		}
	}
}

func TestDiffAgainstRust(t *testing.T) {
	oracle.Check(t, "clap", "spec", specCases, func(request json.RawMessage) any {
		return answerSpec(request)
	})
}
