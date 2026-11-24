//! Process ownership evidence for programs XaYa launched itself.
//!
//! Every owned launch carries an unguessable `XAYA_OWNER` value in its environment. Descendants
//! inherit it, including ones that `setsid` and are reparented to init/launchd and so leave any
//! process tree. A process is identified by (pid, start time) and is only signalled after being
//! re-observed with the same identity and marker, so a reused PID is never touched.
//!
//! Some platforms hide the environment of their own binaries: macOS 27 shows only the argv of
//! Apple platform binaries such as /bin/zsh, /bin/sleep or /usr/bin/git, even to the same user.
//! Such a hidden process is attributed by structure instead, and only by evidence the kernel
//! cannot get wrong: its parent is owned, it sits in a session whose living leader is owned
//! (sessions cannot be joined, only created), or its exact identity was attributed before.
//!
//! Boundaries: a program that scrubs its environment is invisible here, as is a hidden process
//! that left both its owned parent and owned session before it was ever observed; only this
//! user's processes are inspected.
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};

use crate::{Error, Result};

pub(crate) const OWNER_ENV: &str = "XAYA_OWNER";
/// Carried only by an engine's tool processes (Claude Bash commands, Codex exec commands) and
/// their descendants, never by the engine itself or its helpers such as MCP servers.
pub(crate) const TOOL_ENV: &str = "XAYA_TOOL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owned {
    pub pid: i32,
    pub ppid: i32,
    /// Platform start-time identity; compared for equality only.
    pub started: (u64, u64),
}

impl Owned {
    /// The same process: PID and start time. The parent changes when a process is reparented.
    fn same_process(&self, other: &Owned) -> bool {
        self.pid == other.pid && self.started == other.started
    }
}

/// What the platform shows of one process's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Env {
    Readable(Vec<Vec<u8>>),
    /// Hidden by the platform (no entries shown) or no longer readable.
    Hidden,
}

#[derive(Debug, Clone)]
struct Process {
    owned: Owned,
    sid: i32,
    env: Env,
}

impl Process {
    fn carries(&self, entries: &[Vec<u8>]) -> bool {
        matches!(&self.env, Env::Readable(env) if entries.iter().all(|entry| env.contains(entry)))
    }
}

/// A fresh 128-bit marker value for one launch.
pub(crate) fn new_marker() -> Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A random RFC 4122 version 4 UUID, for native session identities XaYa chooses itself.
pub(crate) fn new_uuid() -> Result<String> {
    let mut b = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

fn validate(name: &str, marker: &str) -> Result<Vec<u8>> {
    if marker.len() < 32 || !marker.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Protocol("ownership marker must be at least 32 hex digits"));
    }
    Ok(format!("{name}={marker}").into_bytes())
}

/// Processes of this user, other than the caller, owned by the launch marked `marker`.
pub(crate) fn scan(marker: &str) -> Result<Vec<Owned>> {
    scan_all(&[(OWNER_ENV, marker)])
}

/// Processes owned under every `(name, marker)` entry: carrying them all, or hidden and
/// structurally tied to such a process.
pub(crate) fn scan_all(markers: &[(&str, &str)]) -> Result<Vec<Owned>> {
    scan_with(markers, &[])
}

/// As [`scan_all`], also counting every process whose exact identity is in `known`: processes
/// attributed earlier stay attributed after their owned parent or session leader has gone.
pub(crate) fn scan_with(markers: &[(&str, &str)], known: &[Owned]) -> Result<Vec<Owned>> {
    let entries = entries(markers)?;
    Ok(attribute(&table()?, &entries, known))
}

/// Tool processes of a launch: those carrying both markers, plus `registered` tool shells and
/// their hidden descendants. A tool shell whose environment the platform hides cannot show the
/// tool marker, so the shell registers its own (pid, parent pid) before each command; an entry
/// counts only while a process with exactly that PID and parent is still owned by the launch.
pub(crate) fn scan_tools(owner: &str, tool: &str, registered: &[(i32, i32)]) -> Result<Vec<Owned>> {
    let table = table()?;
    let owner_entries = entries(&[(OWNER_ENV, owner)])?;
    let tool_entries = entries(&[(OWNER_ENV, owner), (TOOL_ENV, tool)])?;
    Ok(attribute_tools(&table, &owner_entries, &tool_entries, registered))
}

