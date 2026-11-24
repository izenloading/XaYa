//! Detached per-session PTY host. XaYa, not an engine's own daemon, owns the terminal and the
//! process tree under it, so native identity, exit and ownership can be verified directly.
//!
//! A host lives in its own session, outside the service and any window. It holds the PTY master,
//! starts one program as session leader on the slave, keeps draining output into a bounded replay
//! while nobody is attached, admits one attached client at a time over a private socket, and
//! records the program's exit durably. Control connections may query status or relay a small set
//! of signals. Everything is authenticated by a token file only the owner can read.
//!
//! A host may also hold a loopback TCP endpoint for the program (see [`Relay`]) and forward each
//! connection to a Unix socket its owner serves. The endpoint lives exactly as long as the host,
//! so a program configured with it keeps one stable address while its owner restarts.
use std::collections::VecDeque;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::runtime::user_id;
use crate::{Error, Result};

/// Output kept for a client that attaches later. Older output is dropped at a line boundary.
pub const REPLAY_LIMIT: usize = 64 * 1024;
const PROTOCOL: u32 = 1;
const MAGIC: &[u8] = b"XPTY1 ";
const TOKEN_HEX: usize = 64;
const MAX_FRAME: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET: &str = "socket";
const TOKEN: &str = "token";
const HOST: &str = "host.json";
const EXIT: &str = "exit.json";
const FAILED: &str = "failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

/// How the hosted program ended: exactly one of `code` or `signal` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub host_pid: i32,
    pub child_pid: i32,
    pub attached: bool,
}

#[derive(Debug, Clone)]
pub struct Host {
    pub dir: PathBuf,
    pub host_pid: i32,
    pub child_pid: i32,
}

#[derive(Serialize, Deserialize)]
struct HostFile {
    protocol: u32,
    host_pid: i32,
    child_pid: i32,
}

mod frame {
    pub const DATA: u8 = b'D';
    pub const REPLAY: u8 = b'P';
    pub const RESIZE: u8 = b'R';
    pub const SIGNAL: u8 = b'S';
    pub const STATUS: u8 = b'Q';
    pub const OK: u8 = b'O';
    pub const ERROR: u8 = b'E';
    pub const JSON: u8 = b'J';
    pub const EXIT: u8 = b'X';
}
const ROLE_ATTACH: u8 = b'A';
const ROLE_CONTROL: u8 = b'C';

fn conflict(message: &str) -> Error {
    Error::Rpc { code: xaya_agent_proto::CONFLICT, message: message.into() }
}

fn write_frame(stream: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too large"))?;
    let mut header = [kind, 0, 0, 0, 0];
    header[1..].copy_from_slice(&length.to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)
}

fn read_frame(stream: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 5];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(ErrorKind::InvalidData, "PTY host frame exceeds limit"));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn size_bytes(size: Size) -> [u8; 4] {
    let [r0, r1] = size.rows.to_be_bytes();
    let [c0, c1] = size.cols.to_be_bytes();
    [r0, r1, c0, c1]
}

fn parse_size(bytes: &[u8]) -> Option<Size> {
    match bytes {
        [r0, r1, c0, c1] => {
            let size =
                Size { rows: u16::from_be_bytes([*r0, *r1]), cols: u16::from_be_bytes([*c0, *c1]) };
            (size.rows > 0 && size.cols > 0).then_some(size)
        },
        _ => None,
    }
}

fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// Write `name` in `dir` atomically: a private temporary file, synced, then renamed.
fn publish(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = dir.join(format!(".{name}.tmp"));
    let _ = fs::remove_file(&temporary);
    let mut file = private_file(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join(name))?;
    File::open(dir)?.sync_all()
}

fn read_private(dir: &Path, name: &str, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != user_id() || metadata.mode() & 0o077 != 0 {
        return Err(Error::Protocol("PTY host file is not private to this user"));
    }
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_token(dir: &Path) -> Result<String> {
    let token = String::from_utf8(read_private(dir, TOKEN, 256)?)
        .map_err(|_| Error::Protocol("invalid PTY host token"))?;
    if token.len() != TOKEN_HEX || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Protocol("invalid PTY host token"));
    }
    Ok(token)
}

