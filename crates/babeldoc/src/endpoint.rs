//! A tiny OpenAI-compatible endpoint on `127.0.0.1` that hands paragraphs to the caller's model.
//!
//! `babeldoc-engine` uses two kinds of requests, each on its own connection. It never follows redirects:
//!
//! - `GET <base>/models`: the probe before BabelDOC starts. Any `200` with a JSON object passes.
//! - `POST <base>/chat/completions` with `Content-Type: application/json`: a system message
//!   (PLaMo's instructions), one user message (the paragraph), `temperature`, `max_tokens` and
//!   `stop`. The answer must be `200` with a JSON object whose `choices[0]` has `message.content`
//!   and `finish_reason`.
//!
//! The endpoint binds `127.0.0.1` on a port the OS picks, and puts a random secret in the URL's
//! path (`http://127.0.0.1:<port>/<secret>/v1`). As soon as the request line and the headers
//! have arrived, and before any body is read, a request is refused when the secret is wrong
//! (`404`), the `Host` is not `127.0.0.1:<port>` (`421`), it carries an `Origin` header (`403`:
//! the engine never sends one, a browser always does), the path or method is unknown
//! (`404`/`405`), a `POST` is not `application/json` (`415`), or the body is too large (`413`).
//! Only the user message is used; the system message is ignored, since the caller fixed the
//! languages and the style when it built the translate function. So the endpoint does not change
//! when the model is not PLaMo.
//!
//! How a translation result is answered:
//!
//! - `Ok(text)`: `200`, `finish_reason: "stop"`.
//! - [`TranslateError::Unit`]: `200`, empty content, `finish_reason: "length"`. The engine counts
//!   the unit as `incomplete_answers`, keeps its source text and goes on.
//! - [`TranslateError::Fatal`]: `500`. The engine treats any non-`200` answer as a connection
//!   failure and stops the whole run (`translation_failed`, exit code 3), so this is only for a
//!   model that can no longer translate.
//!
//! Requests are served one at a time, on the thread that calls [`Endpoint::serve`]. The stop
//! flag is checked while waiting for a connection and at least every [`SLICE`] while reading a
//! request or writing an answer, and every request has a total deadline, so a client that
//! stalls cannot hold up a stop. The one thing this module cannot interrupt is the translate
//! function itself: it is given a [`StopSignal`] and must return soon after the signal is set
//! (see [`Endpoint::serve`]).
//!
//! Nothing here logs or prints. [`Stats`] carries counts, sizes and times only, never text. A
//! panic in the translate function is caught and answered `500`, and its payload is dropped
//! without being formatted; but the process's panic hook runs before that, and Rust's default
//! hook prints the payload on standard error. This crate does not replace the hook, which is
//! process-wide: the caller must either install its own hook that does not print payloads, or
//! catch panics inside the translate function.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The largest request body accepted (one paragraph and BabelDOC's instructions fit easily).
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// The largest request line and headers accepted.
pub const MAX_HEAD_BYTES: usize = 16 << 10;
/// How long a whole request may take to arrive, and a whole answer to be written.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// The longest a single read or write blocks before the stop flag and the deadline are checked
/// again.
pub const SLICE: Duration = Duration::from_millis(50);
/// How often [`Endpoint::serve`] checks its stop flag while no one connects.
const POLL: Duration = Duration::from_millis(20);
const SECRET_BYTES: usize = 16;

/// Why the caller's model did not give a translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslateError {
    /// This paragraph could not be translated (the output ran away, was too long, or failed
    /// validation). The engine keeps its source text and goes on with the next one.
    Unit,
    /// The model can no longer translate (it failed, or the run is being cancelled). The engine
    /// stops the run.
    Fatal,
}

/// Set when the endpoint is stopping (the run was cancelled, or the engine exited). A translate
/// function that takes long should poll it and return [`TranslateError::Fatal`] once it is set.
#[derive(Clone, Copy)]
pub struct StopSignal<'a>(&'a AtomicBool);

impl<'a> StopSignal<'a> {
    pub fn new(flag: &'a AtomicBool) -> Self {
        Self(flag)
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for StopSignal<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StopSignal")
            .field(&self.is_stopped())
            .finish()
    }
}