fn attribute_tools(
    table: &[Process],
    owner: &[Vec<u8>],
    tool: &[Vec<u8>],
    registered: &[(i32, i32)],
) -> Vec<Owned> {
    let roots: Vec<Owned> = attribute(table, owner, &[])
        .into_iter()
        .filter(|process| registered.contains(&(process.pid, process.ppid)))
        .collect();
    attribute(table, tool, &roots)
}

/// Whether any process remains in session `sid`, recorded as the session an owned launch's
/// program created. Conservative and never used to signal: a hidden member that escaped its
/// owned parent is still counted. A living leader that is not owned shows the number now
/// belongs to someone else's session.
pub(crate) fn session_in_use(sid: i32, marker: &str) -> Result<bool> {
    let entries = entries(&[(OWNER_ENV, marker)])?;
    Ok(session_members(&table()?, sid, &entries))
}

fn entries(markers: &[(&str, &str)]) -> Result<Vec<Vec<u8>>> {
    markers.iter().map(|(name, marker)| validate(name, marker)).collect()
}

fn table() -> Result<Vec<Process>> {
    // SAFETY: getpid has no preconditions.
    let own = unsafe { libc::getpid() };
    Ok(platform::processes()?
        .into_iter()
        .filter(|(owned, _)| owned.pid != own)
        .map(|(owned, sid)| {
            let env = match platform::environment(owned.pid) {
                Ok(env) if !env.is_empty() => Env::Readable(env),
                _ => Env::Hidden,
            };
            Process { owned, sid, env }
        })
        .collect())
}

fn attribute(table: &[Process], entries: &[Vec<u8>], known: &[Owned]) -> Vec<Owned> {
    let mut owned: HashSet<i32> = table
        .iter()
        .filter(|p| p.carries(entries) || known.iter().any(|k| k.same_process(&p.owned)))
        .map(|p| p.owned.pid)
        .collect();
    // A readable process without the markers demonstrably is not ours; only hidden ones are
    // attributed by structure, until nothing changes.
    loop {
        let sessions: HashSet<i32> = table
            .iter()
            .filter(|p| p.owned.pid == p.sid && owned.contains(&p.owned.pid))
            .map(|p| p.sid)
            .collect();
        let before = owned.len();
        for process in table.iter().filter(|p| p.env == Env::Hidden) {
            if owned.contains(&process.owned.ppid) || sessions.contains(&process.sid) {
                owned.insert(process.owned.pid);
            }
        }
        if owned.len() == before {
            break;
        }
    }
    table.iter().filter(|p| owned.contains(&p.owned.pid)).map(|p| p.owned.clone()).collect()
}

fn session_members(table: &[Process], sid: i32, entries: &[Vec<u8>]) -> bool {
    let by_pid: HashMap<i32, &Process> = table.iter().map(|p| (p.owned.pid, p)).collect();
    if by_pid.get(&sid).is_some_and(|leader| leader.sid == sid && !leader.carries(entries)) {
        return false;
    }
    table.iter().any(|p| p.sid == sid)
}

/// SIGKILL every `expected` process still present with the same identity and ownership.
/// Returns the PIDs signalled.
pub(crate) fn terminate(expected: &[Owned], marker: &str) -> Result<Vec<i32>> {
    signal_all(expected, &[(OWNER_ENV, marker)], libc::SIGKILL)
}

