// Port of mj-store/src/database/mounts.rs.

package store

import (
	"database/sql"
	"errors"
	"math"
	"runtime"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RememberMountSources atomically applies the controller's MRU policy for
// newly used mount sources.
func RememberMountSources(host string, mounts []core.AdditionalMount) error {
	if len(mounts) == 0 {
		return nil
	}
	sources := make([]string, 0, len(mounts))
	for _, mount := range mounts {
		sources = append(sources, mount.Source)
	}
	_, err := submitDatabaseWrite("remember_mount_sources", func(*Connection) (struct{}, error) {
		return struct{}{}, rememberSources(DatabasePath(), host, sources)
	})
	return err
}

func ReplaceMountHistory(host string, sources []string) error {
	sources = slices.Clone(sources)
	_, err := submitDatabaseWrite("replace_mount_history", func(*Connection) (struct{}, error) {
		return struct{}{}, replaceMountHistoryIn(DatabasePath(), host, sources)
	})
	return err
}

func replaceMountHistoryIn(path, host string, sources []string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := writeMountHistory(tx, host, sources); err != nil {
		return err
	}
	return tx.Commit()
}

func writeMountHistory(tx *Transaction, host string, sources []string) error {
	if _, err := tx.Exec("DELETE FROM mount_history WHERE host = ?1", host); err != nil {
		return err
	}
	var written []string
	for _, source := range sources[:min(len(sources), 20)] {
		if slices.ContainsFunc(written, func(existing string) bool { return mountsPathEqual(existing, source) }) {
			continue
		}
		if _, err := tx.Exec("INSERT INTO mount_history(host, source, ordinal) VALUES (?1, ?2, ?3)",
			host, pathToBlob(source), int64(len(written))); err != nil {
			return err
		}
		written = append(written, source)
	}
	return nil
}

func writeHostContainerSize(tx *Transaction, host string, size core.HostContainerSize) error {
	if strings.TrimSpace(host) == "" {
		return errors.New("container size host is empty")
	}
	if uint64(size.Cpus) > math.MaxInt64 {
		return errors.New("container CPU count exceeds SQLite range: out of range integral type conversion attempted")
	}
	if size.MemoryBytes > math.MaxInt64 {
		return errors.New("container memory exceeds SQLite range: out of range integral type conversion attempted")
	}
	cpus, memory := int64(size.Cpus), int64(size.MemoryBytes)
	if cpus <= 0 || memory <= 0 {
		return errors.New("container size values must be positive")
	}
	_, err := tx.Exec(`INSERT INTO host_container_sizes(host, cpus, memory_bytes)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(host) DO UPDATE SET cpus = excluded.cpus, memory_bytes = excluded.memory_bytes`,
		host, cpus, memory)
	return err
}

func RememberProjectDirectory(host string, directory string) error {
	host = "project:" + host
	_, err := submitDatabaseWrite("remember_project_directory", func(*Connection) (struct{}, error) {
		return struct{}{}, rememberSources(DatabasePath(), host, []string{directory})
	})
	return err
}

// mountsPathEqual is PathBuf equality: the paths' components match, so
// repeated and trailing separators and inner `.` components do not count.
func mountsPathEqual(left, right string) bool {
	return slices.Equal(mountsPathComponents(left), mountsPathComponents(right))
}

func mountsPathComponents(path string) []string {
	separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
	var components []string
	if path != "" && separator(rune(path[0])) {
		components = append(components, "/")
	}
	for index, part := range strings.FieldsFunc(path, separator) {
		if part == "." && !(index == 0 && components == nil && strings.HasPrefix(path, ".")) {
			continue
		}
		components = append(components, part)
	}
	return components
}

func rememberSources(path, host string, newSources []string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	sources, err := valuesQueryMap(tx, "SELECT source FROM mount_history WHERE host = ?1 ORDER BY ordinal", []any{host},
		func(rows *sql.Rows) (string, error) {
			var blob any
			if err := rows.Scan(&blob); err != nil {
				return "", err
			}
			bytes, ok := blob.([]byte)
			if !ok {
				_, _, err := blobOrNull(blob)
				if err == nil {
					err = errors.New("Invalid column type Null")
				}
				return "", err
			}
			return blobToPath(bytes), nil
		})
	if err != nil {
		return err
	}
	for index := len(newSources) - 1; index >= 0; index-- {
		source := newSources[index]
		sources = slices.DeleteFunc(sources, func(existing string) bool { return mountsPathEqual(existing, source) })
		sources = slices.Insert(sources, 0, source)
	}
	sources = sources[:min(len(sources), 20)]
	if err := writeMountHistory(tx, host, sources); err != nil {
		return err
	}
	return tx.Commit()
}
