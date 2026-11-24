//! Bounded newline frames with a total deadline, including slow trickling peers.

use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use polling::{Event, Events, Poller};
use serde::Serialize;
use xaya_agent_proto::MAX_FRAME_BYTES;

pub const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// A review may contain the complete findings from two engines. Only review responses
/// use this larger limit; requests, events and all other responses keep the normal bound.
pub(crate) fn response_limit(method: &str) -> usize {
    match method {
        xaya_agent_proto::REVIEW_GET
        | xaya_agent_proto::REVIEW_START
        | xaya_agent_proto::REVIEW_CANCEL => 1024 * 1024,
        xaya_agent_proto::REVIEW_LIST | xaya_agent_proto::TASK_BOARD => 4 * 1024 * 1024,
        // A review and a diff of up to 256 KiB, JSON-escaped.
        xaya_agent_proto::TASK_ACCEPTANCE => 4 * 1024 * 1024,
        _ => MAX_FRAME_BYTES,
    }
}

pub fn read_frame(reader: &mut BufReader<UnixStream>, timeout: Duration) -> io::Result<Vec<u8>> {
    read_frame_with_limit(reader, timeout, MAX_FRAME_BYTES)
}

pub(crate) fn read_frame_with_limit(
    reader: &mut BufReader<UnixStream>,
    timeout: Duration,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    reader.get_ref().set_nonblocking(true)?;
    let poller = Poller::new()?;
    // SAFETY: the reader owns the registered stream and outlives this local poller on all paths.
    unsafe { poller.add(reader.get_ref(), Event::readable(0))? };
    let mut events = Events::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(ErrorKind::TimedOut, "protocol frame timed out"));
        }
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                poller.modify(reader.get_ref(), Event::readable(0))?;
                events.clear();
                poller.wait(&mut events, Some(remaining))?;
                continue;
            },
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "incomplete protocol frame"));
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.unwrap_or(available.len());
        if bytes.len() + count > limit {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "protocol frame exceeds size limit",
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            reader.get_ref().set_nonblocking(false)?;
            return Ok(bytes);
        }
    }
}

pub fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> io::Result<()> {
    write_frame_with_limit(stream, value, MAX_FRAME_BYTES)
}

pub(crate) fn write_frame_with_limit(
    stream: &mut UnixStream,
    value: &impl Serialize,
    limit: usize,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        return Err(io::Error::new(ErrorKind::InvalidData, "protocol frame exceeds size limit"));
    }
    bytes.push(b'\n');
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut remaining_bytes = bytes.as_slice();
    while !remaining_bytes.is_empty() {
        let remaining_time = deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Err(io::Error::new(ErrorKind::TimedOut, "protocol write timed out"));
        }
        stream.set_write_timeout(Some(remaining_time))?;
        match stream.write(remaining_bytes) {
            Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "protocol peer closed")),
            Ok(count) => remaining_bytes = &remaining_bytes[count..],
            Err(error) if error.kind() == ErrorKind::Interrupted => (),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn frames_remain_separate() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut server = BufReader::new(server);
        client.write_all(b"one\ntwo\n").unwrap();
        assert_eq!(read_frame(&mut server, IO_TIMEOUT).unwrap(), b"one");
        assert_eq!(read_frame(&mut server, IO_TIMEOUT).unwrap(), b"two");
    }

    #[test]
    fn trickling_does_not_extend_total_deadline() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut server = BufReader::new(server);
        let writer = thread::spawn(move || {
            for _ in 0..10 {
                if client.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
        });
        let start = Instant::now();
        assert!(read_frame(&mut server, Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_millis(500));
        drop(server);
        writer.join().unwrap();
    }

    #[test]
    fn oversize_and_partial_eof_are_rejected() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut server = BufReader::new(server);
        let writer = thread::spawn(move || {
            let _ = client.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]);
        });
        assert_eq!(read_frame(&mut server, IO_TIMEOUT).unwrap_err().kind(), ErrorKind::InvalidData);
        drop(server);
        writer.join().unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut server = BufReader::new(server);
        client.write_all(b"partial").unwrap();
        drop(client);
        assert_eq!(
            read_frame(&mut server, IO_TIMEOUT).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }
}