fn random_token() -> Result<String> {
    let mut bytes = [0; TOKEN_HEX / 2];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// ------------------------------------------------------------------------------------ spawning

/// A listening loopback endpoint handed to the host, which forwards each accepted connection
/// to `target`. The caller may close its own copy once the host is spawned.
pub struct Relay<'a> {
    pub listener: &'a std::net::TcpListener,
    pub target: &'a Path,
}

/// At most this many relayed connections at once; more are closed unanswered.
const RELAY_LIMIT: usize = 16;

/// Start a detached host in a new private `dir` and wait until it serves `argv`.
pub fn spawn(
    executable: &Path,
    dir: &Path,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    size: Size,
) -> Result<Host> {
    spawn_relayed(executable, dir, cwd, argv, env, size, None)
}

/// [`spawn`], with the host also holding `relay`'s endpoint for its whole lifetime.
pub fn spawn_relayed(
    executable: &Path,
    dir: &Path,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    size: Size,
    relay: Option<Relay<'_>>,
) -> Result<Host> {
    spawn_relayed_unset(executable, dir, cwd, argv, env, &[], size, relay)
}

/// [`spawn_relayed`], with the variables named in `unset` removed from the program's environment
/// rather than inherited from this process.
#[allow(clippy::too_many_arguments)]
pub fn spawn_relayed_unset(
    executable: &Path,
    dir: &Path,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    unset: &[&str],
    size: Size,
    relay: Option<Relay<'_>>,
) -> Result<Host> {
    if argv.is_empty() || argv[0].is_empty() || size.rows == 0 || size.cols == 0 {
        return Err(Error::Protocol("PTY host needs a program and a non-empty size"));
    }
    // Never reuse or follow an existing path: a host directory belongs to exactly one launch.
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => {},
        Ok(_) => return Err(conflict("PTY host directory already exists")),
        Err(error) => return Err(error.into()),
    }
    DirBuilder::new().mode(0o700).create(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != user_id() || metadata.mode() & 0o777 != 0o700 {
        return Err(Error::Protocol("PTY host directory is not private"));
    }
    let mut token = private_file(&dir.join(TOKEN))?;
    token.write_all(random_token()?.as_bytes())?;
    token.sync_all()?;
    let mut command = Command::new(executable);
    command.arg("pty-host").arg("--dir").arg(dir);
    if let Some(relay) = &relay {
        let fd = relay.listener.as_raw_fd();
        command.arg("--relay-fd").arg(fd.to_string()).arg("--relay-to").arg(relay.target);
        // SAFETY: fcntl is async-signal-safe. Only the forked child stops closing this one
        // descriptor on exec; every other process spawned by the caller never inherits it.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // The program inherits the host's environment.
    for name in unset {
        command.env_remove(name);
    }
    let status = command
        .args(["--rows", &size.rows.to_string(), "--cols", &size.cols.to_string(), "--"])
        .args(argv)
        .current_dir(cwd)
        .env_remove(crate::faults::CRASH_ENV)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(Error::Protocol("PTY host failed to detach"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = read_private(dir, HOST, 4096) {
            let host: HostFile = serde_json::from_slice(&bytes)?;
            if host.protocol != PROTOCOL {
                return Err(Error::Protocol("PTY host protocol mismatch"));
            }
            return Ok(Host {
                dir: dir.to_owned(),
                host_pid: host.host_pid,
                child_pid: host.child_pid,
            });
        }
        if let Ok(reason) = read_private(dir, FAILED, 1024) {
            return Err(Error::Rpc {
                code: xaya_agent_proto::ENGINE_ERROR,
                message: format!("PTY host failed: {}", String::from_utf8_lossy(&reason)),
            });
        }
        if Instant::now() >= deadline {
            return Err(Error::Protocol("PTY host did not become ready"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ------------------------------------------------------------------------------------- serving

/// Entry point of `xaya-agentd pty-host`. Must run before any thread is started: it forks.
/// `relay` is an inherited listening descriptor and the socket its connections go to.
pub fn serve(dir: &Path, size: Size, argv: &[String], relay: Option<(RawFd, &Path)>) -> Result<()> {
    // Take the endpoint first, closed on exec: the hosted program must never inherit it.
    let relay = match relay {
        Some((fd, target)) => Some((relay_listener(fd)?, target.to_owned())),
        None => None,
    };
    // SAFETY: the process is still single-threaded; the parent only exits.
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error().into()),
        0 => {},
        _ => std::process::exit(0),
    }
    // SAFETY: plain syscalls in the freshly forked, single-threaded child.
    unsafe {
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error().into());
        }
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let result = host(dir, size, argv, relay);
    if let Err(error) = &result {
        let _ = publish(dir, FAILED, error.to_string().as_bytes());
    }
    result
}

fn set_size(master: RawFd, size: Size) -> io::Result<bool> {
    let mut current: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor and output buffer.
    unsafe { libc::ioctl(master, libc::TIOCGWINSZ as _, &mut current) };
    let changed = current.ws_row != size.rows || current.ws_col != size.cols;
    let wanted = libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: valid descriptor and input buffer; the kernel signals the foreground group.
    if unsafe { libc::ioctl(master, libc::TIOCSWINSZ as _, &wanted) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(changed)
}

/// Ask the foreground program to redraw even when the size did not change.
fn redraw(master: RawFd) {
    // SAFETY: tcgetpgrp only reads the terminal's foreground process group.
    let group = unsafe { libc::tcgetpgrp(master) };
    if group > 0 {
        // SAFETY: signals the hosted terminal's own foreground group.
        unsafe { libc::kill(-group, libc::SIGWINCH) };
    }
}

struct Replay {
    bytes: VecDeque<u8>,
    dropped: bool,
}

impl Replay {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        let excess = self.bytes.len().saturating_sub(REPLAY_LIMIT);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.dropped = true;
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        if !self.dropped {
            return bytes;
        }
        // Start after a line boundary so a cut escape sequence is not replayed.
        match bytes.iter().position(|&b| b == b'\n') {
            Some(index) => bytes[index + 1..].to_vec(),
            None => Vec::new(),
        }
    }
}

fn decode(status: libc::c_int) -> Exit {
    if libc::WIFSIGNALED(status) {
        Exit { code: None, signal: Some(libc::WTERMSIG(status)) }
    } else {
        Exit { code: Some(libc::WEXITSTATUS(status)), signal: None }
    }
}

fn relay_listener(fd: RawFd) -> Result<std::net::TcpListener> {
    // SAFETY: fstat only reads the descriptor's metadata.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if fd < 3
        || unsafe { libc::fstat(fd, &mut stat) } == -1
        || stat.st_mode & libc::S_IFMT != libc::S_IFSOCK
    {
        return Err(Error::Protocol("PTY host relay descriptor is not a socket"));
    }
    // SAFETY: the spawner handed this descriptor to us alone; we own it from here on.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    // SAFETY: valid descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(Error::Protocol("PTY host relay must listen on loopback"));
    }
    listener.set_nonblocking(false)?;
    Ok(listener)
}

/// Forward every connection to `target`. When nothing serves `target` (the owner is down), the
/// connection is closed unanswered, which the program sees as an unavailable service.
fn relay(listener: std::net::TcpListener, target: PathBuf) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if active.fetch_add(1, Ordering::AcqRel) >= RELAY_LIMIT {
            active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let (active, target) = (Arc::clone(&active), target.clone());
        let spawned = std::thread::Builder::new().spawn(move || {
            let _ = forward(stream, &target);
            active.fetch_sub(1, Ordering::AcqRel);
        });
        if spawned.is_err() {
            // The closure (and its count) was dropped with the failed spawn.
            continue;
        }
    }
}

fn forward(tcp: std::net::TcpStream, target: &Path) -> io::Result<()> {
    let unix = UnixStream::connect(target)?;
    let (mut from_program, mut to_owner) = (tcp.try_clone()?, unix.try_clone()?);
    let upstream = std::thread::Builder::new().spawn(move || {
        let _ = io::copy(&mut from_program, &mut to_owner);
        let _ = to_owner.shutdown(std::net::Shutdown::Write);
    })?;
    let (mut from_owner, mut to_program) = (unix, tcp);
    let _ = io::copy(&mut from_owner, &mut to_program);
    // The owner answered and closed: end both directions so the upstream copy returns too.
    let _ = to_program.shutdown(std::net::Shutdown::Both);
    let _ = from_owner.shutdown(std::net::Shutdown::Both);
    let _ = upstream.join();
    Ok(())
}

fn host(
    dir: &Path,
    size: Size,
    argv: &[String],
    relayed: Option<(std::net::TcpListener, PathBuf)>,
) -> Result<()> {
    let token = read_token(dir)?;
    let mut winsize =
        libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: valid output pointers; no name buffer; default terminal attributes.
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut::<libc::c_char>(),
            std::ptr::null_mut::<libc::termios>(),
            &mut winsize,
        )
    } == -1
    {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: openpty returned two new descriptors owned by this process.
    let (master, slave) =
        unsafe { (OwnedFd::from_raw_fd(master_fd), OwnedFd::from_raw_fd(slave_fd)) };
    // SAFETY: valid descriptor.
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            // The host ignores HUP/PIPE, and ignored dispositions survive exec. Give the
            // program the defaults a terminal program expects, so a hangup ends it.
            let mut default: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            for signal in [libc::SIGHUP, libc::SIGPIPE] {
                if libc::sigaction(signal, &default, std::ptr::null_mut()) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let child_pid = child.id() as i32;
    drop(command);
    // Threads only after the program is started: nothing forks from here on.
    if let Some((listener, target)) = relayed {
        std::thread::Builder::new().name("relay".into()).spawn(move || relay(listener, target))?;
    }
    let listener = UnixListener::bind(dir.join(SOCKET))?;
    fs::set_permissions(dir.join(SOCKET), fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let bound = fs::symlink_metadata(dir.join(SOCKET))?;
    let identity = (bound.dev(), bound.ino());
    // SAFETY: getpid has no preconditions.
    let host_pid = unsafe { libc::getpid() };
    publish(
        dir,
        HOST,
        &serde_json::to_vec(&HostFile { protocol: PROTOCOL, host_pid, child_pid })?,
    )?;
    // SAFETY: valid descriptor; nonblocking so a busy program never stalls the host.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut state = State {
        master,
        master_open: true,
        client: None,
        replay: Replay { bytes: VecDeque::new(), dropped: false },
        token,
        host_pid,
        child_pid,
    };
    let mut checked = Instant::now();
    let mut orphaned: Option<Instant> = None;
    let exit = loop {
        state.poll(&listener)?;
        let mut status = 0;
        // SAFETY: our own child; WNOHANG never blocks.
        let reaped = unsafe { libc::waitpid(child_pid, &mut status, libc::WNOHANG) };
        if reaped == child_pid {
            break decode(status);
        }
        if checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            // Nobody can reach a host whose socket is gone or replaced: it would leave an
            // unmanaged writer behind. Hang up the program, then insist.
            let reachable = fs::symlink_metadata(dir.join(SOCKET))
                .is_ok_and(|m| (m.dev(), m.ino()) == identity);
            match orphaned {
                None if !reachable => {
                    orphaned = Some(Instant::now());
                    // SAFETY: our own unreaped child's process group.
                    unsafe { libc::kill(-child_pid, libc::SIGHUP) };
                },
                Some(since) if since.elapsed() >= Duration::from_secs(5) => {
                    // SAFETY: as above.
                    unsafe { libc::kill(-child_pid, libc::SIGKILL) };
                },
                _ => {},
            }
        }
    };
    if orphaned.is_some() {
        // The directory is not ours any more; never recreate or write into it.
        return Ok(());
    }
    // Descendants may keep the slave open; only drain what is already buffered.
    state.drain_master();
    publish(dir, EXIT, &serde_json::to_vec(&exit)?)?;
    let _ = fs::remove_file(dir.join(SOCKET));
    if let Some(client) = &mut state.client {
        let _ = write_frame(client, frame::EXIT, &serde_json::to_vec(&exit)?);
    }
    Ok(())
}

struct State {
    master: OwnedFd,
    master_open: bool,
    client: Option<UnixStream>,
    replay: Replay,
    token: String,
    host_pid: i32,
    child_pid: i32,
}

impl State {
    fn poll(&mut self, listener: &UnixListener) -> Result<()> {
        let mut fds =
            vec![libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if self.master_open {
            fds.push(libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(client) = &self.client {
            fds.push(libc::pollfd { fd: client.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        // SAFETY: valid array of pollfd for its whole length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };
        if ready == -1 {
            let error = io::Error::last_os_error();
            return if error.kind() == ErrorKind::Interrupted { Ok(()) } else { Err(error.into()) };
        }
        let readable = |fd: RawFd| fds.iter().any(|p| p.fd == fd && p.revents != 0);
        if self.master_open && readable(self.master.as_raw_fd()) {
            self.drain_master();
        }
        if let Some(client) = &self.client
            && readable(client.as_raw_fd())
        {
            self.client_input();
        }
        if readable(listener.as_raw_fd()) {
            match listener.accept() {
                Ok((stream, _)) => self.admit(stream),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {},
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn drain_master(&mut self) {
        let mut buffer = [0; 16 * 1024];
        for _ in 0..64 {
            // SAFETY: valid descriptor and buffer.
            let read = unsafe {
                libc::read(self.master.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
            };
            if read > 0 {
                let data = &buffer[..read as usize];
                self.replay.push(data);
                if let Some(client) = &mut self.client
                    && write_frame(client, frame::DATA, data).is_err()
                {
                    self.client = None;
                }
                continue;
            }
            let error = io::Error::last_os_error();
            if read == 0
                || (error.kind() != ErrorKind::WouldBlock && error.kind() != ErrorKind::Interrupted)
            {
                // EOF/EIO: no process holds the slave any more.
                self.master_open = false;
            }
            return;
        }
    }

    fn write_master(&mut self, mut data: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + IO_TIMEOUT;
        while !data.is_empty() {
            // SAFETY: valid descriptor and buffer.
            let written =
                unsafe { libc::write(self.master.as_raw_fd(), data.as_ptr().cast(), data.len()) };
            if written > 0 {
                data = &data[written as usize..];
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() != ErrorKind::WouldBlock && error.kind() != ErrorKind::Interrupted {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(ErrorKind::TimedOut, "program is not reading input"));
            }
            // Keep the program's output moving while its input is full.
            self.drain_master();
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn client_input(&mut self) {
        let Some(client) = &mut self.client else { return };
        let message = read_frame(client);
        let keep = match message {
            Ok((frame::DATA, data)) => self.write_master(&data).is_ok(),
            Ok((frame::RESIZE, bytes)) => match parse_size(&bytes) {
                Some(size) => {
                    if let Ok(false) = set_size(self.master.as_raw_fd(), size) {
                        redraw(self.master.as_raw_fd());
                    }
                    true
                },
                None => false,
            },
            // EOF is a detach; anything else is a protocol violation.
            _ => false,
        };
        if !keep {
            self.client = None;
        }
    }

    fn admit(&mut self, mut stream: UnixStream) {
        let result = (|| -> io::Result<()> {
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            let mut hello = vec![0; MAGIC.len() + TOKEN_HEX + 2];
            stream.read_exact(&mut hello)?;
            let token = &hello[MAGIC.len()..MAGIC.len() + TOKEN_HEX];
            if !hello.starts_with(MAGIC)
                || hello[MAGIC.len() + TOKEN_HEX] != b'\n'
                || !equal(token, self.token.as_bytes())
            {
                return write_frame(&mut stream, frame::ERROR, b"authentication failed");
            }
            match hello[MAGIC.len() + TOKEN_HEX + 1] {
                ROLE_ATTACH => {
                    let mut size = [0; 4];
                    stream.read_exact(&mut size)?;
                    let Some(size) = parse_size(&size) else {
                        return write_frame(&mut stream, frame::ERROR, b"invalid size");
                    };
                    if self.client.is_some() {
                        return write_frame(&mut stream, frame::ERROR, b"already attached");
                    }
                    write_frame(&mut stream, frame::OK, &[])?;
                    write_frame(&mut stream, frame::REPLAY, &self.replay.snapshot())?;
                    // A program that suspended itself has no shell here to resume it.
                    // SAFETY: our own child's process group.
                    unsafe { libc::kill(-self.child_pid, libc::SIGCONT) };
                    if !set_size(self.master.as_raw_fd(), size)? {
                        redraw(self.master.as_raw_fd());
                    }
                    self.client = Some(stream.try_clone()?);
                    Ok(())
                },
                ROLE_CONTROL => self.control(&mut stream),
                _ => write_frame(&mut stream, frame::ERROR, b"unknown role"),
            }
        })();
        if result.is_err() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn control(&mut self, stream: &mut UnixStream) -> io::Result<()> {
        match read_frame(stream)? {
            (frame::STATUS, _) => {
                let status = Status {
                    host_pid: self.host_pid,
                    child_pid: self.child_pid,
                    attached: self.client.is_some(),
                };
                write_frame(stream, frame::JSON, &serde_json::to_vec(&status)?)
            },
            (frame::SIGNAL, bytes) => {
                let signal = <[u8; 4]>::try_from(bytes.as_slice()).map(i32::from_be_bytes);
                match signal {
                    Ok(signal @ (libc::SIGHUP | libc::SIGINT | libc::SIGTERM | libc::SIGKILL)) => {
                        // The program is its session's leader: signal its whole process group.
                        // SAFETY: our own unreaped child's group.
                        unsafe { libc::kill(-self.child_pid, signal) };
                        write_frame(stream, frame::OK, &[])
                    },
                    _ => write_frame(stream, frame::ERROR, b"signal not allowed"),
                }
            },
            _ => write_frame(stream, frame::ERROR, b"unknown control request"),
        }
    }
}

// -------------------------------------------------------------------------------------- client

fn connect(dir: &Path, token: &str, role: u8, size: Option<Size>) -> Result<UnixStream> {
    if token.len() != TOKEN_HEX {
        return Err(Error::Protocol("invalid PTY host token"));
    }
    let mut stream = UnixStream::connect(dir.join(SOCKET))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut hello = MAGIC.to_vec();
    hello.extend_from_slice(token.as_bytes());
    hello.push(b'\n');
    hello.push(role);
    if let Some(size) = size {
        hello.extend_from_slice(&size_bytes(size));
    }
    stream.write_all(&hello)?;
    Ok(stream)
}

fn refused(payload: &[u8]) -> Error {
    conflict(&format!("PTY host refused: {}", String::from_utf8_lossy(payload)))
}

fn control(dir: &Path, kind: u8, payload: &[u8]) -> Result<(u8, Vec<u8>)> {
    let mut stream = connect(dir, &read_token(dir)?, ROLE_CONTROL, None)?;
    write_frame(&mut stream, kind, payload)?;
    let (kind, payload) = read_frame(&mut stream)?;
    if kind == frame::ERROR {
        return Err(refused(&payload));
    }
    Ok((kind, payload))
}

pub fn status(dir: &Path) -> Result<Status> {
    match control(dir, frame::STATUS, &[])? {
        (frame::JSON, payload) => Ok(serde_json::from_slice(&payload)?),
        _ => Err(Error::Protocol("unexpected PTY host status reply")),
    }
}

/// Relay HUP, INT, TERM or KILL to the hosted program's process group.
pub fn signal(dir: &Path, signal: i32) -> Result<()> {
    control(dir, frame::SIGNAL, &signal.to_be_bytes()).map(|_| ())
}

/// The durable exit record, once the hosted program has been reaped.
pub fn exit_status(dir: &Path) -> Result<Option<Exit>> {
    match read_private(dir, EXIT, 4096) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// An exclusive attachment. Dropping it detaches; the program keeps running.
pub struct Attachment {
    stream: UnixStream,
    replay: String,
    output: Vec<u8>,
    exit: Option<Exit>,
}

impl Drop for Attachment {
    /// Orderly detach. Closing a Unix socket that still holds unread output can reset the
    /// connection and discard input the host has not read yet (observed on macOS), so signal
    /// end-of-input first and drain until the host closes its side.
    fn drop(&mut self) {
        if self.stream.shutdown(std::net::Shutdown::Write).is_err() {
            return;
        }
        let _ = self.stream.set_read_timeout(Some(Duration::from_millis(200)));
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut sink = [0; 16 * 1024];
        while Instant::now() < deadline {
            match self.stream.read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {},
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {},
                Err(_) => return,
            }
        }
    }
}

pub fn attach(dir: &Path, size: Size) -> Result<Attachment> {
    attach_with_token(dir, &read_token(dir)?, size)
}

pub fn attach_with_token(dir: &Path, token: &str, size: Size) -> Result<Attachment> {
    let mut stream = connect(dir, token, ROLE_ATTACH, Some(size))?;
    match read_frame(&mut stream)? {
        (frame::OK, _) => {},
        (frame::ERROR, payload) => return Err(refused(&payload)),
        _ => return Err(Error::Protocol("unexpected PTY host reply")),
    }
    let (kind, replay) = read_frame(&mut stream)?;
    if kind != frame::REPLAY {
        return Err(Error::Protocol("PTY host did not send its replay"));
    }
    Ok(Attachment {
        stream,
        replay: String::from_utf8_lossy(&replay).into_owned(),
        output: Vec::new(),
        exit: None,
    })
}

impl Attachment {
    /// Output recorded before this attachment.
    pub fn replay(&self) -> &str {
        &self.replay
    }

    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(MAX_FRAME) {
            write_frame(&mut self.stream, frame::DATA, chunk)?;
        }
        Ok(())
    }

    pub fn resize(&mut self, size: Size) -> Result<()> {
        Ok(write_frame(&mut self.stream, frame::RESIZE, &size_bytes(size))?)
    }

    /// Next live frame: `Some(bytes)` for output, `None` once the program has exited.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        if self.exit.is_some() {
            return Ok(None);
        }
        self.stream.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match read_frame(&mut self.stream)? {
            (frame::DATA, data) => {
                self.output.extend_from_slice(&data);
                Ok(Some(data))
            },
            (frame::EXIT, payload) => {
                self.exit = Some(serde_json::from_slice(&payload)?);
                Ok(None)
            },
            _ => Err(Error::Protocol("unexpected PTY host frame")),
        }
    }

    /// Replay plus live output, once it contains `needle`.
    pub fn read_until(&mut self, needle: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let text = format!("{}{}", self.replay, String::from_utf8_lossy(&self.output));
            if text.contains(needle) {
                return Ok(text);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.next(left)?.is_none() {
                return Err(Error::Protocol("PTY output did not arrive in time"));
            }
        }
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Result<Exit> {
        let deadline = Instant::now() + timeout;
        while self.exit.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Protocol("PTY program did not exit in time"));
            }
            self.next(left)?;
        }
        self.exit.ok_or(Error::Protocol("missing exit"))
    }
}

// --------------------------------------------------------------------------------- interactive

/// Bytes that end an interactive attachment: Ctrl+] and Ctrl+Z. A hosted program has no shell to
/// resume it after a suspend, so Ctrl+Z detaches instead (as it did for native background TUIs).
pub const DETACH_KEYS: [u8; 2] = [0x1d, 0x1a];

fn terminal_size(fd: RawFd) -> Option<Size> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor and output buffer.
    (unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size) } == 0
        && size.ws_row > 0
        && size.ws_col > 0)
        .then_some(Size { rows: size.ws_row, cols: size.ws_col })
}

/// Raw terminal mode for the duration of an attachment; restored on every exit path.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable(fd: RawFd) -> Self {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor and output buffer; a non-terminal input is left unchanged.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Self(None);
        }
        let mut raw = original;
        // SAFETY: cfmakeraw only edits the local copy; tcsetattr applies it.
        unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
        }
        Self(Some(original))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(original) = &self.0 {
            // SAFETY: restores the attributes read from this same descriptor.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, original) };
        }
    }
}

/// `xaya-agentd pty-attach`: connect this terminal to a host until detach or program exit.
pub fn interactive(dir: &Path) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let size = terminal_size(1).unwrap_or(Size { rows: 24, cols: 80 });
    let mut attachment = attach(dir, size)?;
    let resized = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(libc::SIGWINCH, Arc::clone(&resized))?;
    let raw = RawMode::enable(0);
    let mut stdout = io::stdout().lock();
    stdout.write_all(attachment.replay.as_bytes())?;
    stdout.flush()?;
    let detached = Arc::new(AtomicBool::new(false));
    let mut writer = attachment.stream.try_clone()?;
    let input_detached = Arc::clone(&detached);
    // Blocking stdin reads stay on their own thread; it ends with the process if still blocked.
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            // SAFETY: valid descriptor and buffer.
            let read = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                input_detached.store(true, Ordering::Release);
                return;
            }
            let data = &buffer[..read as usize];
            let end = data.iter().position(|b| DETACH_KEYS.contains(b));
            let forwarded = &data[..end.unwrap_or(data.len())];
            if !forwarded.is_empty() && write_frame(&mut writer, frame::DATA, forwarded).is_err() {
                input_detached.store(true, Ordering::Release);
                return;
            }
            if end.is_some() {
                input_detached.store(true, Ordering::Release);
                return;
            }
        }
    });
    let exited = loop {
        if detached.load(Ordering::Acquire) {
            break false;
        }
        if resized.swap(false, Ordering::AcqRel)
            && let Some(size) = terminal_size(1)
        {
            attachment.resize(size)?;
        }
        let mut poll =
            libc::pollfd { fd: attachment.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd. Polling first keeps partial frames out of timeouts.
        if unsafe { libc::poll(&mut poll, 1, 100) } <= 0 {
            continue;
        }
        match attachment.next(IO_TIMEOUT) {
            Ok(Some(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            },
            Ok(None) => break true,
            Err(_) => break false,
        }
    };
    drop(raw);
    let notice = if exited {
        "\r\n[XaYa: the session program exited]\r\n"
    } else {
        "\r\n[XaYa: detached; the session keeps running. Reattach with xaya attach]\r\n"
    };
    stdout.write_all(notice.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_drops_oldest_output_at_a_line_boundary() {
        let mut replay = Replay { bytes: VecDeque::new(), dropped: false };
        replay.push(b"first\n");
        assert_eq!(replay.snapshot(), b"first\n");
        let line = [[b'x'; 99].as_slice(), b"\n"].concat();
        for _ in 0..(REPLAY_LIMIT / line.len() + 10) {
            replay.push(&line);
        }
        replay.push(b"\x1b[31mlast\n");
        let snapshot = replay.snapshot();
        assert!(snapshot.len() <= REPLAY_LIMIT);
        assert!(snapshot.starts_with(&line), "starts on a whole line");
        assert!(snapshot.ends_with(b"\x1b[31mlast\n"));
    }

    #[test]
    fn frames_round_trip_and_reject_oversize() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame::DATA, b"abc").unwrap();
        assert_eq!(read_frame(&mut bytes.as_slice()).unwrap(), (frame::DATA, b"abc".to_vec()));
        let mut oversized = vec![frame::DATA];
        oversized.extend_from_slice(&((MAX_FRAME + 1) as u32).to_be_bytes());
        assert!(read_frame(&mut oversized.as_slice()).is_err());
        assert_eq!(
            parse_size(&size_bytes(Size { rows: 40, cols: 120 })),
            Some(Size { rows: 40, cols: 120 })
        );
        assert_eq!(parse_size(&[0, 0, 0, 80]), None);
    }

    #[test]
    fn exit_status_distinguishes_codes_from_signals() {
        assert_eq!(decode(7 << 8), Exit { code: Some(7), signal: None });
        assert_eq!(decode(libc::SIGTERM), Exit { code: None, signal: Some(libc::SIGTERM) });
    }
}
