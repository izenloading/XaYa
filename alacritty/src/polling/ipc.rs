//! Alacritty socket IPC.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Result as IoResult, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs};

use log::{error, warn};
use polling::{Event as PollEvent, Events, Poller};
use std::result::Result;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::cli::{IpcStatus, Options, SocketMessage};
use crate::event::{Event, EventType};
use crate::window_status::WindowStatus;

/// Environment variable name for the IPC socket path.
const ALACRITTY_SOCKET_ENV: &str = "ALACRITTY_SOCKET";

/// IPC socket listener.
pub struct IpcListener {
    pub socket: UnixListener,

    event_proxy: EventLoopProxy<Event>,
    data: String,
}

impl IpcListener {
    pub fn new(
        options: &Options,
        event_proxy: EventLoopProxy<Event>,
        path: &Path,
    ) -> Result<Self, IoError> {
        // Create unix socket in nonblocking mode.
        let socket = UnixListener::bind(path)?;
        socket.set_nonblocking(true)?;

        // Register socket path as environment variable for `alacritty msg`.
        unsafe { env::set_var(ALACRITTY_SOCKET_ENV, path.as_os_str()) };
        if options.daemon {
            println!("ALACRITTY_SOCKET={}; export ALACRITTY_SOCKET", path.display());
        }

        Ok(Self { event_proxy, socket, data: Default::default() })
    }

    /// Process the next IPC message.
    pub fn process_message(&mut self) -> Result<(), IoError> {
        let (stream, _) = self.socket.accept()?;

        match read_message(&stream, &mut self.data) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => (),
        };

        let message: SocketMessage = match serde_json::from_str(&self.data) {
            Ok(message) => message,
            Err(err) => {
                warn!("Failed to parse IPC message: {err}");
                return Ok(());
            },
        };

        // Handle IPC events.
        match message {
            SocketMessage::CreateWindow(options) => {
                let event = Event::new(EventType::CreateWindow(options), None);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::Config(ipc_config) => {
                let window_id =
                    ipc_config.window_id.and_then(|id| u64::try_from(id).ok()).map(WindowId::from);
                let event = Event::new(EventType::IpcConfig(ipc_config), window_id);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::GetConfig(config) => {
                let window_id =
                    config.window_id.and_then(|id| u64::try_from(id).ok()).map(WindowId::from);
                let event = Event::new(EventType::IpcGetConfig(Arc::new(stream)), window_id);
                let _ = self.event_proxy.send_event(event);
            },
            SocketMessage::Status(_) => {
                let event = Event::new(EventType::IpcStatus(Arc::new(stream)), None);
                let _ = self.event_proxy.send_event(event);
            },
            // Requests addressing one window are answered by the event loop.
            message @ (SocketMessage::Focus(_)
            | SocketMessage::GetText(_)
            | SocketMessage::SendText(_)
            | SocketMessage::CreateWorktree(_)
            | SocketMessage::Split(_)) => {
                let event = Event::new(EventType::IpcRequest(message, Arc::new(stream)), None);
                let _ = self.event_proxy.send_event(event);
            },
        }

        Ok(())
    }
}

/// Read one newline- or EOF-terminated IPC request on the listener thread.
fn read_message(stream: &UnixStream, data: &mut String) -> IoResult<usize> {
    read_message_with_timeout(stream, data, Duration::from_secs(1))
}

fn read_message_with_timeout(
    stream: &UnixStream,
    data: &mut String,
    timeout: Duration,
) -> IoResult<usize> {
    // Wait explicitly for client bytes. BSD can reject SO_RCVTIMEO changes
    // after the peer has already closed, even with a complete request buffered.
    // Polling also enforces a total deadline against stalled or trickling clients.
    stream.set_nonblocking(true)?;
    data.clear();
    let deadline = Instant::now() + timeout;
    let poller = Poller::new()?;
    // SAFETY: the borrowed stream outlives this local poller, including all early
    // returns. The poller is destroyed before the caller can close the stream.
    unsafe { poller.add(stream, PollEvent::readable(0))? };
    let mut events = Events::new();
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(IoError::new(ErrorKind::TimedOut, "IPC request timed out"));
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                poller.modify(stream, PollEvent::readable(0))?;
                events.clear();
                poller.wait(&mut events, Some(remaining))?;
                continue;
            },
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            break;
        }
        let end = available.iter().position(|&byte| byte == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(available.len());
        if bytes.len() + count > 1024 * 1024 {
            return Err(IoError::new(ErrorKind::InvalidData, "IPC request exceeds 1 MiB"));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            break;
        }
    }
    // GetConfig is replied to asynchronously using the existing blocking writer.
    stream.set_nonblocking(false)?;
    *data = String::from_utf8(bytes).map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
    Ok(data.len())
}