/// The translate function the endpoint calls for each paragraph.
pub type Translate<'f> = dyn FnMut(&str, StopSignal<'_>) -> Result<String, TranslateError> + 'f;

/// What the endpoint did. Counts, sizes and times only: never text.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// `GET …/models` answered.
    pub probes: usize,
    /// Paragraphs translated.
    pub translated: usize,
    /// Paragraphs answered as [`TranslateError::Unit`].
    pub unit_failures: usize,
    /// Requests answered `500` after [`TranslateError::Fatal`] or a panic in the translate
    /// function.
    pub fatal_failures: usize,
    /// Panics caught in the translate function (also counted in `fatal_failures`).
    pub panics: usize,
    /// Requests refused before the translate function was called (wrong secret, unknown path or
    /// method, bad headers, too large, broken JSON, no user message).
    pub rejected: usize,
    /// Connections that closed, timed out, or were cut by a stop before a whole request
    /// arrived.
    pub dropped: usize,
    /// Bytes of paragraph text passed to the translate function.
    pub source_bytes: usize,
    /// Time spent in the translate function.
    pub translate_time: Duration,
}

/// The listening socket and its secret. See the module documentation.
///
/// `Debug` shows the port only, never the secret.
pub struct Endpoint {
    listener: TcpListener,
    secret: String,
    port: u16,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Binds `127.0.0.1` on a free port with a new random secret.
    pub fn bind() -> io::Result<Self> {
        let secret = secret()?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            secret,
            port,
        })
    }

    /// The base URL to give the engine, secret included. Keep it off command lines and out of
    /// logs: `run` passes it in the engine's environment.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/v1", self.port, self.secret)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Serves requests one at a time until `stop` is set, then returns what it did.
    ///
    /// `stop` is checked while waiting for a connection and during every read and write, so a
    /// stalled client delays the return by at most about [`SLICE`]. A paragraph being translated
    /// is not interrupted: `translate` gets a [`StopSignal`] for `stop` and must return soon
    /// after it is set (with [`TranslateError::Fatal`]); `serve` returns once it has.
    pub fn serve(&self, translate: &mut Translate<'_>, stop: &AtomicBool) -> Stats {
        let mut stats = Stats::default();
        while !stop.load(Ordering::SeqCst) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if peer.ip().is_loopback() {
                        self.handle(stream, translate, stop, &mut stats);
                    } else {
                        stats.rejected += 1;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // WouldBlock: no one is connecting. Anything else (too many open files, a
                // connection reset before it was accepted): try again shortly.
                Err(_) => thread::sleep(POLL),
            }
        }
        stats
    }

    fn handle(
        &self,
        mut stream: TcpStream,
        translate: &mut Translate<'_>,
        stop: &AtomicBool,
        stats: &mut Stats,
    ) {
        // Accepted sockets inherit the listener's non-blocking mode on macOS and the BSDs.
        if stream.set_nonblocking(false).is_err()
            || stream.set_read_timeout(Some(SLICE)).is_err()
            || stream.set_write_timeout(Some(SLICE)).is_err()
        {
            stats.dropped += 1;
            return;
        }
        let mut io = Io {
            stream: &mut stream,
            deadline: Instant::now() + REQUEST_DEADLINE,
            stop,
        };
        let answer = match self.read_request(&mut io) {
            Ok((route, body)) => self.answer(route, &body, translate, stop, stats),
            Err(Refusal::Dropped) => {
                stats.dropped += 1;
                return;
            }
            Err(Refusal::Status(status)) => {
                stats.rejected += 1;
                error_answer(status)
            }
        };
        // The answer gets a deadline of its own: translating may have taken long.
        io.deadline = Instant::now() + REQUEST_DEADLINE;
        let _ = io.write_answer(&answer);
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    /// Reads the head, checks it, and only then reads the body.
    fn read_request(&self, io: &mut Io<'_>) -> Result<(Route, Vec<u8>), Refusal> {
        let (head, mut body) = read_head(io)?;
        let route = self.route(&head).map_err(Refusal::Status)?;
        let length = head.length.unwrap_or(0);
        if length > MAX_BODY_BYTES {
            return Err(Refusal::Status(413));
        }
        if body.len() > length {
            // Pipelined or trailing bytes: the engine sends one request per connection.
            return Err(Refusal::Status(400));
        }
        while body.len() < length {
            io.read_some(&mut body)?;
            if body.len() > length {
                return Err(Refusal::Status(400));
            }
        }
        Ok((route, body))
    }

    fn answer(
        &self,
        route: Route,
        body: &[u8],
        translate: &mut Translate<'_>,
        stop: &AtomicBool,
        stats: &mut Stats,
    ) -> Answer {
        match route {
            Route::Models => {
                stats.probes += 1;
                Answer::json(
                    200,
                    &json!({
                        "object": "list",
                        "data": [{"id": "local", "object": "model", "owned_by": "local"}],
                    }),
                )
            }
            Route::Chat => {
                let Some(text) = user_text(body) else {
                    stats.rejected += 1;
                    return error_answer(400);
                };
                stats.source_bytes += text.len();
                let started = Instant::now();
                let signal = StopSignal(stop);
                let result =
                    match panic::catch_unwind(AssertUnwindSafe(|| translate(&text, signal))) {
                        Ok(result) => result,
                        Err(payload) => {
                            stats.panics += 1;
                            // Dropped unread: the payload can quote the paragraph.
                            drop(payload);
                            Err(TranslateError::Fatal)
                        }
                    };
                stats.translate_time += started.elapsed();
                match result {
                    Ok(translation) => {
                        stats.translated += 1;
                        completion(&translation, "stop")
                    }
                    Err(TranslateError::Unit) => {
                        stats.unit_failures += 1;
                        completion("", "length")
                    }
                    Err(TranslateError::Fatal) => {
                        stats.fatal_failures += 1;
                        error_answer(500)
                    }
                }
            }
        }
    }

    fn route(&self, head: &Head) -> Result<Route, u16> {
        // The secret first: without it, every path is unknown.
        let path = head.target.strip_prefix('/').ok_or(404u16)?;
        let (secret, rest) = path.split_once('/').ok_or(404u16)?;
        if !same_secret(secret, &self.secret) {
            return Err(404);
        }
        // The engine sends `Host: 127.0.0.1:<port>`; anything else did not come from it (a page
        // in a browser that resolved some name to 127.0.0.1, for instance).
        if head.host.as_deref() != Some(format!("127.0.0.1:{}", self.port).as_str()) {
            return Err(421);
        }
        // Browsers send `Origin` on cross-origin requests and on every POST; the engine never
        // does.
        if head.origin {
            return Err(403);
        }
        let (route, method) = match rest {
            "v1/models" => (Route::Models, "GET"),
            "v1/chat/completions" => (Route::Chat, "POST"),
            _ => return Err(404),
        };
        if head.method != method {
            return Err(405);
        }
        if route == Route::Chat && !is_json(head.content_type.as_deref()) {
            return Err(415);
        }
        Ok(route)
    }
}

