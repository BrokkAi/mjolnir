// Port of std 1.99.1 tests/env_modify.rs
//
// These tests are in a separate integration test as they modify the
// environment, and would otherwise cause some other tests to fail. (Go runs
// the tests of a package one at a time, and none here calls t.Parallel.)

package rsenv

import (
	"math/rand/v2"
	"os"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// makeRandName is make_rand_name: `TEST` and ten random alphanumerics, a name
// no variable has.
func makeRandName(t *testing.T) string {
	t.Helper()
	const alphanumeric = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
	var n strings.Builder
	n.WriteString("TEST")
	for range 10 {
		n.WriteByte(alphanumeric[rand.IntN(len(alphanumeric))])
	}
	if VarOs(n.String()).IsSome() {
		t.Fatalf("%s is set", n.String())
	}
	return n.String()
}

func eq(t *testing.T, a, b rs.Option[string]) {
	t.Helper()
	if a != b {
		t.Errorf("%s != %s", rs.DebugString(a), rs.DebugString(b))
	}
}

func TestItEnvModify_test_set_var(t *testing.T) {
	n := makeRandName(t)
	SetVar(n, "VALUE")
	eq(t, VarOs(n), rs.Some("VALUE"))
}

func TestItEnvModify_test_remove_var(t *testing.T) {
	n := makeRandName(t)
	SetVar(n, "VALUE")
	RemoveVar(n)
	eq(t, VarOs(n), rs.None[string]())
}

func TestItEnvModify_test_set_var_overwrite(t *testing.T) {
	n := makeRandName(t)
	SetVar(n, "1")
	SetVar(n, "2")
	eq(t, VarOs(n), rs.Some("2"))
	SetVar(n, "")
	eq(t, VarOs(n), rs.Some(""))
}

func TestItEnvModify_test_var_big(t *testing.T) {
	s := ""
	i := 0
	for i < 100 {
		s += "aaaaaaaaaa"
		i++
	}
	n := makeRandName(t)
	SetVar(n, s)
	eq(t, VarOs(n), rs.Some(s))
}

func TestItEnvModify_test_env_set_get_huge(t *testing.T) {
	n := makeRandName(t)
	s := strings.Repeat("x", 10000)
	SetVar(n, s)
	eq(t, VarOs(n), rs.Some(s))
	RemoveVar(n)
	eq(t, VarOs(n), rs.None[string]())
}

func TestItEnvModify_test_env_set_var(t *testing.T) {
	n := makeRandName(t)

	e := VarsOs()
	SetVar(n, "VALUE")
	for k, v := range e.All() {
		if k == n && v == "VALUE" {
			t.Errorf("a snapshot taken before set_var holds %s", n)
		}
	}

	found := false
	for k, v := range VarsOs().All() {
		if k == n && v == "VALUE" {
			found = true
			break
		}
	}
	if !found {
		t.Errorf("vars_os does not hold %s", n)
	}
}

func TestItEnvModify_env_home_dir(t *testing.T) {
	varToOsString := func(value string, err error) rs.Option[string] {
		switch err := err.(type) {
		case nil:
			return rs.Some(value)
		case *VarError_NotUnicode:
			return rs.Some(err.F0)
		default:
			return rs.None[string]()
		}
	}

	if rspath.Host == rspath.Unix {
		oldhome := varToOsString(Var("HOME"))

		SetVar("HOME", "/home/MountainView")
		eq(t, HomeDir(), rs.Some("/home/MountainView"))

		RemoveVar("HOME")
		// When HOME is not set, some platforms return `None`,
		// but others return `Some` with a default.
		// Just check that it is not "/home/MountainView".
		if HomeDir() == rs.Some("/home/MountainView") {
			t.Errorf("home_dir is still /home/MountainView")
		}

		if home, ok := oldhome.Get(); ok {
			SetVar("HOME", home)
		}
	} else {
		oldhome := varToOsString(Var("HOME"))
		olduserprofile := varToOsString(Var("USERPROFILE"))

		RemoveVar("HOME")
		RemoveVar("USERPROFILE")

		if HomeDir().IsNone() {
			t.Errorf("home_dir is None")
		}

		SetVar("HOME", "/home/PaloAlto")
		if HomeDir() == rs.Some("/home/PaloAlto") {
			t.Errorf("HOME must not be used")
		}

		SetVar("USERPROFILE", "/home/MountainView")
		eq(t, HomeDir(), rs.Some("/home/MountainView"))

		RemoveVar("HOME")

		eq(t, HomeDir(), rs.Some("/home/MountainView"))

		SetVar("USERPROFILE", "")
		if HomeDir() == rs.Some("") {
			t.Errorf("Empty USERPROFILE must be ignored")
		}

		RemoveVar("USERPROFILE")

		if home, ok := oldhome.Get(); ok {
			SetVar("HOME", home)
		}
		if profile, ok := olduserprofile.Get(); ok {
			SetVar("USERPROFILE", profile)
		}
	}
}

// miri shouldn't detect any data race in this fn
func TestItEnvModify_test_env_get_set_multithreaded(t *testing.T) {
	// #[cfg_attr(any(not(miri), target_os = "emscripten"), ignore)]: Rust
	// runs it only under Miri.
	if os.Getenv("MJ_RUN_IGNORED") != "1" {
		t.Skip("ignored: Rust runs this test only under Miri")
	}
	var threads sync.WaitGroup
	threads.Go(func() {
		for range 100 {
			_ = VarOs("foo")
		}
	})
	threads.Go(func() {
		for range 100 {
			SetVar("foo", "bar")
		}
	})
	threads.Wait()
}
