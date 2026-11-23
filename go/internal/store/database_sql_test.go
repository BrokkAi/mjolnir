// Port of the test-only SQL scripts in mj-store/src/database/ that the
// migration tests in database/schema.rs build historical stores from
// (legacy_v1.sql, project_catalog_v67.sql). Byte-identical copies live under
// testdata/database/.

package store

import _ "embed"

// legacyV1SQL is the store at historical revision 1, which upgrade
// regression tests migrate forward (database/legacy_v1.sql).
//
//go:embed testdata/database/legacy_v1.sql
var legacyV1SQL string

// projectCatalogV67SQL is the frozen project catalog of revision 67, before
// the catalog merge (database/project_catalog_v67.sql).
//
//go:embed testdata/database/project_catalog_v67.sql
var projectCatalogV67SQL string