/// `application/json`, with or without parameters (`; charset=utf-8`).
fn is_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Models,
    Chat,
}

struct Head {
    method: String,
    target: String,
    host: Option<String>,
    content_type: Option<String>,
    origin: bool,
    length: Option<usize>,
}

enum Refusal {
    /// Answer with this status.
    Status(u16),
    /// The connection closed, timed out, or the endpoint is stopping: there is no one to
    /// answer.
    Dropped,
}

/// One connection's reads and writes, bounded by a deadline and cut short by `stop`.
struct Io<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
    stop: &'a AtomicBool,
}

impl Io<'_> {
    fn check(&self) -> Result<(), Refusal> {
        if self.stop.load(Ordering::SeqCst) || Instant::now() > self.deadline {
            return Err(Refusal::Dropped);
        }
        Ok(())
    }

    /// Reads what is available into `buffer`; [`Refusal::Dropped`] when the peer closed the
    /// connection, the deadline passed, or the endpoint is stopping.
    fn read_some(&mut self, buffer: &mut Vec<u8>) -> Result<(), Refusal> {
        let mut chunk = [0u8; 8192];
        loop {
            self.check()?;
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(Refusal::Dropped),
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    return Ok(());
                }
                Err(e) if is_retry(&e) => {}
                Err(_) => return Err(Refusal::Dropped),
            }
        }
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), Refusal> {
        while !bytes.is_empty() {
            self.check()?;
            match self.stream.write(bytes) {
                Ok(0) => return Err(Refusal::Dropped),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if is_retry(&e) => {}
                Err(_) => return Err(Refusal::Dropped),
            }
        }
        Ok(())
    }

    fn write_answer(&mut self, answer: &Answer) -> Result<(), Refusal> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n\r\n",
            answer.status,
            reason(answer.status),
            answer.body.len()
        );
        self.write_all(head.as_bytes())?;
        self.write_all(&answer.body)
    }
}

