// Port of the tests in mj-core/src/move_workspace.rs.

package core

import "testing"

func moveWorkspaceTestFile(path string, bytes uint64) WorkspaceFile {
	return WorkspaceFile{Location: WorkspacePath{Repository: "bifrost", Path: path}, Bytes: bytes}
}

func TestLargeTransferConsentStartsAtOneDecimalGigabyte(t *testing.T) {
	assessment := WorkspaceAssessment{RequiredBytes: LargeTransferBytes - 1}
	var selection WorkspaceSelection
	if err := selection.Validate(&assessment); err != nil {
		t.Fatal(err)
	}
	assessment.RequiredBytes++
	if selection.Validate(&assessment) == nil {
		t.Fatal("a decimal gigabyte passed without consent")
	}
	selection.AcknowledgeLargeTransfer = true
	if err := selection.Validate(&assessment); err != nil {
		t.Fatal(err)
	}
	assessment.RequiredBytes++
	if err := selection.Validate(&assessment); err != nil {
		t.Fatal(err)
	}
}

func TestExclusionsCannotNameTrackedOrParentPaths(t *testing.T) {
	assessment := WorkspaceAssessment{Files: []WorkspaceFile{moveWorkspaceTestFile("output/binary", 42)}}
	for _, path := range []string{"../output", "/output", "tracked.rs"} {
		selection := WorkspaceSelection{Exclusions: []WorkspacePath{moveWorkspaceTestFile(path, 0).Location}}
		if selection.Validate(&assessment) == nil {
			t.Fatalf("accepted exclusion %q", path)
		}
	}
}
