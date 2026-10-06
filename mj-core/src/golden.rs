//! Small helpers for checked-in, full-surface golden tests.

use std::fs;
use std::path::Path;

/// Compare rendered output with `<manifest_dir>/tests/golden/<name>.txt`.
///
/// Set `MJ_UPDATE_GOLDEN=1` to write the current output to that file.
pub fn assert_golden(manifest_dir: &str, name: &str, actual: &str) {
    let path = Path::new(manifest_dir)
        .join("tests")
        .join("golden")
        .join(format!("{name}.txt"));

    if std::env::var_os("MJ_UPDATE_GOLDEN").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let parent = path.parent().expect("golden path has a parent");
        fs::create_dir_all(parent).unwrap_or_else(|error| {
            panic!(
                "could not create golden directory {}: {error}",
                parent.display()
            )
        });
        fs::write(
            &path,
            format!("{}\n", normalize_one_trailing_newline(actual)),
        )
        .unwrap_or_else(|error| panic!("could not write golden {}: {error}", path.display()));
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "could not read golden {}: {error}\nset MJ_UPDATE_GOLDEN=1 to create or update it",
            path.display()
        )
    });
    let expected = normalize_one_trailing_newline(&expected);
    let actual = normalize_one_trailing_newline(actual);
    if expected != actual {
        let diff = similar::TextDiff::from_lines(expected, actual)
            .unified_diff()
            .header("expected", "actual")
            .to_string();
        panic!(
            "golden output differs at {}\n{diff}\nset MJ_UPDATE_GOLDEN=1 to update it",
            path.display()
        );
    }
}

/// Like [`assert_golden`] for surfaces that show platform key names (Cmd-V on
/// macOS): macOS compares with `<name>-macos.txt`.
pub fn assert_platform_golden(manifest_dir: &str, name: &str, actual: &str) {
    if cfg!(target_os = "macos") {
        assert_golden(manifest_dir, &format!("{name}-macos"), actual);
    } else {
        assert_golden(manifest_dir, name, actual);
    }
}

fn normalize_one_trailing_newline(text: &str) -> &str {
    text.strip_suffix('\n').unwrap_or(text)
}
