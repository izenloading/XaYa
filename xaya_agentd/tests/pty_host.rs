#![cfg(unix)]
//! Detached PTY host: XaYa, not the engine's own daemon, owns the terminal and process tree.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use xaya_agentd::pty_host::{self, Exit, Size};

const BINARY: &str = env!("CARGO_BIN_EXE_xaya-agentd");

// Echoes lines, reports SIGWINCH sizes, can flood output and exit with a chosen code.
const PROGRAM: &str = r#"
import os, signal, stat, sys
def winch(*_):
    size = os.get_terminal_size(0)
    print(f"size {size.lines}x{size.columns}", flush=True)
signal.signal(signal.SIGWINCH, winch)
print("ready", os.getpid(), os.environ.get("XAYA_OWNER"), flush=True)
for line in sys.stdin:
    line = line.strip()
    if line == "burst":
        for _ in range(3000):
            sys.stdout.write("x" * 100 + "\n")
        print("burst-done", flush=True)
    elif line == "fds":
        socks = []
        for fd in range(3, 256):
            try:
                if stat.S_ISSOCK(os.fstat(fd).st_mode):
                    socks.append(fd)
            except OSError:
                pass
        print("sockets", socks, flush=True)
    elif line.startswith("exit "):
        sys.exit(int(line.split()[1]))
    else:
        print("echo:" + line, flush=True)
"#;

struct Fixture {
    temp: TempDir,
    hosts: std::cell::RefCell<Vec<PathBuf>>,
}

impl Drop for Fixture {
    // A failed assertion must not leak a detached host and its program.
    fn drop(&mut self) {
        for dir in self.hosts.borrow().iter() {
            if pty_host::exit_status(dir).ok().flatten().is_none() {
                let _ = pty_host::signal(dir, libc::SIGKILL);
            }
        }
    }
}

impl Fixture {
    fn new() -> Self {
        Self { temp: tempfile::tempdir().unwrap(), hosts: Default::default() }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    fn spawn(&self, name: &str) -> pty_host::Host {
        self.spawn_relayed(name, None)
    }

    fn spawn_relayed(&self, name: &str, relay: Option<pty_host::Relay<'_>>) -> pty_host::Host {
        let argv = ["python3", "-c", PROGRAM].map(String::from);
        let host = pty_host::spawn_relayed(
            Path::new(BINARY),
            &self.dir(name),
            self.temp.path(),
            &argv,
            &[("XAYA_OWNER".into(), "fixture-marker".into())],
            Size { rows: 24, cols: 80 },
            relay,
        )
        .unwrap();
        self.hosts.borrow_mut().push(host.dir.clone());
        host
    }
}

fn wait<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn host_outlives_its_spawner_and_admits_one_authenticated_client() {
    let fixture = Fixture::new();
    let host = fixture.spawn("one");
    // Its own session: neither the spawner's death nor its terminal hangup reaches it.
    // SAFETY: getsid/getppid only query process attributes.
    unsafe {
        assert_ne!(libc::getsid(host.host_pid), libc::getsid(0));
        assert_ne!(libc::getsid(host.child_pid), libc::getsid(host.host_pid));
    }
    assert!(alive(host.host_pid) && alive(host.child_pid));
    let mut first = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    let ready = first.read_until("fixture-marker", Duration::from_secs(10)).unwrap();
    assert!(ready.contains(&format!("ready {}", host.child_pid)), "{ready}");
    first.send(b"hello\n").unwrap();
    first.read_until("echo:hello", Duration::from_secs(5)).unwrap();
    assert!(pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).is_err(), "exclusive");
    let token = "0".repeat(64);
    assert!(pty_host::attach_with_token(&host.dir, &token, Size { rows: 24, cols: 80 }).is_err());
    let status = pty_host::status(&host.dir).unwrap();
    assert!(status.attached);
    assert_eq!(status.child_pid, host.child_pid);
    drop(first);
    let mut again = wait(|| pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).ok());
    // The bounded replay lets a new client see what happened before it arrived.
    assert!(again.replay().contains("echo:hello"), "{}", again.replay());
    again.send(b"again\n").unwrap();
    again.read_until("echo:again", Duration::from_secs(5)).unwrap();
    assert_eq!(pty_host::status(&host.dir).unwrap().child_pid, host.child_pid);
    pty_host::signal(&host.dir, libc::SIGKILL).unwrap();
    wait(|| pty_host::exit_status(&host.dir).unwrap());
}

#[test]
fn detached_output_is_drained_and_resizes_reach_the_program() {
    let fixture = Fixture::new();
    let host = fixture.spawn("drain");
    let mut client = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    client.read_until("ready", Duration::from_secs(10)).unwrap();
    client.send(b"burst\n").unwrap();
    drop(client);
    // ~300 KB while nobody is attached: the program must not block on a full PTY.
    std::thread::sleep(Duration::from_millis(500));
    let mut client = wait(|| pty_host::attach(&host.dir, Size { rows: 40, cols: 120 }).ok());
    assert!(client.replay().len() <= pty_host::REPLAY_LIMIT, "replay is bounded");
    client.read_until("size 40x120", Duration::from_secs(5)).unwrap();
    client.send(b"after\n").unwrap();
    let text = client.read_until("echo:after", Duration::from_secs(5)).unwrap();
    assert!(
        client.replay().contains("burst-done") || text.contains("burst-done"),
        "input sent just before detaching must not be lost: {:?}",
        &text[text.len().saturating_sub(600)..]
    );
    client.resize(Size { rows: 30, cols: 100 }).unwrap();
    client.read_until("size 30x100", Duration::from_secs(5)).unwrap();
    pty_host::signal(&host.dir, libc::SIGKILL).unwrap();
}