/// Send a message to the active Alacritty socket.
pub fn send_message(socket: Option<PathBuf>, message: SocketMessage) -> IoResult<()> {
    let mut socket = find_socket(socket)?;

    // Write message to socket.
    let message_json = serde_json::to_string(&message)?;
    socket.write_all(message_json.as_bytes())?;
    let _ = socket.flush();

    // Shutdown write end, to allow reading.
    socket.shutdown(Shutdown::Write)?;

    // Get matching IPC reply.
    handle_reply(&socket, &message)?;

    Ok(())
}

/// Ask the running xaya what the programs in each of its windows are doing.
pub fn request_status(socket: Option<PathBuf>) -> IoResult<Vec<WindowStatus>> {
    let mut stream = find_socket(socket)?;
    let message = serde_json::to_string(&SocketMessage::Status(IpcStatus::default()))?;
    stream.write_all(message.as_bytes())?;
    stream.shutdown(Shutdown::Write)?;

    let mut buffer = String::new();
    BufReader::new(&stream).read_line(&mut buffer)?;
    if buffer.is_empty() {
        return Err(IoError::other("no reply; the running xaya may predate `msg status`"));
    }
    match serde_json::from_str(&buffer) {
        Ok(SocketReply::Status(windows)) => Ok(windows),
        Ok(_) => Err(IoError::other("unexpected IPC reply")),
        Err(err) => Err(IoError::other(format!("Invalid IPC format: {err}"))),
    }
}

/// Process IPC responses.
fn handle_reply(stream: &UnixStream, message: &SocketMessage) -> IoResult<()> {
    // Read reply, returning early if there is none.
    let mut buffer = String::new();
    let mut reader = BufReader::new(stream);
    if let Ok(0) | Err(_) = reader.read_line(&mut buffer) {
        return Ok(());
    }

    // Parse IPC reply.
    let reply: SocketReply = serde_json::from_str(&buffer)
        .map_err(|err| IoError::other(format!("Invalid IPC format: {err}")))?;

    // Ensure reply matches request.
    match (message, &reply) {
        // Write requested config to STDOUT.
        (SocketMessage::GetConfig(..), SocketReply::GetConfig(config)) => {
            println!("{config}");
            Ok(())
        },
        (
            SocketMessage::GetText(..) | SocketMessage::CreateWorktree(..),
            SocketReply::Text(text),
        ) => {
            println!("{text}");
            Ok(())
        },
        (_, SocketReply::Error(error)) => Err(IoError::other(error.clone())),
        // Ignore requests without reply.
        _ => Ok(()),
    }
}

/// Send IPC message reply.
pub fn send_reply(stream: &mut UnixStream, message: SocketReply) {
    if let Err(err) = send_reply_fallible(stream, message) {
        error!("Failed to send IPC reply: {err}");
    }
}

/// Send IPC message reply, returning possible errors.
fn send_reply_fallible(stream: &mut UnixStream, message: SocketReply) -> IoResult<()> {
    let json = serde_json::to_string(&message).map_err(IoError::other)?;
    stream.write_all(json.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Directory for the IPC socket file.
#[cfg(not(target_os = "macos"))]
pub fn socket_dir() -> PathBuf {
    xdg::BaseDirectories::with_prefix("alacritty")
        .get_runtime_directory()
        .map(ToOwned::to_owned)
        .ok()
        .and_then(|path| fs::create_dir_all(&path).map(|_| path).ok())
        .unwrap_or_else(env::temp_dir)
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
pub fn socket_dir() -> PathBuf {
    env::temp_dir()
}

/// Find the IPC socket path.
fn find_socket(socket_path: Option<PathBuf>) -> IoResult<UnixStream> {
    // Handle --socket CLI override.
    if let Some(socket_path) = socket_path {
        // Ensure we inform the user about an invalid path.
        return UnixStream::connect(&socket_path).map_err(|err| {
            let message = format!("invalid socket path {socket_path:?}");
            IoError::new(err.kind(), message)
        });
    }

    // Handle environment variable.
    if let Ok(path) = env::var(ALACRITTY_SOCKET_ENV) {
        let socket_path = PathBuf::from(path);
        if let Ok(socket) = UnixStream::connect(socket_path) {
            return Ok(socket);
        }
    }

    // Search for sockets files.
    for entry in fs::read_dir(socket_dir())?.filter_map(|entry| entry.ok()) {
        let path = entry.path();

        // Skip files that aren't Alacritty sockets.
        let socket_prefix = socket_prefix();
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|file| file.starts_with(&socket_prefix) && file.ends_with(".sock"))
            .is_none()
        {
            continue;
        }

        // Attempt to connect to the socket.
        match UnixStream::connect(&path) {
            Ok(socket) => return Ok(socket),
            // Delete orphan sockets.
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs::remove_file(&path);
            },
            // Ignore other errors like permission issues.
            Err(_) => (),
        }
    }

    Err(IoError::new(ErrorKind::NotFound, "no socket found"))
}

/// File prefix matching all available sockets.
///
/// This prefix will include display server information to allow for environments with multiple
/// display servers running for the same user.
#[cfg(not(target_os = "macos"))]
pub fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY").or_else(|_| env::var("DISPLAY")).unwrap_or_default();
    format!("Alacritty-{}", display.replace('/', "-"))
}