/// Send `signal` to every `expected` process re-observed with the same identity and still owned
/// under `markers`; an identity attributed earlier remains owned, so a reused PID is never hit.
pub(crate) fn signal_all(
    expected: &[Owned],
    markers: &[(&str, &str)],
    signal: libc::c_int,
) -> Result<Vec<i32>> {
    let current = scan_with(markers, expected)?;
    let mut signalled = Vec::new();
    for process in expected {
        if current.iter().any(|found| found.same_process(process)) {
            // SAFETY: identity and ownership were just re-observed for this exact process.
            if unsafe { libc::kill(process.pid, signal) } == 0 {
                signalled.push(process.pid);
            }
        }
    }
    Ok(signalled)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(super) fn processes() -> Result<Vec<(Owned, i32)>> {
        // SAFETY: a null buffer asks for the required size.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut pids = vec![0 as libc::pid_t; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: buffer of `bytes` bytes owned for the call's duration.
        let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if found <= 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let mut result = Vec::new();
        for &pid in pids.iter().take(found as usize).filter(|&&pid| pid > 0) {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            // SAFETY: valid output buffer of exactly `size` bytes.
            let read = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
            };
            if read != size || info.pbi_uid != uid {
                continue; // Exited meanwhile, or not ours.
            }
            // SAFETY: getsid only reads the process table.
            let sid = unsafe { libc::getsid(pid) };
            if sid == -1 {
                continue; // Exited meanwhile.
            }
            result.push((
                Owned {
                    pid,
                    ppid: info.pbi_ppid as i32,
                    started: (info.pbi_start_tvsec, info.pbi_start_tvusec),
                },
                sid,
            ));
        }
        Ok(result)
    }

    /// Environment entries from KERN_PROCARGS2: argc, exec path, padding, argv, then env.
    pub(super) fn environment(pid: i32) -> Result<Vec<Vec<u8>>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: size query with a null buffer.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let mut buffer = vec![0u8; size];
        // SAFETY: buffer has `size` bytes.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        buffer.truncate(size);
        parse_procargs(&buffer)
    }

    pub(super) fn parse_procargs(buffer: &[u8]) -> Result<Vec<Vec<u8>>> {
        let malformed = || Error::Protocol("malformed process arguments");
        let argc = i32::from_ne_bytes(
            buffer.get(..4).ok_or_else(malformed)?.try_into().unwrap_or_default(),
        );
        let mut rest = &buffer[4..];
        // Executable path, then NUL padding.
        let end = rest.iter().position(|&b| b == 0).ok_or_else(malformed)?;
        rest = &rest[end..];
        let start = rest.iter().position(|&b| b != 0).unwrap_or(rest.len());
        rest = &rest[start..];
        let mut strings = rest.split(|&b| b == 0);
        for _ in 0..argc.max(0) {
            strings.next().ok_or_else(malformed)?;
        }
        Ok(strings.take_while(|s| !s.is_empty()).map(<[u8]>::to_vec).collect())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub(super) fn processes() -> Result<Vec<(Owned, i32)>> {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let mut result = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else { continue };
            if metadata.uid() != uid {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
            // comm may contain spaces and parentheses; fields resume after the last ')'.
            let Some(fields) =
                stat.rfind(')').map(|i| stat[i + 2..].split(' ').collect::<Vec<_>>())
            else {
                continue;
            };
            let (Some(ppid), Some(sid), Some(start)) =
                (fields.get(1), fields.get(3), fields.get(19))
            else {
                continue;
            };
            let (Ok(ppid), Ok(sid), Ok(start)) = (ppid.parse(), sid.parse(), start.parse()) else {
                continue;
            };
            result.push((Owned { pid, ppid, started: (start, 0) }, sid));
        }
        Ok(result)
    }

    pub(super) fn environment(pid: i32) -> Result<Vec<Vec<u8>>> {
        let bytes = std::fs::read(format!("/proc/{pid}/environ"))?;
        Ok(bytes.split(|&b| b == 0).filter(|s| !s.is_empty()).map(<[u8]>::to_vec).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn markers_and_uuids_are_well_formed() {
        let marker = new_marker().unwrap();
        assert_eq!(marker.len(), 32);
        assert!(validate(OWNER_ENV, &marker).is_ok());
        assert!(validate(OWNER_ENV, "short").is_err());
        let uuid = new_uuid().unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert!(["8", "9", "a", "b"].contains(&&uuid[19..20]));
        assert_ne!(uuid, new_uuid().unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn procargs_environment_follows_argv_exactly() {
        let mut buffer = 2i32.to_ne_bytes().to_vec();
        buffer
            .extend_from_slice(b"/bin/x\0\0\0\0x\0XAYA_OWNER=not-env\0A=1\0XAYA_OWNER=abc\0\0junk");
        let env = platform::parse_procargs(&buffer).unwrap();
        assert_eq!(env, vec![b"A=1".to_vec(), b"XAYA_OWNER=abc".to_vec()]);
    }

    #[test]
    fn reparented_marked_descendant_is_found_and_only_its_identity_is_killed() {
        let marker = new_marker().unwrap();
        // The grandchild setsid()s and is reparented to init, leaving every process tree.
        let script = "import os, time\nif os.fork() == 0:\n    os.setsid()\n    if os.fork() == \
                      0:\n\x20       print(os.getpid(), flush=True)\n        time.sleep(30)\n    \
                      os._exit(0)\nos.wait()\n";
        let mut launcher = Command::new("python3")
            .args(["-c", script])
            .env(OWNER_ENV, &marker)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut text = String::new();
        let mut stdout = launcher.stdout.take().unwrap();
        let mut byte = [0];
        while stdout.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            text.push(byte[0] as char);
        }
        let pid: i32 = text.trim().parse().unwrap();
        // The launcher exits once its intermediate child has; only the grandchild remains.
        launcher.wait().unwrap();
        let found = scan(&marker).unwrap();
        let owned = found.iter().find(|p| p.pid == pid).cloned().expect("marked grandchild found");
        assert_eq!(owned.ppid, 1);
        let stale = Owned { started: (0, 0), ..owned.clone() };
        assert!(terminate(&[stale], &marker).unwrap().is_empty(), "PID reuse is never signalled");
        assert!(scan(&new_marker().unwrap()).unwrap().iter().all(|p| p.pid != pid));
        assert_eq!(terminate(&[owned], &marker).unwrap(), vec![pid]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && scan(&marker).unwrap().iter().any(|p| p.pid == pid) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(scan(&marker).unwrap().iter().all(|p| p.pid != pid));
    }

    /// When the test binary is started with this variable set, the `helper` test below becomes a
    /// readable (non-platform) process that starts one `/bin/sleep` child and then waits.
    const HELPER_ENV: &str = "XAYA_OWNERSHIP_TEST_HELPER";

    #[test]
    fn helper() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        println!("{}", child.id());
        child.wait().unwrap();
    }

    /// A readable process carrying `env`, with one `/bin/sleep` child (hidden on platforms that
    /// hide their own binaries). Returns the helper and its child's PID.
    fn spawn_helper(env: &[(&str, &str)]) -> (std::process::Child, i32) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "ownership::tests::helper", "--nocapture", "--test-threads=1"])
            .env(HELPER_ENV, "1")
            .env_remove(TOOL_ENV)
            .stdout(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut helper = command.spawn().unwrap();
        let pid = first_pid(helper.stdout.take().unwrap());
        (helper, pid)
    }

    /// libtest prints a header first, then the child PID after its own "test ... " status.
    fn first_pid(mut stdout: std::process::ChildStdout) -> i32 {
        let mut line = String::new();
        let mut byte = [0];
        loop {
            assert_eq!(stdout.read(&mut byte).unwrap(), 1, "helper ended early");
            if byte[0] != b'\n' {
                line.push(byte[0] as char);
                continue;
            }
            if let Some(Ok(pid)) = line.split_whitespace().last().map(str::parse) {
                return pid;
            }
            line.clear();
        }
    }

    fn pids(found: &[Owned]) -> Vec<i32> {
        let mut pids: Vec<_> = found.iter().map(|p| p.pid).collect();
        pids.sort_unstable();
        pids
    }

    #[test]
    fn tool_scan_requires_every_marker_and_signals_only_tools() {
        let (owner, tool) = (new_marker().unwrap(), new_marker().unwrap());
        let other_tool = new_marker().unwrap();
        let (mut helper, helper_child) = spawn_helper(&[(OWNER_ENV, &owner)]);
        let (mut other, other_child) =
            spawn_helper(&[(OWNER_ENV, &owner), (TOOL_ENV, &other_tool)]);
        let (mut tool_process, tool_child) =
            spawn_helper(&[(OWNER_ENV, &owner), (TOOL_ENV, &tool)]);
        let tool_pid = tool_process.id() as i32;
        let markers = [(OWNER_ENV, owner.as_str()), (TOOL_ENV, tool.as_str())];
        let found = scan_all(&markers).unwrap();
        // The tool's child is found whether the platform shows its environment or hides it.
        let mut expected = vec![tool_pid, tool_child];
        expected.sort_unstable();
        assert_eq!(pids(&found), expected);
        let all = pids(&scan(&owner).unwrap());
        for pid in [helper.id() as i32, other.id() as i32, tool_pid, helper_child, other_child] {
            assert!(all.contains(&pid), "{pid} is owned");
        }
        assert!(scan_all(&[(TOOL_ENV, "short")]).is_err(), "markers are validated");
        let mut signalled = signal_all(&found, &markers, libc::SIGTERM).unwrap();
        signalled.sort_unstable();
        assert_eq!(signalled, expected);
        tool_process.wait().unwrap();
        assert!(helper.try_wait().unwrap().is_none() && other.try_wait().unwrap().is_none());
        assert!(terminate(&scan(&owner).unwrap(), &owner).unwrap().len() >= 4);
        for child in [&mut helper, &mut other] {
            child.wait().unwrap();
        }
    }

    fn process(pid: i32, ppid: i32, sid: i32, env: Env) -> Process {
        Process { owned: Owned { pid, ppid, started: (pid as u64, 0) }, sid, env }
    }

    #[test]
    fn hidden_processes_are_attributed_only_through_owned_structure() {
        let marked = || Env::Readable(vec![b"XAYA_OWNER=m".to_vec(), b"A=1".to_vec()]);
        let plain = || Env::Readable(vec![b"A=1".to_vec()]);
        let entries = vec![b"XAYA_OWNER=m".to_vec()];
        let table = vec![
            process(10, 1, 10, plain()),      // the service, leading its own session
            process(11, 10, 10, marked()),    // a marked process in the service's session
            process(12, 10, 10, Env::Hidden), // service helper: not ours
            process(20, 1, 20, marked()),     // program, leading its own session
            process(21, 20, 20, Env::Hidden), // tool shell
            process(22, 21, 20, Env::Hidden), // its child, hidden as well
            process(23, 1, 20, Env::Hidden),  // escaped its parent, still in the session
            process(24, 21, 24, Env::Hidden), // setsid'd while its parent is owned
            process(25, 24, 24, Env::Hidden), // inside that new session
            process(26, 21, 20, plain()),     // readable without marker: scrubbed, not ours
            process(27, 26, 20, Env::Hidden), // descendant of the scrubbed one, in session
            process(30, 1, 30, Env::Hidden),  // unrelated orphan in another session
            process(31, 11, 31, Env::Hidden), // child of an owned non-leader
        ];
        let owned = pids(&attribute(&table, &entries, &[]));
        assert_eq!(owned, vec![11, 20, 21, 22, 23, 24, 25, 27, 31]);

        // Once the program is gone, escaped hidden processes stay owned only by identity.
        let orphaned: Vec<_> = table.iter().filter(|p| p.owned.pid != 20).cloned().collect();
        assert_eq!(pids(&attribute(&orphaned, &entries, &[])), vec![11, 31]);
        let seen = attribute(&table, &entries, &[]);
        assert_eq!(pids(&attribute(&orphaned, &entries, &seen)), vec![
            11, 21, 22, 23, 24, 25, 27, 31
        ]);
        // A reused PID has a new start time and is not the process seen before.
        let mut reused = orphaned.clone();
        reused.iter_mut().filter(|p| p.owned.pid == 23).for_each(|p| p.owned.started = (99, 0));
        assert!(!pids(&attribute(&reused, &entries, &seen)).contains(&23));
    }

    #[test]
    fn registered_tool_shells_count_only_while_owned_with_the_same_parent() {
        let owner = vec![b"XAYA_OWNER=m".to_vec()];
        let tool = vec![b"XAYA_OWNER=m".to_vec(), b"XAYA_TOOL=t".to_vec()];
        let table = vec![
            process(20, 1, 20, Env::Readable(owner.clone())), // the engine
            process(21, 20, 20, Env::Hidden),                 // registered tool shell
            process(22, 21, 20, Env::Hidden),                 // its platform child
            process(23, 20, 20, Env::Hidden),                 // platform helper, unregistered
            process(24, 20, 20, Env::Hidden),                 // registered under another parent
            process(25, 23, 20, Env::Readable(tool.clone())), // marked tool under the helper
            process(26, 25, 20, Env::Hidden),                 // its platform child
            process(40, 1, 40, Env::Hidden),                  // not the launch's at all
        ];
        let registered = [(21, 20), (24, 99), (40, 1)];
        assert_eq!(pids(&attribute_tools(&table, &owner, &tool, &registered)), vec![
            21, 22, 25, 26
        ]);
        assert_eq!(pids(&attribute_tools(&table, &owner, &tool, &[])), vec![25, 26]);
    }

    #[test]
    fn a_recorded_session_is_in_use_until_empty_unless_its_number_was_reused() {
        let entries = vec![b"XAYA_OWNER=m".to_vec()];
        let marked = Env::Readable(entries.clone());
        let plain = Env::Readable(vec![b"A=1".to_vec()]);
        // Leader gone: a hidden member keeps the session in use.
        assert!(session_members(&[process(23, 1, 20, Env::Hidden)], 20, &entries));
        assert!(session_members(&[process(20, 1, 20, marked)], 20, &entries));
        assert!(!session_members(&[process(23, 1, 30, Env::Hidden)], 20, &entries));
        // A living leader that is not ours: the number was reused by someone else's session.
        for leader in [plain, Env::Hidden] {
            let table = [process(20, 1, 20, leader), process(21, 20, 20, Env::Hidden)];
            assert!(!session_members(&table, 20, &entries));
        }
    }
}
