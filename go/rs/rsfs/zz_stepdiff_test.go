//go:build unix

package rsfs_test

// TEMPORARY: step-level differences against the recorded corpus. Delete
// before finishing.

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestStepDiff(t *testing.T) {
	corpus := os.Getenv("MJ_RSFS_STEPS")
	if corpus == "" {
		t.Skip("set MJ_RSFS_STEPS to a corpus name")
	}
	file, err := os.Open(filepath.Join("testdata", "oracle", corpus+".jsonl.gz"))
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	unpacked, _ := gzip.NewReader(file)
	reader := bufio.NewReaderSize(unpacked, 1<<20)
	index := 0
	shown := 0
	for {
		line, err := reader.ReadBytes('\n')
		if len(bytes.TrimSpace(line)) == 0 {
			break
		}
		var c struct {
			Request json.RawMessage `json:"request"`
			Reply   json.RawMessage `json:"reply"`
		}
		if err := json.Unmarshal(line, &c); err != nil {
			t.Fatal(err)
		}
		got, _ := json.Marshal(answerRsfs(c.Request))
		var rust, gov struct{ Results []any }
		dec := func(b []byte, v any) { d := json.NewDecoder(bytes.NewReader(b)); d.UseNumber(); d.Decode(v) }
		dec(c.Reply, &rust)
		dec(got, &gov)
		req := decodeRequest(c.Request)
		steps := req["steps"].([]any)
		for i := range steps {
			var r, g any
			if i < len(rust.Results) {
				r = rust.Results[i]
			}
			if i < len(gov.Results) {
				g = gov.Results[i]
			}
			if !reflect.DeepEqual(r, g) {
				shown++
				if shown <= 60 {
					sj, _ := json.Marshal(steps[i])
					rj, _ := json.Marshal(r)
					gj, _ := json.Marshal(g)
					step := steps[i].(map[string]any)
					fmt.Printf("case %d step %d %s p=%q q=%q\n  step: %s\n  rust: %s\n  go:   %s\n", index, i, step["do"],
						string(decodeBytes(step["p"])), string(decodeBytes(step["q"])), trim(string(sj)), trim(string(rj)), trim(string(gj)))
				}
			}
		}
		if len(rust.Results) != len(gov.Results) {
			fmt.Printf("case %d: %d rust results, %d go results; reply %s\n", index, len(rust.Results), len(gov.Results), trim(string(c.Reply)))
		}
		index++
		if err != nil {
			break
		}
	}
	fmt.Printf("%d differing steps\n", shown)
}

func trim(s string) string {
	if len(s) > 400 {
		return s[:400] + "..."
	}
	return s
}
