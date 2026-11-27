//! Temporary folders and fake engines for the tests.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A folder removed with its contents on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "babeldoc-crate-test-{}-{}-{nanos}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&path).unwrap();
        // The engine reports resolved paths; keep the tests' paths comparable.
        Self(path.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes an executable shell script standing in for `babeldoc-engine`. `$OUT` is set to the
/// value of `--output`; the arguments are left in `"$@"`.
pub fn fake_engine(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("fake-engine");
    let script = format!(
        "#!/bin/sh\nprev=\nfor a; do case \"$prev\" in --output) OUT=$a;; esac; prev=$a; done\n{body}\n"
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub fn has_python() -> bool {
    std::process::Command::new("python3")
        .args(["-I", "-c", ""])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A stand-in for the engine's client: probes, sends two paragraphs the way
/// the engine does, checks the answers, writes a PDF and prints a result.
pub const PYTHON_CLIENT: &str = r#"
import http.client, json, sys, urllib.parse

endpoint, out = sys.argv[1], sys.argv[2]
url = urllib.parse.urlparse(endpoint)
base = url.path.rstrip("/")

def request(method, path, body=None):
    connection = http.client.HTTPConnection("127.0.0.1", url.port, timeout=10)
    headers = {"Host": url.netloc, "Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    connection.request(method, base + path, body=data, headers=headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return response.status, json.loads(payload)

def chat(text):
    return request("POST", "/chat/completions", {
        "model": "local",
        "messages": [
            {"role": "system", "content": "[plamo-translate]\ntarget_lang=ja\nform=polite"},
            {"role": "user", "content": text},
        ],
        "temperature": 0, "max_tokens": 256, "stop": ["<|plamo:tag|>", "<|plamo:bos|>"],
    })

status, _ = request("GET", "/models")
assert status == 200, status
status, answer = chat("hello")
assert status == 200 and answer["choices"][0]["message"]["content"] == "<hello>", answer
assert answer["choices"][0]["finish_reason"] == "stop"
status, answer = chat("keep me")
assert status == 200 and answer["choices"][0]["finish_reason"] == "length", answer
sys.stderr.write(json.dumps({"event": "progress", "stage": "Save PDF", "overall": 100}) + "\n")

import os
pdf = os.path.join(out, "in.ja.pdf")
with open(pdf, "wb") as handle:
    handle.write(b"%PDF")
keys = ("total", "translated", "failed", "unparsed_requests", "placeholder_mismatch",
        "incomplete_answers", "empty_answers", "invalid_answers", "skipped_no_letters",
        "connection_errors")
units = dict.fromkeys(keys, 0)
units.update(total=2, translated=1, failed=1, incomplete_answers=1)
print(json.dumps({"schema_version": 1, "status": "partial", "exit_code": 0, "profile": "paper",
                  "source": "en", "target": "ja", "input_pages": 1, "pages": 1,
                  "output": pdf, "dual_output": None, "seconds": 0.1, "units": units,
                  "nothing_to_translate": False}))
"#;
