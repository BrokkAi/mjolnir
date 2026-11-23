// Port of mj-store/src/database/projects.rs.

// Durable catalog decisions share the controller's serialized database
// writer.

package store

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type ProjectDiscoveryChange struct {
	Sequence  int64
	SessionID string
	Directory string
	TargetID  string
}

func ReadProjectCatalog() (core.ProjectCatalogView, error) {
	panic("unported: mj-store/src/database/projects.rs read_project_catalog")
}

func readProjectCatalogFrom(conn Queryer) (core.ProjectCatalogView, error) {
	panic("unported: mj-store/src/database/projects.rs read_project_catalog_from")
}

func StoreProjectLocation(location *core.ProjectLocation) error {
	panic("unported: mj-store/src/database/projects.rs store_project_location")
}

// StoreCatalogProject records a configured project. Existing choices win
// after first reconciliation. Callers order new entries by reference count
// then ID so a fresh catalog chooses the least disruptive ID.
func StoreCatalogProject(bundleID string, project *core.ProjectBundleSnapshot, configured bool) (string, error) {
	panic("unported: mj-store/src/database/projects.rs store_catalog_project")
}

func storeCatalogProjectAt(path, bundleID string, project *core.ProjectBundleSnapshot, configured bool) (string, error) {
	panic("unported: mj-store/src/database/projects.rs store_catalog_project_at")
}

// retireChangedProject moves the old shape of an edited TOML ID aside. An
// edited TOML ID can describe a new shape. Existing sessions retain the old
// shape and a scoped alias, so their workers cannot write new-project
// history.
func retireChangedProject(tx *Transaction, id, newKey string) error {
	panic("unported: mj-store/src/database/projects.rs retire_changed_project")
}

// ProjectAlias is one pending project alias (Rust's (String, String,
// ProjectBundleSnapshot)): the alias id, its canonical id and its snapshot.
type ProjectAlias struct {
	BundleID    string
	CanonicalID string
	Project     core.ProjectBundleSnapshot
}

func PendingProjectAliases() ([]ProjectAlias, error) {
	panic("unported: mj-store/src/database/projects.rs pending_project_aliases")
}

func FinishProjectAlias(bundleID string) error {
	panic("unported: mj-store/src/database/projects.rs finish_project_alias")
}

func sessionProjectID(conn Queryer, sessionID, bundleID string) (string, error) {
	panic("unported: mj-store/src/database/projects.rs session_project_id")
}

func canonicalProjectID(conn Queryer, bundleID string) (string, error) {
	panic("unported: mj-store/src/database/projects.rs canonical_project_id")
}

func ProjectSeeded(harness string, home string) (bool, error) {
	panic("unported: mj-store/src/database/projects.rs project_seeded")
}

func FinishProjectSeed(harness string, home string) error {
	panic("unported: mj-store/src/database/projects.rs finish_project_seed")
}

func ProjectDiscoveryChanges(retry bool) ([]ProjectDiscoveryChange, error) {
	panic("unported: mj-store/src/database/projects.rs project_discovery_changes")
}

func FinishProjectDiscovery(sequence int64, err *string) error {
	panic("unported: mj-store/src/database/projects.rs finish_project_discovery")
}

// StoreDirectoryProject allocates a catalog ID and decides reuse inside the
// serialized writer.
func StoreDirectoryProject(project *core.ProjectBundleSnapshot) (string, error) {
	panic("unported: mj-store/src/database/projects.rs store_directory_project")
}

func BindSessionProject(sessionID, bundleID string, project *core.ProjectBundleSnapshot) error {
	panic("unported: mj-store/src/database/projects.rs bind_session_project")
}

func SeedFailure(harness string, home string, directory string, sourceFile bool, err *string) error {
	panic("unported: mj-store/src/database/projects.rs seed_failure")
}

// ProjectSeedFailure is one recorded seed failure (Rust's (String, PathBuf,
// PathBuf, bool)).
type ProjectSeedFailure struct {
	Harness    string
	Home       string
	Directory  string
	SourceFile bool
}

func SeedFailures() ([]ProjectSeedFailure, error) {
	panic("unported: mj-store/src/database/projects.rs seed_failures")
}

func DiscoveryErrors() ([]string, error) {
	panic("unported: mj-store/src/database/projects.rs discovery_errors")
}

// SavedProject is the canonical id and snapshot a bundle id names; ok is
// false when the catalog has neither (Rust's Option<(String,
// ProjectBundleSnapshot)>).
func SavedProject(bundleID string) (canonical string, project core.ProjectBundleSnapshot, ok bool, err error) {
	panic("unported: mj-store/src/database/projects.rs saved_project")
}

func HideCatalogProject(bundleID string) error {
	panic("unported: mj-store/src/database/projects.rs hide_catalog_project")
}

// SetupStyleID is a project name reduced to the characters a bundle id may
// contain.
func SetupStyleID(value string) string {
	id := make([]rune, 0, 64)
	for _, character := range value {
		if len(id) == 64 {
			break
		}
		if character < 0x80 && (character >= '0' && character <= '9' || character >= 'a' && character <= 'z' || character >= 'A' && character <= 'Z' || character == '-' || character == '_' || character == '.') {
			id = append(id, character)
		}
	}
	if len(id) == 0 || string(id) == "." || string(id) == ".." {
		return "repository"
	}
	return string(id)
}
