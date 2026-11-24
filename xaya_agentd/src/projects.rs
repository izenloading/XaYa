//! Repository-defined commands require a local trust record for the exact configuration bytes.
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use xaya_agent_proto::{self as proto, ProjectInfo, VerifyStep};

use crate::checkpoints::Git;
use crate::storage::Store;
use crate::{Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    verify: Verify,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verify {
    steps: Vec<Step>,
    #[serde(default = "default_timeout")]
    timeout: String,
    #[serde(default)]
    require_binding: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    name: String,
    argv: Vec<String>,
    #[serde(default = "default_cwd")]
    cwd: String,
    timeout_secs: Option<u64>,
}
fn default_timeout() -> String {
    "5m".into()
}
fn default_cwd() -> String {
    ".".into()
}
fn invalid(message: &str) -> Error {
    Error::Rpc { code: proto::INVALID_PARAMS, message: message.into() }
}

pub(crate) fn get(store: &Store, cwd: &str) -> Result<ProjectInfo> {
    get_with_cancel(store, cwd, &AtomicBool::new(false))
}

pub(crate) fn get_with_cancel(
    store: &Store,
    cwd: &str,
    stopped: &AtomicBool,
) -> Result<ProjectInfo> {
    let cwd = Path::new(cwd).canonicalize()?;
    let root = PathBuf::from(Git::new(&cwd, stopped).run(&["rev-parse", "--show-toplevel"], None)?)
        .canonicalize()?;
    let directory = root.join(".xaya");
    if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
        return Err(invalid(".xaya must not be a symlink"));
    }
    let path = directory.join("project.toml");
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("project.toml must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(invalid("project.toml exceeds 64 KiB"));
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("project.toml must be UTF-8"))?;
    let config: Config =
        toml::from_str(text).map_err(|error| invalid(&format!("invalid project.toml: {error}")))?;
    let timeout = timeout(&config.verify.timeout)?;
    if config.verify.steps.is_empty() || config.verify.steps.len() > 16 {
        return Err(invalid("verify.steps requires 1..16 steps"));
    }
    let mut names = BTreeSet::new();
    let mut steps = Vec::new();
    for step in config.verify.steps {
        if step.name.trim().is_empty()
            || step.name.len() > 64
            || step.name.chars().any(char::is_control)
            || !names.insert(step.name.clone())
        {
            return Err(invalid(
                "step names must be unique, nonempty and contain no control characters (<=64 \
                 bytes)",
            ));
        }
        let step_timeout = step.timeout_secs.unwrap_or(timeout);
        crate::checks::validate(&proto::CheckRunParams {
            checkpoint_id: String::new(),
            command_id: "config".into(),
            argv: step.argv.clone(),
            timeout_secs: step_timeout,
        })?;
        if step_timeout > timeout {
            return Err(invalid("step timeout exceeds verify.timeout"));
        }
        resolve_cwd(&root, &step.cwd)?;
        steps.push(VerifyStep {
            name: step.name,
            argv: step.argv,
            cwd: step.cwd,
            timeout_secs: step_timeout,
        });
    }
    if steps.iter().flat_map(|step| &step.argv).map(String::len).sum::<usize>() > 8192 {
        return Err(invalid("combined step arguments exceed 8192 bytes"));
    }
    let root = root.to_str().ok_or_else(|| invalid("project path must be UTF-8"))?.to_owned();
    let info = ProjectInfo {
        trusted: store.project_trusted(&root, &hash)?,
        root,
        config_path: path.to_str().ok_or_else(|| invalid("project path must be UTF-8"))?.into(),
        config_hash: hash,
        timeout_secs: timeout,
        steps,
        require_binding: config.verify.require_binding,
    };
    if serde_json::to_vec(&info)?.len() > proto::MAX_FRAME_BYTES - 1024 {
        return Err(invalid("project configuration exceeds protocol size limit"));
    }
    Ok(info)
}

/// Whether the repository containing `cwd` requires a proved binding before landing.
///
/// Read without trust: the setting can only refuse more. No configuration means no
/// requirement; a configuration that exists but cannot be read or parsed fails closed.
pub(crate) fn binding_required(store: &Store, cwd: &str, stopped: &AtomicBool) -> Result<bool> {
    let cwd = Path::new(cwd).canonicalize()?;
    let root = PathBuf::from(Git::new(&cwd, stopped).run(&["rev-parse", "--show-toplevel"], None)?)
        .canonicalize()?;
    for path in [root.join(".xaya"), root.join(".xaya/project.toml")] {
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            result => {
                result?;
            },
        }
    }
    let root = root.to_str().ok_or_else(|| invalid("project path must be UTF-8"))?;
    Ok(get_with_cancel(store, root, stopped)?.require_binding)
}

pub(crate) fn trust(store: &Store, cwd: &str, hash: &str) -> Result<ProjectInfo> {
    let mut info = get(store, cwd)?;
    if info.config_hash != hash {
        return Err(Error::Rpc {
            code: proto::CONFLICT,
            message: "project configuration changed; review it again before trusting".into(),
        });
    }
    store.trust_project(&info.root, hash)?;
    info.trusted = true;
    Ok(info)
}

/// A task workspace the service created holds the repository's committed files. It carries a
/// trust the user already gave the same configuration bytes in another checkout of the same
/// repository; any other bytes stay untrusted until someone reviews them.
pub(crate) fn inherit_trust(
    store: &Store,
    project: &str,
    workspace: &str,
    stopped: &AtomicBool,
) -> Result<()> {
    let Ok(info) = get_with_cancel(store, workspace, stopped) else {
        return Ok(()); // No usable plan: the cycle reports it where it needs one.
    };
    if info.trusted {
        return Ok(());
    }
    for root in store.roots_trusting(&info.config_hash)? {
        if root != info.root
            && crate::tasks::repository(&root, stopped).ok().as_deref() == Some(project)
        {
            return store.trust_project(&info.root, &info.config_hash);
        }
    }
    Ok(())
}