#[test]
fn child_exit_is_recorded_durably_and_signals_are_whitelisted() {
    let fixture = Fixture::new();
    let host = fixture.spawn("exit");
    let mut client = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    client.read_until("ready", Duration::from_secs(10)).unwrap();
    assert!(pty_host::signal(&host.dir, libc::SIGSTOP).is_err(), "only HUP/INT/TERM/KILL");
    client.send(b"exit 7\n").unwrap();
    assert_eq!(client.wait_exit(Duration::from_secs(10)).unwrap(), Exit {
        code: Some(7),
        signal: None
    });
    assert_eq!(
        pty_host::exit_status(&host.dir).unwrap(),
        Some(Exit { code: Some(7), signal: None })
    );
    wait(|| (!alive(host.host_pid)).then_some(()));
    assert!(!host.dir.join("socket").exists(), "socket removed after exit");
    assert!(pty_host::status(&host.dir).is_err());

    let host = fixture.spawn("terminated");
    let mut client = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    client.read_until("ready", Duration::from_secs(10)).unwrap();
    pty_host::signal(&host.dir, libc::SIGTERM).unwrap();
    let exit = wait(|| pty_host::exit_status(&host.dir).unwrap());
    assert_eq!(exit, Exit { code: None, signal: Some(libc::SIGTERM) });
    assert!(!alive(host.child_pid));
}

#[test]
fn hangup_reaches_a_program_with_default_signal_handling() {
    // The host itself ignores SIGHUP; that disposition must not leak into the program, or a
    // hangup never ends one that installs no handler of its own.
    let fixture = Fixture::new();
    let host = fixture.spawn("hangup");
    let mut client = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    client.read_until("ready", Duration::from_secs(10)).unwrap();
    pty_host::signal(&host.dir, libc::SIGHUP).unwrap();
    let exit = wait(|| pty_host::exit_status(&host.dir).unwrap());
    assert_eq!(exit, Exit { code: None, signal: Some(libc::SIGHUP) });
}

#[test]
fn host_directory_is_new_private_and_never_reused() {
    let fixture = Fixture::new();
    let host = fixture.spawn("private");
    for (path, mode) in [("", 0o700), ("socket", 0o600), ("token", 0o600), ("host.json", 0o600)] {
        let metadata = fs::symlink_metadata(host.dir.join(path)).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, mode, "{path}");
    }
    let argv = ["/bin/sh".to_string()];
    let size = Size { rows: 24, cols: 80 };
    let reused =
        pty_host::spawn(Path::new(BINARY), &host.dir, fixture.temp.path(), &argv, &[], size);
    assert!(reused.is_err(), "an existing host directory is never reused");
    std::os::unix::fs::symlink(fixture.temp.path(), fixture.dir("linked")).unwrap();
    let linked = pty_host::spawn(
        Path::new(BINARY),
        &fixture.dir("linked"),
        fixture.temp.path(),
        &argv,
        &[],
        size,
    );
    assert!(linked.is_err());
    pty_host::signal(&host.dir, libc::SIGKILL).unwrap();
    wait(|| pty_host::exit_status(&host.dir).unwrap());
}

#[test]
fn host_hangs_up_its_program_when_its_directory_disappears() {
    let fixture = Fixture::new();
    let host = fixture.spawn("orphan");
    let mut client = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    client.read_until("ready", Duration::from_secs(10)).unwrap();
    drop(client);
    // An unreachable host would otherwise leave an unmanaged writer running forever.
    fs::remove_dir_all(&host.dir).unwrap();
    wait(|| (!alive(host.child_pid) && !alive(host.host_pid)).then_some(()));
    assert!(!host.dir.exists(), "the host never recreates a directory it no longer owns");
}

#[test]
fn relay_endpoint_lives_with_the_host_and_forwards_only_to_its_target() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::net::UnixListener;

    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = fixture.dir("relay.sock");
    let host = fixture
        .spawn_relayed("relayed", Some(pty_host::Relay { listener: &listener, target: &target }));
    // The spawner's copy goes away, as when the service crashes: the host keeps the port.
    drop(listener);
    let mut client = pty_host::attach(&host.dir, Size { rows: 24, cols: 80 }).unwrap();
    client.read_until("ready", Duration::from_secs(10)).unwrap();
    // The hosted program never inherits the relay endpoint.
    client.send(b"fds\n").unwrap();
    client.read_until("sockets []", Duration::from_secs(5)).unwrap();

    // No target yet: the connection is closed without a reply, like an unreachable service.
    let mut early = TcpStream::connect(("127.0.0.1", port)).unwrap();
    early.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = early.write_all(b"lost");
    let mut nothing = Vec::new();
    assert_eq!(early.read_to_end(&mut nothing).unwrap_or(0), 0);

    let service = UnixListener::bind(&target).unwrap();
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = service.accept().unwrap();
        let mut request = [0; 4];
        stream.read_exact(&mut request).unwrap();
        stream.write_all(b"pong:").unwrap();
        stream.write_all(&request).unwrap();
    });
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(b"ping").unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "pong:ping");
    echo.join().unwrap();

    client.send(b"exit 0\n").unwrap();
    client.wait_exit(Duration::from_secs(10)).unwrap();
    wait(|| (!alive(host.host_pid)).then_some(()));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "endpoint ends with its host");
}
