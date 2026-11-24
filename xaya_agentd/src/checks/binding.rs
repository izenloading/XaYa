//! Binding proof: does a passing gate read the tree it is credited with?
//!
//! The canary only ever runs in a private checkout. A gate that still passes after a tracked
//! source file was corrupted either reads another tree (an absolute path, an editable install,
//! a shared build output) or reads no source at all. Neither exit code is evidence.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

use crate::checkpoints::Git;
use crate::{Error, Result};

/// Unbalanced parentheses: a parse error in Rust, C-family, Go, Java, JavaScript and Python alike.
pub(super) const CANARY: &str =
    "((( xaya binding canary: deliberately corrupted private copy, never the user's checkout\n";
pub(super) const MAX_CANDIDATES: usize = 2;

const SOURCE_SUFFIXES: &[&str] = &[
    "rs", "c", "cc", "cpp", "h", "hpp", "go", "java", "kt", "swift", "js", "jsx", "mjs", "cjs",
    "ts", "tsx", "py", "rb", "php", "scala", "cs", "zig",
];
const SKIP_DIRS: &[&str] = &[
    "test",
    "tests",
    "testing",
    "__tests__",
    "spec",
    "specs",
    "fixtures",
    "testdata",
    "examples",
    "example",
    "benches",
    "vendor",
    "node_modules",
    "third_party",
    "target",
    "build",
    "dist",
];
const ENTRY_NAMES: &[&str] =
    &["lib.rs", "main.rs", "__init__.py", "main.go", "index.ts", "index.js", "main.py"];

fn rpc(message: &str) -> Error {
    Error::Rpc { code: xaya_agent_proto::INVALID_PARAMS, message: message.into() }
}

/// Syntax only; tree membership is checked against the exact commit separately.
pub(super) fn validate_path(path: &str) -> Result<()> {
    let relative = Path::new(path);
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(char::is_control)
        || !relative.components().all(|c| matches!(c, Component::Normal(_)))
        || path.starts_with(".xaya/")
        || path.starts_with(".git/")
        || path == ".git"
    {
        return Err(rpc(
            "binding path must be a relative tracked file outside .xaya and .git, without .."
        ));
    }
    Ok(())
}

fn looks_like_test(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let Some((name, directories)) = parts.split_last() else {
        return true;
    };
    if directories.iter().any(|d| SKIP_DIRS.contains(d) || d.starts_with('.')) {
        return true;
    }
    let stem = name.split('.').next().unwrap_or_default();
    matches!(stem, "conftest" | "setup" | "build")
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || name.contains(".test.")
        || name.contains(".spec.")
}

fn is_source(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SOURCE_SUFFIXES.contains(&e))
        && !looks_like_test(path)
}

fn rank(path: &str) -> (bool, usize, &str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    (!ENTRY_NAMES.contains(&name), path.matches('/').count(), path)
}

fn entries(output: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    // `<mode> SP <type> SP <object> TAB <path>`, NUL terminated.
    output.split('\0').filter_map(|entry| {
        let (meta, path) = entry.split_once('\t')?;
        let mut meta = meta.split(' ');
        Some((meta.next()?, meta.next()?, path))
    })
}

/// The subset of `paths` that are regular (non-symlink) files in `commit`, in Git's order.
fn regular_files(git: &Git<'_>, commit: &str, paths: &[String]) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for chunk in paths.chunks(256) {
        let mut args = vec!["ls-tree", "-z", "--full-tree", commit, "--"];
        args.extend(chunk.iter().map(String::as_str));
        for (mode, kind, path) in entries(&git.run(&args, None)?) {
            if kind == "blob"
                && matches!(mode, "100644" | "100755")
                && chunk.iter().any(|p| p == path)
            {
                found.push(path.to_owned());
            }
        }
    }
    Ok(found)
}

pub(super) fn tracked(git: &Git<'_>, commit: &str, path: &str) -> Result<bool> {
    Ok(!regular_files(git, commit, &[path.to_owned()])?.is_empty())
}

/// Paths the change under verification added or modified: the checkpoint's uncommitted work,
/// or for an integrated candidate (base == commit) the commit against its first parent.
fn changed(git: &Git<'_>, base: &str, commit: &str) -> Result<Vec<String>> {
    let output = if base != commit {
        git.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                "--diff-filter=AM",
                base,
                commit,
            ],
            None,
        )?
    } else {
        git.run(
            &[
                "diff-tree",
                "--no-commit-id",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                "--diff-filter=AM",
                commit,
            ],
            None,
        )?
    };
    Ok(output.split('\0').filter(|p| !p.is_empty()).map(str::to_owned).collect())
}

