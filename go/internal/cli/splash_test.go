// Port of the tests in mj-cli/src/splash.rs.

package cli

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 5d41d8daac: Finding T-1 showed configured ASCII symbols still displayed the Unicode splash; the test checks the ASCII path and safe handling of unreadable config.
func TestConfiguredAsciiSymbolsSkipTheSplashAndUnreadableConfigDoesNotPanic(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	write := func(body string) {
		t.Helper()
		if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write(fmt.Sprintf("version = %d\n[advanced]\nsymbols = \"ascii\"\n", core.ConfigVersion))
	if got := symbolsForSplash(path); got != core.SymbolSetAscii {
		t.Fatalf("ascii config: %v", got)
	}
	write(fmt.Sprintf("version = %d\n[advanced]\nsymbols = \"unicode\"\n", core.ConfigVersion))
	if got := symbolsForSplash(path); got != core.SymbolSetUnicode {
		t.Fatalf("unicode config: %v", got)
	}
	write("not [valid toml")
	if got, want := symbolsForSplash(path), chat.SymbolsFor(nil); got != want {
		t.Fatalf("unreadable config: %v, want %v", got, want)
	}
}