pub(crate) fn resolve_cwd(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.len() > 1024 || Path::new(relative).is_absolute() {
        return Err(invalid("step cwd must be a relative path within the project"));
    }
    let cwd = root.join(relative).canonicalize()?;
    if !cwd.starts_with(root) || !cwd.is_dir() {
        return Err(invalid("step cwd must be an existing directory within the project"));
    }
    Ok(cwd)
}

fn timeout(value: &str) -> Result<u64> {
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 3600),
        _ => return Err(invalid("verify.timeout must use s, m or h (1..3600 seconds)")),
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| (1..=3600).contains(n))
        .ok_or_else(|| invalid("verify.timeout must be 1..3600 seconds"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    fn fixture() -> (tempfile::TempDir, std::sync::Arc<Store>) {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir(temp.path().join(".xaya")).unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("runtime"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        (temp, store)
    }

    #[test]
    fn rejects_unknown_options_duplicate_steps_and_unsafe_cwds_without_dispatch() {
        let (temp, store) = fixture();
        let path = temp.path().join(".xaya/project.toml");
        for config in [
            "[verify]\nsteps = []",
            "[verify]\nsteps = [{name='test', argv=[]}]",
            "[verify]\nsteps = [{name='test', argv=['true']}, {name='test', argv=['false']}]",
            "[verify]\nsteps = [{name='test', argv=['true'], cwd='../../'}]",
            "[verify]\ntimeout = '0s'\nsteps = [{name='test', argv=['true']}]",
            "[verify]\ntimeout = '5m'\nsteps = [{name='test', argv=['true'], timeout_secs=301}]",
            "[verify]\nsteps = [{name='test', argv=['true'], shell=true}]",
            "[verify]\nsteps = [{name='test', argv=['true']}]\n[policy]\nprofile='edit'",
        ] {
            fs::write(&path, config).unwrap();
            assert!(get(&store, temp.path().to_str().unwrap()).is_err(), "accepted {config}");
        }
        let external = tempfile::tempdir().unwrap();
        symlink(external.path(), temp.path().join("escape")).unwrap();
        fs::write(&path, "[verify]\nsteps = [{name='test', argv=['true'], cwd='escape'}]").unwrap();
        assert!(get(&store, temp.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn trust_is_bound_to_canonical_repository_and_exact_bytes() {
        let (temp, store) = fixture();
        let path = temp.path().join(".xaya/project.toml");
        let config = "[verify]\ntimeout = '20m'\nsteps = [{name='test', argv=['true']}]\n";
        fs::write(&path, config).unwrap();
        let root = temp.path().to_str().unwrap();
        let info = get(&store, root).unwrap();
        assert_eq!(info.timeout_secs, 1200);
        assert_eq!(info.config_hash.len(), 64);
        assert!(!info.trusted);
        assert!(trust(&store, root, "wrong").is_err());
        trust(&store, root, &info.config_hash).unwrap();
        assert!(get(&store, root).unwrap().trusted);
        fs::write(&path, format!("{config}\n")).unwrap();
        let changed = get(&store, root).unwrap();
        assert!(!changed.trusted);
        assert_ne!(changed.config_hash, info.config_hash);
        let (other, _) = fixture();
        fs::write(other.path().join(".xaya/project.toml"), config).unwrap();
        assert!(!get(&store, other.path().to_str().unwrap()).unwrap().trusted);
    }

    #[test]
    fn require_binding_defaults_off_and_must_be_a_boolean() {
        let (temp, store) = fixture();
        let path = temp.path().join(".xaya/project.toml");
        let root = temp.path().to_str().unwrap();
        fs::write(&path, "[verify]\nsteps = [{name='test', argv=['true']}]").unwrap();
        assert!(!get(&store, root).unwrap().require_binding);
        fs::write(
            &path,
            "[verify]\nrequire_binding = true\nsteps = [{name='test', argv=['true']}]",
        )
        .unwrap();
        assert!(get(&store, root).unwrap().require_binding);
        fs::write(&path, "[verify]\nrequire_binding = 1\nsteps = [{name='test', argv=['true']}]")
            .unwrap();
        assert!(get(&store, root).is_err());
    }

    #[test]
    fn landing_policy_is_absent_without_configuration_and_fails_closed_when_invalid() {
        let (temp, store) = fixture();
        let root = temp.path().to_str().unwrap();
        assert!(!binding_required(&store, root, &AtomicBool::new(false)).unwrap());
        fs::remove_dir(temp.path().join(".xaya")).unwrap();
        assert!(!binding_required(&store, root, &AtomicBool::new(false)).unwrap());
        fs::create_dir(temp.path().join(".xaya")).unwrap();
        fs::write(temp.path().join(".xaya/project.toml"), "[verify]\nsteps = []").unwrap();
        assert!(binding_required(&store, root, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn symlinked_or_nonregular_configuration_is_rejected() {
        let (temp, store) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let path = temp.path().join(".xaya/project.toml");
        fs::write(outside.path().join("config"), "[verify]\nsteps=[]").unwrap();
        symlink(outside.path().join("config"), &path).unwrap();
        assert!(get(&store, temp.path().to_str().unwrap()).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(get(&store, temp.path().to_str().unwrap()).is_err());
    }
}
