// Port of the tests in mj-client/src/build_identity.rs.

package client

import "testing"

const (
	buildIdentityOld = "6a2a07136a2a07136a2a07136a2a07136a2a0713"
	buildIdentityNew = "19485f1719485f1719485f1719485f1719485f17"
)

func buildIdentityBuild(t *testing.T, published string) BuildIdentity {
	t.Helper()
	identity, err := BuildIdentityParse(published)
	if err != nil {
		t.Fatalf("parse %q: %v", published, err)
	}
	return identity
}

// Hard-won: d7250ce22765: U-1: same-release builds displaced a newer daemon on alternate commands
func TestSameReleaseBuildsAreOrderedByCommitThenByFileTime(t *testing.T) {
	client := buildIdentityBuild(t, "2.24.0+"+buildIdentityOld+".c100.m5000")
	order := func(daemon string) DaemonBuildOrder {
		built := buildIdentityBuild(t, daemon)
		return CompareDaemonBuild(&built, &client)
	}
	for _, check := range []struct {
		daemon string
		want   DaemonBuildOrder
	}{
		// A newer commit wins whatever the file times say.
		{"2.24.0+" + buildIdentityNew + ".c200.m1", DaemonBuildOrderNewer},
		{"2.24.0+" + buildIdentityNew + ".c50.m9999", DaemonBuildOrderOlder},
		// One revision: the later file is the later build.
		{"2.24.0+" + buildIdentityOld + ".c100.m6000", DaemonBuildOrderNewer},
		{"2.24.0+" + buildIdentityOld + ".c100.m4000", DaemonBuildOrderOlder},
		{"2.24.0+" + buildIdentityOld + ".c100.m5000", DaemonBuildOrderSame},
		// Releases come first.
		{"2.25.0+" + buildIdentityNew + ".c1", DaemonBuildOrderNewer},
		{"2.23.0", DaemonBuildOrderOlder},
	} {
		if got := order(check.daemon); got != check.want {
			t.Errorf("order(%q) = %v, want %v", check.daemon, got, check.want)
		}
	}
}

func TestADaemonWithoutAPublishedBuildIsOlderAndMissingFactsAreUnknown(t *testing.T) {
	client := buildIdentityBuild(t, "2.24.0+"+buildIdentityOld+".c100.m5000")
	order := func(daemon string) DaemonBuildOrder {
		built := buildIdentityBuild(t, daemon)
		return CompareDaemonBuild(&built, &client)
	}
	if got := order("2.24.0"); got != DaemonBuildOrderOlder {
		t.Errorf("plain version = %v", got)
	}
	// Different revisions without both commit times, or committed in the
	// same second, cannot be ordered.
	if got := order("2.24.0+" + buildIdentityNew + ".m9999"); got != DaemonBuildOrderUnknown {
		t.Errorf("no commit time = %v", got)
	}
	if got := order("2.24.0+" + buildIdentityNew + ".c100"); got != DaemonBuildOrderUnknown {
		t.Errorf("same commit second = %v", got)
	}
	clientWithoutTime := buildIdentityBuild(t, "2.24.0+"+buildIdentityOld+".m5000")
	daemon := buildIdentityBuild(t, "2.24.0+"+buildIdentityNew+".c1")
	if got := CompareDaemonBuild(&daemon, &clientWithoutTime); got != DaemonBuildOrderUnknown {
		t.Errorf("client without time = %v", got)
	}
	// One revision without both file times.
	if got := order("2.24.0+" + buildIdentityOld + ".c100"); got != DaemonBuildOrderUnknown {
		t.Errorf("one revision without file time = %v", got)
	}
}
