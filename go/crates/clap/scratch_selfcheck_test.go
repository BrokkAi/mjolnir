package clap

import (
	"encoding/json"
	"fmt"
	"testing"
)

func TestScratchSelfCheck(t *testing.T) {
	panics := 0
	total := 0
	specCases(func(req any) {
		raw, _ := json.Marshal(req)
		reply := answerSpec(raw)
		total++
		check := func(r any) {
			if m, ok := r.(map[string]string); ok && m["panic"] != "" {
				panics++
				if panics < 15 {
					fmt.Printf("PANIC: %s\n  req: %.600s\n", m["panic"], raw)
				}
			}
			if m, ok := r.(map[string]any); ok && m["oracle_error"] != nil {
				fmt.Printf("ORACLE_ERROR: %v\n", m["oracle_error"])
			}
		}
		check(reply)
		if m, ok := reply.(map[string]any); ok {
			if results, ok := m["results"].([]any); ok {
				for _, r := range results {
					check(r)
				}
			}
		}
	})
	fmt.Printf("requests=%d panics=%d\n", total, panics)
}