/// A read or write that timed out after [`SLICE`] (or was interrupted): try again.
fn is_retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// The request line and headers, and the body bytes that came with them.
fn read_head(io: &mut Io<'_>) -> Result<(Head, Vec<u8>), Refusal> {
    let mut buffer = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            break end;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(Refusal::Status(431));
        }
        io.read_some(&mut buffer)?;
    };
    if head_end > MAX_HEAD_BYTES {
        return Err(Refusal::Status(431));
    }
    let body = buffer.split_off(head_end + 4);
    let head = std::str::from_utf8(&buffer[..head_end]).map_err(|_| Refusal::Status(400))?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::Status(400));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Refusal::Status(505));
    }
    let mut parsed = Head {
        method: method.to_owned(),
        target: target.to_owned(),
        host: None,
        content_type: None,
        origin: false,
        length: None,
    };
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(Refusal::Status(400))?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "host" if parsed.host.is_none() => parsed.host = Some(value.to_owned()),
            "host" => return Err(Refusal::Status(400)),
            "content-type" if parsed.content_type.is_none() => {
                parsed.content_type = Some(value.to_owned())
            }
            "content-type" => return Err(Refusal::Status(400)),
            "origin" => parsed.origin = true,
            "content-length" if parsed.length.is_none() => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Refusal::Status(400));
                }
                parsed.length = Some(value.parse::<usize>().map_err(|_| Refusal::Status(413))?);
            }
            "content-length" => return Err(Refusal::Status(400)),
            // The engine never sends a chunked body.
            "transfer-encoding" => return Err(Refusal::Status(501)),
            _ => {}
        }
    }
    Ok((parsed, body))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The text of the only user message, if the body is a chat request with exactly one.
fn user_text(body: &[u8]) -> Option<String> {
    let request: Value = serde_json::from_slice(body).ok()?;
    let messages = request.as_object()?.get("messages")?.as_array()?;
    let mut users = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"));
    let content = users.next()?.get("content")?.as_str()?;
    if users.next().is_some() || content.trim().is_empty() {
        return None;
    }
    Some(content.to_owned())
}

struct Answer {
    status: u16,
    body: Vec<u8>,
}

impl Answer {
    fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            body: serde_json::to_vec(value).expect("a JSON value serializes"),
        }
    }
}

fn completion(content: &str, finish_reason: &str) -> Answer {
    Answer::json(
        200,
        &json!({
            "id": "chatcmpl-local",
            "object": "chat.completion",
            "created": 0,
            "model": "local",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": finish_reason,
            }],
        }),
    )
}