/// Well-known entry points within two levels, found without listing the whole tree.
fn entry_points(git: &Git<'_>, commit: &str) -> Result<Vec<String>> {
    let mut directories = vec![String::new()];
    for (_, kind, path) in entries(&git.run(&["ls-tree", "-z", "--full-tree", commit], None)?) {
        if kind == "tree" && !SKIP_DIRS.contains(&path) && !path.starts_with('.') {
            directories.push(format!("{path}/"));
        }
    }
    let mut paths = Vec::new();
    for directory in &directories {
        for prefix in [directory.clone(), format!("{directory}src/")] {
            paths.extend(ENTRY_NAMES.iter().map(|name| format!("{prefix}{name}")));
        }
    }
    regular_files(git, commit, &paths)
}

/// The best file to corrupt, plus one structurally different backup: a first pick that nothing
/// imports would otherwise report a bound gate as unbound.
pub(super) fn candidates(git: &Git<'_>, base: &str, commit: &str) -> Result<Vec<String>> {
    let mut touched: Vec<String> =
        changed(git, base, commit)?.into_iter().filter(|p| is_source(p)).collect();
    touched = regular_files(git, commit, &touched)?;
    touched.sort_by(|a, b| rank(a).cmp(&rank(b)));
    let mut entries: Vec<String> =
        entry_points(git, commit)?.into_iter().filter(|p| is_source(p)).collect();
    entries.sort_by(|a, b| rank(a).cmp(&rank(b)));
    let mut picks: Vec<String> = Vec::new();
    for path in touched.iter().take(1).chain(&entries).chain(&touched) {
        if picks.len() == MAX_CANDIDATES {
            break;
        }
        if !picks.contains(path) {
            picks.push(path.clone());
        }
    }
    Ok(picks)
}

/// Prepend the canary inside `root`, refusing any symlinked component that could escape it.
pub(super) fn corrupt(root: &Path, path: &str) -> Result<()> {
    validate_path(path)?;
    let mut current = root.to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        let last = index + 1 == components.len();
        if metadata.file_type().is_symlink()
            || (last && !metadata.is_file())
            || (!last && !metadata.is_dir())
        {
            return Err(Error::Protocol("binding path is not a regular file inside the checkout"));
        }
    }
    let original = fs::read(&current)?;
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&current)?;
    file.write_all(CANARY.as_bytes())?;
    file.write_all(&original)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_selection_skips_tests_vendored_and_hidden_paths() {
        for path in ["src/value.rs", "pkg/__init__.py", "web/index.ts", "cmd/main.go"] {
            assert!(is_source(path), "{path}");
        }
        for path in [
            "tests/value.rs",
            "src/value_test.go",
            "pkg/test_value.py",
            "web/value.spec.ts",
            "node_modules/x/index.js",
            ".github/x.py",
            "build.rs",
            "README.md",
            "Cargo.toml",
        ] {
            assert!(!is_source(path), "{path}");
        }
    }

    #[test]
    fn entry_points_rank_before_deeper_files() {
        let mut paths = vec!["a/b/c.rs", "src/value.rs", "src/lib.rs", "x/src/lib.rs"];
        paths.sort_by(|a, b| rank(a).cmp(&rank(b)));
        assert_eq!(paths, ["src/lib.rs", "x/src/lib.rs", "src/value.rs", "a/b/c.rs"]);
    }

    #[test]
    fn paths_must_stay_relative_and_outside_control_directories() {
        for path in ["src/lib.rs", "a b/c.py"] {
            assert!(validate_path(path).is_ok(), "{path}");
        }
        for path in [
            "",
            "/etc/passwd",
            "../x.rs",
            "src/../x.rs",
            "./x.rs",
            ".xaya/project.toml",
            ".git/config",
            "a\nb.rs",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn corruption_refuses_symlinked_components_and_prepends_otherwise() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim.rs"), "fn main() {}\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("victim.rs"), root.join("file.rs")).unwrap();
        assert!(corrupt(root, "linked/victim.rs").is_err());
        assert!(corrupt(root, "file.rs").is_err());
        assert_eq!(fs::read_to_string(outside.path().join("victim.rs")).unwrap(), "fn main() {}\n");
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        corrupt(root, "src/lib.rs").unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            format!("{CANARY}pub fn x() {{}}\n")
        );
    }
}