/// File prefix matching all available sockets.
#[cfg(target_os = "macos")]
pub fn socket_prefix() -> String {
    String::from("Alacritty")
}

/// IPC socket replies.
#[derive(Serialize, Deserialize, Debug)]
pub enum SocketReply {
    GetConfig(String),
    Status(Vec<WindowStatus>),
    /// Text requested with `get-text`.
    Text(String),
    /// Why a request failed.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    use alacritty_terminal::program_status::{ProgramEvent, parse_report};

    use crate::program_status::ProgramActivity;

    /// Window `id` after `reports`.
    fn window(id: u64, reports: &[&str]) -> WindowStatus {
        let mut activity = ProgramActivity::default();
        for body in reports {
            activity.handle(ProgramEvent::Report(parse_report(body).unwrap()), false);
        }
        WindowStatus::new(id, format!("window {id}"), false, &activity, Instant::now())
    }

    #[test]
    fn reads_a_delayed_request_from_a_nonblocking_accepted_stream() {
        let (server, mut client) = UnixStream::pair().unwrap();
        // BSD accepts can inherit O_NONBLOCK from the listening socket.
        server.set_nonblocking(true).unwrap();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            client.write_all(b"{\"GetConfig\":").unwrap();
            thread::sleep(Duration::from_millis(20));
            client.write_all(b"{\"window_id\":null}}\n").unwrap();
        });
        let mut data = String::new();
        let result = read_message(&server, &mut data);
        writer.join().unwrap();
        assert_eq!(result.unwrap(), data.len());
        assert!(matches!(
            serde_json::from_str::<SocketMessage>(&data).unwrap(),
            SocketMessage::GetConfig(_)
        ));
    }

    #[test]
    fn status_requests_and_replies_round_trip() {
        // Client-only flags stay on the client.
        let status = IpcStatus { json: true, wait: true, watch: true };
        let json = serde_json::to_string(&SocketMessage::Status(status)).unwrap();
        assert_eq!(json, r#"{"Status":{}}"#);
        assert!(matches!(
            serde_json::from_str::<SocketMessage>(&json).unwrap(),
            SocketMessage::Status(IpcStatus { json: false, wait: false, watch: false })
        ));

        let windows = vec![window(7, &["state=done:app=codex"])];
        let reply = serde_json::to_string(&SocketReply::Status(windows.clone())).unwrap();
        match serde_json::from_str(&reply).unwrap() {
            SocketReply::Status(parsed) => assert_eq!(parsed, windows),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    #[test]
    fn status_reaches_the_event_loop_with_its_stream() {
        let dir = std::env::temp_dir().join(format!("xaya-ipc-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("status.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let client = thread::spawn({
            let path = path.clone();
            move || request_status(Some(path))
        });

        // Play the event loop's part: read the request and answer it.
        let (stream, _) = listener.accept().unwrap();
        let mut data = String::new();
        read_message(&stream, &mut data).unwrap();
        assert!(matches!(serde_json::from_str(&data).unwrap(), SocketMessage::Status(_)));
        let windows = vec![window(9, &["state=working:app=claude-code"])];
        send_reply(&mut stream.try_clone().unwrap(), SocketReply::Status(windows.clone()));
        drop(stream);

        assert_eq!(client.join().unwrap().unwrap(), windows);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_from_a_xaya_without_it_fails_clearly() {
        let dir = std::env::temp_dir().join(format!("xaya-ipc-old-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("old.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = thread::spawn({
            let path = path.clone();
            move || request_status(Some(path))
        });
        // An older build reads a message it can't parse, then hangs up without replying.
        let (stream, _) = listener.accept().unwrap();
        read_message(&stream, &mut String::new()).unwrap();
        drop(stream);
        let err = client.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("predate"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stalled_client_has_a_bounded_read_deadline() {
        let (server, _client) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let err = read_message_with_timeout(&server, &mut String::new(), Duration::from_millis(30))
            .unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn reads_a_buffered_request_after_the_client_has_disconnected() {
        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"{\"GetConfig\":{\"window_id\":null}}\n").unwrap();
        drop(client);
        let mut data = String::new();
        assert_eq!(read_message(&server, &mut data).unwrap(), data.len());
        assert!(matches!(
            serde_json::from_str::<SocketMessage>(&data).unwrap(),
            SocketMessage::GetConfig(_)
        ));
    }

    #[test]
    fn accepts_eof_terminated_legacy_requests() {
        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"{\"GetConfig\":{\"window_id\":null}}").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut data = String::new();
        assert_eq!(read_message(&server, &mut data).unwrap(), data.len());
        assert!(matches!(
            serde_json::from_str::<SocketMessage>(&data).unwrap(),
            SocketMessage::GetConfig(_)
        ));
    }
}
