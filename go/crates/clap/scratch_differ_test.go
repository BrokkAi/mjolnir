package clap

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"testing"
)

func scratchDecode(b []byte) any {
	d := json.NewDecoder(bytes.NewReader(b))
	d.UseNumber()
	var v any
	d.Decode(&v)
	return v
}

func scratchDiff(path string, a, b any, out *[]string) {
	if len(*out) > 3 {
		return
	}
	switch av := a.(type) {
	case map[string]any:
		bv, ok := b.(map[string]any)
		if !ok {
			*out = append(*out, fmt.Sprintf("%s: type rust=%T go=%T", path, a, b))
			return
		}
		for k := range av {
			if !reflect.DeepEqual(av[k], bv[k]) {
				scratchDiff(path+"."+k, av[k], bv[k], out)
			}
		}
		for k := range bv {
			if _, ok := av[k]; !ok {
				*out = append(*out, fmt.Sprintf("%s.%s: missing in rust, go=%v", path, k, bv[k]))
			}
		}
	case []any:
		bv, ok := b.([]any)
		if !ok || len(av) != len(bv) {
			*out = append(*out, fmt.Sprintf("%s: rust=%.3000v\n   go=%.3000v", path, a, b))
			return
		}
		for i := range av {
			if !reflect.DeepEqual(av[i], bv[i]) {
				scratchDiff(fmt.Sprintf("%s[%d]", path, i), av[i], bv[i], out)
			}
		}
	default:
		*out = append(*out, fmt.Sprintf("%s:\nRUST: %q\nGO:   %q", path, fmt.Sprint(a), fmt.Sprint(b)))
	}
}

func TestScratchDiffer(t *testing.T) {
	reqs, err := os.Open(os.Getenv("CLAP_REQS"))
	if err != nil {
		t.Skip()
	}
	replies, _ := os.Open(os.Getenv("CLAP_REPLIES"))
	rq := bufio.NewScanner(reqs)
	rq.Buffer(make([]byte, 1<<24), 1<<24)
	rp := bufio.NewScanner(replies)
	rp.Buffer(make([]byte, 1<<24), 1<<24)
	n, bad := 0, 0
	for rq.Scan() && rp.Scan() {
		n++
		req := append([]byte(nil), rq.Bytes()...)
		goReply, _ := json.Marshal(answerSpec(req))
		r, g := scratchDecode(rp.Bytes()), scratchDecode(goReply)
		if reflect.DeepEqual(r, g) {
			continue
		}
		bad++
		if bad > 6 {
			continue
		}
		var out []string
		scratchDiff("", r, g, &out)
		// find which argv
		var parsed struct {
			Argvs [][]string `json:"argvs"`
		}
		json.Unmarshal(req, &parsed)
		fmt.Printf("=== request %d\n", n)
		for _, o := range out {
			fmt.Println(o)
		}
		var idx int
		if _, err := fmt.Sscanf(firstIndex(out), ".results[%d]", &idx); err == nil && idx < len(parsed.Argvs) {
			fmt.Printf("argv: %q\n", parsed.Argvs[idx])
		}
		os.WriteFile(fmt.Sprintf("/tmp/claptry/bad_%d.json", bad), req, 0o644)
	}
	fmt.Printf("compared=%d bad=%d\n", n, bad)
}

func firstIndex(out []string) string {
	if len(out) == 0 {
		return ""
	}
	return out[0]
}
