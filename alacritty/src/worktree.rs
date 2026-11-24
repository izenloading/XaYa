//! Git worktrees for agents: each agent gets its own checkout and branch, so agents working on the
//! same repository don't edit each other's files.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Prefix of the branches created for new worktrees, followed by a number.
const BRANCH_PREFIX: &str = "agent-";

/// A worktree that was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The repository's name, from its main checkout's directory.
    pub repo: String,
    pub branch: String,
    pub path: PathBuf,
}

impl Worktree {
    /// Label shown in front of the title of the worktree's window, such as `⎇ agent-2`.
    pub fn label(&self) -> String {
        format!("⎇ {}", self.branch)
    }
}

/// Create a worktree of the repository at `cwd`, checked out from its current commit.
///
/// `branch` is created, or checked out when it exists; without one, the first free `agent-N` is
/// created. The worktree goes to `<base>/<branch>`, by default `<repo>.worktrees` beside the
/// main checkout.
pub fn create(cwd: &Path, branch: Option<&str>, base: Option<&Path>) -> Result<Worktree, String> {
    let toplevel = PathBuf::from(git(cwd, ["rev-parse", "--show-toplevel"])?);
    // The main checkout holds the common git directory, also when `cwd` is a worktree itself.
    let common =
        PathBuf::from(git(cwd, ["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    let main = common.parent().filter(|_| common.ends_with(".git")).unwrap_or(&toplevel);
    let repo = main.file_name().and_then(OsStr::to_str).unwrap_or("repo").to_owned();
    let base = match base {
        Some(base) => base.to_path_buf(),
        None => main.with_file_name(format!("{repo}.worktrees")),
    };

    let branch_exists = |branch: &str| {
        let reference = format!("refs/heads/{branch}");
        git(&toplevel, ["show-ref", "--verify", "--quiet", reference.as_str()]).is_ok()
    };
    let (branch, exists) = match branch {
        Some(branch) => (branch.to_owned(), branch_exists(branch)),
        None => {
            let free =
                |branch: &str| !branch_exists(branch) && !base.join(dir_name(branch)).exists();
            (next_branch(free), false)
        },
    };

    let path = base.join(dir_name(&branch));
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let path_arg = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add"];
    if exists {
        args.extend([path_arg.as_str(), branch.as_str()]);
    } else {
        args.extend(["-b", branch.as_str(), path_arg.as_str()]);
    }
    git(&toplevel, args)?;

    Ok(Worktree { repo, branch, path })
}

/// The first `agent-N` that's `free`.
fn next_branch(free: impl Fn(&str) -> bool) -> String {
    (1..).map(|n| format!("{BRANCH_PREFIX}{n}")).find(|branch| free(branch)).unwrap()
}

/// Directory name for `branch`: `feature/x` goes to `feature-x`.
fn dir_name(branch: &str) -> String {
    branch.replace(['/', '\\'], "-")
}

/// The checkout `path` is in: the nearest directory with a `.git`, which is a file in worktrees.
pub fn checkout_root(path: &Path) -> Option<&Path> {
    path.ancestors().find(|dir| dir.join(".git").exists())
}

/// Run git in `dir`, returning its trimmed output or its error message.
fn git<I, S>(dir: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| format!("Unable to run git: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim().trim_start_matches("fatal: ");
        Err(if message.is_empty() {
            format!("git failed: {}", output.status)
        } else {
            message.into()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A repository with one commit in a fresh directory, `None` without git.
    fn repo(name: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("xaya-worktree-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let repo = dir.join("project");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        let run = |args: &[&str]| git(&repo, args.iter().copied());
        run(&["init", "-q", "-b", "main"]).ok()?;
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]).unwrap();
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]).unwrap();
        Some(repo.canonicalize().unwrap())
    }

    #[test]
    fn branch_names() {
        assert_eq!(next_branch(|_| true), "agent-1");
        assert_eq!(next_branch(|branch| branch != "agent-1" && branch != "agent-2"), "agent-3");
        assert_eq!(dir_name("feature/login"), "feature-login");
    }

    #[test]
    fn worktrees_get_free_branches_beside_the_repo() {
        let Some(repo) = repo("free") else { return };
        let first = create(&repo.join("src"), None, None).unwrap();
        assert_eq!(first.repo, "project");
        assert_eq!(first.branch, "agent-1");
        assert_eq!(first.path, repo.with_file_name("project.worktrees/agent-1"));
        assert!(first.path.join("src/main.rs").is_file());
        assert_eq!(first.label(), "⎇ agent-1");

        // From inside a worktree, the next one still goes beside the main checkout.
        let second = create(&first.path, None, None).unwrap();
        assert_eq!(second.branch, "agent-2");
        assert_eq!(second.repo, "project");
        assert_eq!(second.path, repo.with_file_name("project.worktrees/agent-2"));

        assert_eq!(checkout_root(&repo.join("src")), Some(repo.as_path()));
        assert_eq!(checkout_root(&first.path.join("src")), Some(first.path.as_path()));
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn named_branches_are_created_or_checked_out() {
        let Some(repo) = repo("named") else { return };
        let base = repo.with_file_name("trees");
        let created = create(&repo, Some("feature/login"), Some(&base)).unwrap();
        assert_eq!(created.path, base.join("feature-login"));
        assert_eq!(git(&created.path, ["branch", "--show-current"]).unwrap(), "feature/login");

        // An existing branch is checked out; one checked out elsewhere is refused by git.
        git(&repo, ["branch", "existing"]).unwrap();
        let existing = create(&repo, Some("existing"), Some(&base)).unwrap();
        assert_eq!(git(&existing.path, ["branch", "--show-current"]).unwrap(), "existing");
        assert!(create(&repo, Some("main"), Some(&base)).is_err());

        // Taken directories and non-repositories fail with a message.
        let taken = create(&repo, Some("feature/login"), Some(&base)).unwrap_err();
        assert!(taken.contains("already exists"), "{taken}");
        let outside = create(&std::env::temp_dir(), None, None).unwrap_err();
        assert!(outside.contains("not a git repository"), "{outside}");
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }
}