fn error_answer(status: u16) -> Answer {
    Answer::json(
        status,
        &json!({"error": {"message": reason(status), "type": "local_endpoint"}}),
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        421 => "Misdirected Request",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

/// Compares in time that does not depend on where the strings differ.
fn same_secret(given: &str, secret: &str) -> bool {
    given.len() == secret.len()
        && given
            .bytes()
            .zip(secret.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// `bytes` random bytes from the OS, as lowercase hex. Also used for the names of
/// private folders (`run`).
#[cfg(unix)]
pub(crate) fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn secret() -> io::Result<String> {
    random_hex(SECRET_BYTES)
}

#[cfg(not(unix))]
fn secret() -> io::Result<String> {
    // Windows is not supported in this stage (see `run`).
    let _ = SECRET_BYTES;
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// A running endpoint whose translate function upper-cases the text, fails for "unit" and
    /// "fatal", and panics for "panic". Counts the calls.
    struct Running {
        url: String,
        port: u16,
        secret: String,
        calls: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<Stats>>,
    }

    impl Running {
        fn start() -> Self {
            let endpoint = Endpoint::bind().unwrap();
            let url = endpoint.url();
            let port = endpoint.port();
            let secret = endpoint.secret.clone();
            let calls = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (thread_calls, thread_stop) = (calls.clone(), stop.clone());
            let thread = thread::spawn(move || {
                let mut translate = |text: &str, signal: StopSignal<'_>| {
                    thread_calls.fetch_add(1, Ordering::SeqCst);
                    match text {
                        "unit" => Err(TranslateError::Unit),
                        "fatal" => Err(TranslateError::Fatal),
                        "panic" => panic!("the model crashed"),
                        "block" => {
                            // A slow model that polls the signal, as `serve` asks.
                            while !signal.is_stopped() {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(TranslateError::Fatal)
                        }
                        _ => Ok(text.to_uppercase()),
                    }
                };
                endpoint.serve(&mut translate, &thread_stop)
            });
            Self {
                url,
                port,
                secret,
                calls,
                stop,
                thread: Some(thread),
            }
        }

        fn finish(mut self) -> Stats {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn connect(&self) -> TcpStream {
            let stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            stream
        }

        /// Sends raw bytes and returns the status and the JSON body (or `None` when the
        /// endpoint closed the connection without answering).
        fn raw(&self, bytes: &[u8]) -> Option<(u16, Value)> {
            let mut stream = self.connect();
            stream.write_all(bytes).unwrap();
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            parse(&answer)
        }

        fn send(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/json\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                self.port,
                body.len()
            );
            self.raw(request.as_bytes()).expect("an answer")
        }

        fn base(&self) -> String {
            format!("/{}/v1", self.secret)
        }

        fn chat(&self, body: &str) -> (u16, Value) {
            self.send("POST", &format!("{}/chat/completions", self.base()), body)
        }
    }

    fn parse(answer: &[u8]) -> Option<(u16, Value)> {
        let end = find(answer, b"\r\n\r\n")?;
        let head = std::str::from_utf8(&answer[..end]).unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let body = &answer[end + 4..];
        assert_eq!(body.len(), length);
        Some((status, serde_json::from_slice(body).unwrap()))
    }

    /// The body `babeldoc-engine`'s client sends (`PlamoClient.translate`).
    fn engine_body(text: &str) -> String {
        json!({
            "model": "plamo-3-translate-2b",
            "messages": [
                {"role": "system", "content": "[plamo-translate]\ntarget_lang=ja\nform=polite"},
                {"role": "user", "content": text},
            ],
            "temperature": 0,
            "max_tokens": 256,
            "stop": ["<|plamo:tag|>", "<|plamo:bos|>"],
        })
        .to_string()
    }

    #[test]
    fn url_is_loopback_with_a_random_secret_in_the_path() {
        let a = Endpoint::bind().unwrap();
        let b = Endpoint::bind().unwrap();
        assert!(
            a.url()
                .starts_with(&format!("http://127.0.0.1:{}/", a.port()))
        );
        assert!(a.url().ends_with("/v1"));
        assert_eq!(a.secret.len(), SECRET_BYTES * 2);
        assert!(a.secret.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.secret, b.secret);
        assert_eq!(
            a.listener.local_addr().unwrap().ip(),
            std::net::IpAddr::from(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn probe_and_translation_answer_like_an_openai_server() {
        let server = Running::start();
        let (status, models) = server.send("GET", &format!("{}/models", server.base()), "");
        assert_eq!(status, 200);
        assert!(models.is_object());

        let (status, answer) = server.chat(&engine_body("bring the paper lanterns"));
        assert_eq!(status, 200);
        let choice = &answer["choices"][0];
        assert_eq!(choice["message"]["content"], "BRING THE PAPER LANTERNS");
        assert_eq!(choice["finish_reason"], "stop");
        assert!(server.url.contains(&server.secret));

        let stats = server.finish();
        assert_eq!(stats.probes, 1);
        assert_eq!(stats.translated, 1);
        assert_eq!(stats.source_bytes, "bring the paper lanterns".len());
        assert_eq!(stats.rejected, 0);
    }

    #[test]
    fn only_the_user_message_reaches_the_model() {
        let server = Running::start();
        let (status, answer) = server.chat(&engine_body("ünïcode {v1} text"));
        assert_eq!(status, 200);
        assert_eq!(
            answer["choices"][0]["message"]["content"],
            "ÜNÏCODE {V1} TEXT"
        );
        assert_eq!(server.calls(), 1);
        server.finish();
    }

    #[test]
    fn a_unit_failure_is_an_incomplete_answer_and_a_fatal_one_is_a_server_error() {
        let server = Running::start();
        let (status, answer) = server.chat(&engine_body("unit"));
        assert_eq!(status, 200);
        assert_eq!(answer["choices"][0]["finish_reason"], "length");
        assert_eq!(answer["choices"][0]["message"]["content"], "");

        let (status, _) = server.chat(&engine_body("fatal"));
        assert_eq!(status, 500);
        let (status, _) = server.chat(&engine_body("panic"));
        assert_eq!(status, 500);

        // Still serving after a panic.
        let (status, _) = server.chat(&engine_body("again"));
        assert_eq!(status, 200);
        let stats = server.finish();
        assert_eq!(stats.unit_failures, 1);
        assert_eq!(stats.fatal_failures, 2);
        assert_eq!(stats.translated, 1);
    }

    #[test]
    fn a_wrong_or_missing_secret_is_not_found_and_never_translated() {
        let server = Running::start();
        let wrong = "0".repeat(SECRET_BYTES * 2);
        for path in [
            format!("/{wrong}/v1/chat/completions"),
            "/v1/chat/completions".to_owned(),
            format!("/{}x/v1/chat/completions", server.secret),
            format!("/{}/v1/chat/completions", &server.secret[1..]),
            "chat/completions".to_owned(),
        ] {
            let (status, _) = server.send("POST", &path, &engine_body("hello"));
            assert_eq!(status, 404, "{path}");
        }
        let (status, _) = server.send("GET", &format!("/{wrong}/v1/models"), "");
        assert_eq!(status, 404);
        assert_eq!(server.calls(), 0);
        let stats = server.finish();
        assert_eq!(stats.rejected, 6);
        assert_eq!(stats.probes, 0);
    }

    #[test]
    fn unknown_paths_and_methods_are_refused() {
        let server = Running::start();
        let base = server.base();
        let cases = [
            ("GET", format!("{base}/chat/completions"), 405),
            ("POST", format!("{base}/models"), 405),
            ("PUT", format!("{base}/chat/completions"), 405),
            ("POST", format!("{base}/completions"), 404),
            ("POST", format!("{base}/chat/completions/"), 404),
            ("POST", format!("{base}/chat/completions?x=1"), 404),
            ("GET", format!("{base}/"), 404),
        ];
        for (method, path, expected) in &cases {
            let (status, _) = server.send(method, path, &engine_body("hello"));
            assert_eq!(status, *expected, "{method} {path}");
        }
        assert_eq!(server.calls(), 0);
        server.finish();
    }

    #[test]
    fn a_request_for_another_host_is_refused() {
        let server = Running::start();
        let body = engine_body("hello");
        let request = format!(
            "POST {}/chat/completions HTTP/1.1\r\nHost: example.com\r\nContent-Length: {}\r\n\r\n{body}",
            server.base(),
            body.len()
        );
        assert_eq!(server.raw(request.as_bytes()).unwrap().0, 421);
        let request = format!(
            "POST {}/chat/completions HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            server.base(),
            body.len()
        );
        assert_eq!(server.raw(request.as_bytes()).unwrap().0, 421);
        assert_eq!(server.calls(), 0);
        server.finish();
    }

    #[test]
    fn a_body_too_large_is_refused_without_reading_it() {
        let server = Running::start();
        let request = format!(
            "POST {}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            server.base(),
            server.port,
            MAX_BODY_BYTES + 1
        );
        assert_eq!(server.raw(request.as_bytes()).unwrap().0, 413);
        let request = format!(
            "POST {}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Content-Type: application/json\r\nContent-Length: 99999999999999999999999\r\n\r\n",
            server.base(),
            server.port,
        );
        assert_eq!(server.raw(request.as_bytes()).unwrap().0, 413);
        let huge_head = format!(
            "GET / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        assert_eq!(server.raw(huge_head.as_bytes()).unwrap().0, 431);
        assert_eq!(server.calls(), 0);
        server.finish();
    }

    #[test]
    fn broken_json_and_requests_without_one_user_message_are_bad_requests() {
        let server = Running::start();
        let bodies = [
            "{not json".to_owned(),
            "[]".to_owned(),
            "\"text\"".to_owned(),
            json!({"messages": []}).to_string(),
            json!({"messages": "hello"}).to_string(),
            json!({"messages": [{"role": "system", "content": "hello"}]}).to_string(),
            json!({"messages": [{"role": "user", "content": 3}]}).to_string(),
            json!({"messages": [{"role": "user", "content": [{"type": "text"}]}]}).to_string(),
            json!({"messages": [{"role": "user", "content": "  "}]}).to_string(),
            json!({"messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"},
            ]})
            .to_string(),
            String::new(),
        ];
        for body in &bodies {
            let (status, _) = server.chat(body);
            assert_eq!(status, 400, "{body}");
        }
        // Invalid UTF-8 in the body.
        let mut request = format!(
            "POST {}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Content-Type: application/json\r\nContent-Length: 4\r\n\r\n",
            server.base(),
            server.port
        )
        .into_bytes();
        request.extend_from_slice(&[0xff, 0xfe, b'{', b'}']);
        assert_eq!(server.raw(&request).unwrap().0, 400);
        assert_eq!(server.calls(), 0);
        let stats = server.finish();
        assert_eq!(stats.rejected, bodies.len() + 1);
    }

    #[test]
    fn malformed_heads_are_refused() {
        let server = Running::start();
        let base = server.base();
        let port = server.port;
        let cases = [
            ("GARBAGE\r\n\r\n".to_owned(), 400),
            (
                format!("GET {base}/models HTTP/2\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
                505,
            ),
            (
                format!("GET {base}/models HTTP/1.1\r\nno colon\r\n\r\n"),
                400,
            ),
            (
                format!(
                    "POST {base}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                ),
                501,
            ),
            (
                format!(
                    "POST {base}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Content-Length: 2\r\nContent-Length: 2\r\n\r\n{{}}"
                ),
                400,
            ),
            (
                format!(
                    "POST {base}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Content-Length: -1\r\n\r\n"
                ),
                400,
            ),
            (
                format!(
                    "POST {base}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}GET / HTTP/1.1\r\n\r\n"
                ),
                400,
            ),
        ];
        for (request, expected) in &cases {
            assert_eq!(
                server.raw(request.as_bytes()).unwrap().0,
                *expected,
                "{request}"
            );
        }
        assert_eq!(server.calls(), 0);
        server.finish();
    }

    #[test]
    fn a_connection_cut_short_gets_no_answer_and_the_endpoint_keeps_serving() {
        let server = Running::start();
        // Headers promise more body than is sent, then the client closes its side.
        let mut stream = server.connect();
        let head = format!(
            "POST {}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"mess",
            server.base(),
            server.port
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        assert!(answer.is_empty());
        // A connection that closes before sending anything.
        drop(server.connect());

        let (status, _) = server.chat(&engine_body("still here"));
        assert_eq!(status, 200);
        assert_eq!(server.calls(), 1);
        let stats = server.finish();
        assert_eq!(stats.dropped, 2);
        assert_eq!(stats.translated, 1);
    }

    #[test]
    fn serve_returns_once_stopped() {
        let endpoint = Endpoint::bind().unwrap();
        let stop = AtomicBool::new(true);
        let stats = endpoint.serve(&mut |_, _| Ok(String::new()), &stop);
        assert_eq!(stats, Stats::default());
    }

    /// A request is judged on its head, without waiting for a body that never
    /// comes. Before, the endpoint read the whole body first and dropped the connection only
    /// after the read timeout, answering nothing.
    #[test]
    fn the_head_is_checked_before_the_body_is_read() {
        let server = Running::start();
        let base = server.base();
        let port = server.port;
        let wrong = "0".repeat(SECRET_BYTES * 2);
        let cases = [
            (
                format!("POST /{wrong}/v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"),
                404,
            ),
            (
                format!("POST {base}/chat/completions HTTP/1.1\r\nHost: localhost:{port}\r\n"),
                421,
            ),
            (
                format!(
                    "POST {base}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Origin: http://example.com\r\nContent-Type: application/json\r\n"
                ),
                403,
            ),
            (
                format!(
                    "POST {base}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Content-Type: text/plain\r\n"
                ),
                415,
            ),
        ];
        for (head, expected) in &cases {
            let started = Instant::now();
            let mut stream = server.connect();
            // The body is promised and never sent; the connection stays open.
            stream
                .write_all(format!("{head}Content-Length: 1000\r\n\r\n").as_bytes())
                .unwrap();
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            assert_eq!(parse(&answer).map(|a| a.0), Some(*expected), "{head}");
            assert!(started.elapsed() < Duration::from_secs(2), "{head}");
        }
        assert_eq!(server.calls(), 0);
        server.finish();
    }

    /// Only JSON requests without a browser origin can reach translation.
    #[test]
    fn a_post_needs_json_and_no_origin() {
        let server = Running::start();
        let body = engine_body("hello");
        let request = |extra: &str| {
            format!(
                "POST {}/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}\
                 Content-Length: {}\r\n\r\n{body}",
                server.base(),
                server.port,
                body.len()
            )
        };
        for (extra, expected) in [
            ("", 415),
            ("Content-Type: text/plain\r\n", 415),
            ("Content-Type: application/x-www-form-urlencoded\r\n", 415),
            (
                "Content-Type: application/json\r\nContent-Type: application/json\r\n",
                400,
            ),
            ("Content-Type: application/json\r\nOrigin: null\r\n", 403),
            (
                "Origin: http://127.0.0.1\r\nContent-Type: application/json\r\n",
                403,
            ),
        ] {
            assert_eq!(
                server.raw(request(extra).as_bytes()).unwrap().0,
                expected,
                "{extra}"
            );
        }
        assert_eq!(server.calls(), 0);
        let accepted = request("Content-Type: Application/JSON; charset=utf-8\r\n");
        assert_eq!(server.raw(accepted.as_bytes()).unwrap().0, 200);
        // The probe has no body and needs no content type, but an Origin still refuses it.
        let probe = format!(
            "GET {}/models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://example.com\r\n\r\n",
            server.base(),
            server.port
        );
        assert_eq!(server.raw(probe.as_bytes()).unwrap().0, 403);
        assert_eq!(server.calls(), 1);
        server.finish();
    }

    /// A client that stalls in the middle of its head does not hold up a stop.
    #[test]
    fn a_stop_during_a_slow_read_returns_promptly() {
        let server = Running::start();
        let mut stream = server.connect();
        stream
            .write_all(format!("POST {}/chat/completions HTTP/1.1\r\n", server.base()).as_bytes())
            .unwrap();
        // Let the endpoint accept the connection and start reading it.
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let stats = server.finish();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(stats.dropped, 1);
        drop(stream);
    }

    /// The translate function sees the stop and `serve` returns once it does.
    #[test]
    fn a_stop_during_a_translation_reaches_the_function() {
        let server = Running::start();
        let (port, path) = (server.port, format!("{}/chat/completions", server.base()));
        let body = engine_body("block");
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).unwrap();
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
        });
        let waiting = Instant::now();
        while server.calls() == 0 {
            assert!(waiting.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        let stats = server.finish();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(stats.fatal_failures, 1);
        client.join().unwrap();
    }

    #[test]
    fn debug_output_does_not_show_the_secret() {
        let endpoint = Endpoint::bind().unwrap();
        let shown = format!("{endpoint:?}");
        assert!(!shown.contains(&endpoint.secret), "{shown}");
        assert!(shown.contains(&endpoint.port.to_string()));
    }

    #[test]
    fn a_panic_is_counted_and_its_payload_is_not_kept() {
        let server = Running::start();
        let (status, answer) = server.chat(&engine_body("panic"));
        assert_eq!(status, 500);
        assert!(!answer.to_string().contains("crashed"));
        let stats = server.finish();
        assert_eq!(stats.panics, 1);
        assert!(!format!("{stats:?}").contains("crashed"));
    }
}
