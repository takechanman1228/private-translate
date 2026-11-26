//! Readers for a child's pipes, shared by `run` and `locate`.
//!
//! Each pipe is read on a thread of its own that waits with `poll` for at most [`SLICE`] at a
//! time and checks a stop flag in between. So a reader can always be stopped and joined, and
//! the pipe's descriptor closed, even when a grandchild still holds the other end open: no
//! thread or descriptor outlives the call that started the child. Dropping a [`Reader`] stops
//! and joins it.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The longest a reader waits for data before it checks its stop flag again.
pub(crate) const SLICE: Duration = Duration::from_millis(50);

/// A pipe being read on its own thread into a value of type `T`.
pub(crate) struct Reader<T> {
    stop: Arc<AtomicBool>,
    eof: Arc<AtomicBool>,
    handle: Option<JoinHandle<T>>,
}

impl<T> Reader<T> {
    /// Waits up to `grace` for the end of the pipe, then stops the reader and joins it. Returns
    /// the value, and whether the pipe reached its end (`false`: something still held it open).
    pub(crate) fn finish(mut self, grace: Duration) -> (T, bool) {
        let started = Instant::now();
        while !self.eof.load(Ordering::SeqCst) && started.elapsed() < grace {
            thread::sleep(Duration::from_millis(10));
        }
        let eof = self.eof.load(Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
        let value = self
            .handle
            .take()
            .expect("joined once")
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
        (value, eof)
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Reads `pipe` on a new thread, handing each chunk to `on_bytes` with `state`. An error means
/// the thread could not be started (the pipe is then closed).
pub(crate) fn spawn<P, T, F>(pipe: P, mut state: T, mut on_bytes: F) -> std::io::Result<Reader<T>>
where
    P: Read + AsRawFd + Send + 'static,
    T: Send + 'static,
    F: FnMut(&mut T, &[u8]) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let (thread_stop, thread_eof) = (stop.clone(), eof.clone());
    let handle = thread::Builder::new()
        .name("babeldoc-pipe".into())
        .spawn(move || {
            let mut pipe = pipe;
            let mut chunk = [0u8; 8192];
            while !thread_stop.load(Ordering::SeqCst) {
                if !readable(pipe.as_raw_fd(), SLICE) {
                    continue;
                }
                match pipe.read(&mut chunk) {
                    Ok(0) => {
                        thread_eof.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(n) => on_bytes(&mut state, &chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        thread_eof.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
            // `pipe` is dropped here: the descriptor closes with the thread.
            state
        })?;
    Ok(Reader {
        stop,
        eof,
        handle: Some(handle),
    })
}

/// Whether `fd` has data, an end or an error to read within `timeout`.
fn readable(fd: std::os::fd::RawFd, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: one valid `pollfd` for a descriptor this thread owns.
    let ready = unsafe { libc::poll(&mut poll, 1, millis) };
    ready > 0 && poll.revents != 0
}

/// Reads `pipe` to its end, keeping at most `limit` bytes (the rest is read and dropped so that
/// the child never blocks on a full pipe). The value is `(bytes, truncated)`.
pub(crate) fn read_all<P>(pipe: P, limit: usize) -> std::io::Result<Reader<(Vec<u8>, bool)>>
where
    P: Read + AsRawFd + Send + 'static,
{
    spawn(
        pipe,
        (Vec::new(), false),
        move |(kept, truncated), bytes| {
            let room = limit.saturating_sub(kept.len());
            if bytes.len() > room {
                *truncated = true;
            }
            kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
        },
    )
}

/// Splits `pipe` into lines of at most `limit` bytes (longer lines are dropped whole) and hands
/// each complete line to `on_line` on the reader's thread. The reader never waits for whoever
/// consumes the lines, so the child never blocks on a full pipe.
pub(crate) fn read_lines<P, F>(pipe: P, limit: usize, mut on_line: F) -> std::io::Result<Reader<()>>
where
    P: Read + AsRawFd + Send + 'static,
    F: FnMut(&[u8]) + Send + 'static,
{
    let mut line = Vec::new();
    let mut too_long = false;
    spawn(pipe, (), move |(), bytes| {
        for &byte in bytes {
            if byte == b'\n' {
                if !too_long {
                    on_line(&line);
                }
                line.clear();
                too_long = false;
            } else if line.len() < limit {
                line.push(byte);
            } else {
                too_long = true;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// A grandchild that keeps the pipe open does not keep the reader alive.
    #[test]
    fn a_reader_stops_even_when_a_grandchild_holds_the_pipe() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "echo first; (sleep 3 &) ; exit 0"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let reader = read_all(child.stdout.take().unwrap(), 1024).unwrap();
        child.wait().unwrap();
        let started = Instant::now();
        let ((bytes, truncated), eof) = reader.finish(Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(bytes, b"first\n");
        assert!(!truncated);
        assert!(!eof, "the grandchild still holds the pipe");
    }

    #[test]
    fn lines_are_split_and_long_ones_dropped() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf 'a\\nbbbbbbbbbb\\ncc\\npartial'"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (sender, received) = std::sync::mpsc::channel();
        let reader = read_lines(child.stdout.take().unwrap(), 4, move |line| {
            let _ = sender.send(line.to_vec());
        })
        .unwrap();
        child.wait().unwrap();
        let ((), eof) = reader.finish(Duration::from_secs(5));
        assert!(eof);
        let lines: Vec<Vec<u8>> = received.try_iter().collect();
        assert_eq!(lines, vec![b"a".to_vec(), b"cc".to_vec()]);
    }
}
