package clap

import (
	"encoding/json"
	"os"
	"testing"
)

func TestScratchDumpRequests(t *testing.T) {
	f, err := os.Create(os.Getenv("CLAP_DUMP"))
	if err != nil {
		t.Skip()
	}
	defer f.Close()
	specCases(func(req any) {
		raw, _ := json.Marshal(req)
		f.Write(raw)
		f.Write([]byte("\n"))
	})
}
