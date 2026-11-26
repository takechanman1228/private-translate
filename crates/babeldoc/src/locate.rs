//! Whether a `babeldoc-engine` executable can be used: it exists, runs, has the BabelDOC version
//! it was written for, and finds BabelDOC's offline assets.
//!
//! The check runs `babeldoc-engine assets check`, which never downloads anything. The engine has
//! no version flag of its own; it checks the installed BabelDOC version before anything else and
//! answers exit code 4 with `error.code` `babeldoc_version` when it differs. Nothing is
//! installed or fetched here.

#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long the check may take (BabelDOC verifies the assets' hashes).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_STDOUT_BYTES: usize = 1 << 20;

/// The result of [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The engine runs and its assets are in place.
    Ready,
    /// No file at the path.
    NotFound,
    /// The path is not an executable file.
    NotExecutable,
    /// The engine runs with another BabelDOC version (`found`, if installed at all).
    WrongVersion { found: Option<String> },
    /// The engine runs but BabelDOC's offline assets are missing.
    AssetsMissing,
    /// The engine did not answer as expected (`reason` is a fixed code).
    Unusable { reason: &'static str },
    /// Not supported on this platform (Windows, for now).
    Unsupported,
}

/// Checks the engine at `engine` (see the module documentation).
pub fn check(engine: &Path) -> Availability {
    check_within(engine, CHECK_TIMEOUT)
}

pub fn check_within(engine: &Path, timeout: Duration) -> Availability {
    #[cfg(unix)]
    {
        unix::check(engine, timeout)
    }
    #[cfg(not(unix))]
    {
        let _ = (engine, timeout);
        Availability::Unsupported
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use crate::process::read_all;

    pub(super) fn check(engine: &Path, timeout: Duration) -> Availability {
        let metadata = match std::fs::metadata(engine) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Availability::NotFound,
            Err(_) => return Availability::NotExecutable,
        };
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Availability::NotExecutable;
        }
        let mut child = match Command::new(engine)
            .args(["assets", "check"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Availability::NotExecutable;
            }
            Err(_) => {
                return Availability::Unusable {
                    reason: "spawn_failed",
                };
            }
        };
        let pid = child.id() as libc::pid_t;
        let kill = |child: &mut std::process::Child| {
            // SAFETY: the group was created for our unreaped child.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = child.wait();
        };
        let stdout = match child.stdout.take().map(|p| read_all(p, MAX_STDOUT_BYTES)) {
            Some(Ok(reader)) => reader,
            _ => {
                // No thread to read it: stop and reap the child rather than leave it.
                kill(&mut child);
                return Availability::Unusable {
                    reason: "spawn_failed",
                };
            }
        };
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    kill(&mut child);
                    return Availability::Unusable { reason: "timeout" };
                }
            }
        };
        let Some(code) = status.code() else {
            return Availability::Unusable { reason: "crashed" };
        };
        match stdout.finish(Duration::from_secs(5)) {
            ((bytes, false), true) => classify(&bytes, code),
            ((_, true), true) => Availability::Unusable {
                reason: "malformed",
            },
            (_, false) => Availability::Unusable {
                reason: "pipes_open",
            },
        }
    }
}

fn classify(stdout: &[u8], exit: i32) -> Availability {
    let Some(line) = std::str::from_utf8(stdout)
        .ok()
        .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()))
    else {
        return Availability::Unusable {
            reason: "no_result",
        };
    };
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Availability::Unusable {
            reason: "malformed",
        };
    };
    if value.get("exit_code").and_then(Value::as_i64) != Some(i64::from(exit)) {
        return Availability::Unusable { reason: "mismatch" };
    }
    let status = value.get("status").and_then(Value::as_str);
    match (exit, status) {
        (0, Some("ok")) if value.get("ready").and_then(Value::as_bool) == Some(true) => {
            Availability::Ready
        }
        (4, Some("babeldoc_error"))
            if value.pointer("/error/code").and_then(Value::as_str) == Some("babeldoc_version") =>
        {
            let found = value
                .get("babeldoc_version")
                .and_then(Value::as_str)
                .filter(|v| v.len() <= 32 && v.bytes().all(|b| b.is_ascii_graphic()))
                .map(str::to_owned);
            Availability::WrongVersion { found }
        }
        (5, Some("assets_missing")) => Availability::AssetsMissing,
        _ => Availability::Unusable {
            reason: "unexpected",
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, fake_engine};

    fn check_script(body: &str) -> Availability {
        let dir = TempDir::new();
        check_within(&fake_engine(&dir, body), Duration::from_secs(5))
    }

    #[test]
    fn a_ready_engine_is_ready() {
        let body = r#"[ "$1 $2" = "assets check" ] || exit 9
echo '{"status": "ok", "ready": true, "cache_folder": "/x", "groups": {}, "exit_code": 0}'"#;
        assert_eq!(check_script(body), Availability::Ready);
    }

    #[test]
    fn another_babeldoc_version_is_reported() {
        let body = r#"echo '{"status": "babeldoc_error", "babeldoc_version": "0.8.0", "error": {"code": "babeldoc_version", "detail": "x"}, "exit_code": 4}'; exit 4"#;
        assert_eq!(
            check_script(body),
            Availability::WrongVersion {
                found: Some("0.8.0".into())
            }
        );
        let body = r#"echo '{"status": "babeldoc_error", "babeldoc_version": null, "error": {"code": "babeldoc_version"}, "exit_code": 4}'; exit 4"#;
        assert_eq!(
            check_script(body),
            Availability::WrongVersion { found: None }
        );
    }

    #[test]
    fn missing_assets_are_reported() {
        let body = r#"echo '{"status": "assets_missing", "ready": false, "exit_code": 5}'; exit 5"#;
        assert_eq!(check_script(body), Availability::AssetsMissing);
    }

    #[test]
    fn missing_and_unrunnable_engines_are_told_apart() {
        let dir = TempDir::new();
        assert_eq!(check(&dir.path().join("none")), Availability::NotFound);
        assert_eq!(check(dir.path()), Availability::NotExecutable);
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "#!/bin/sh\n").unwrap();
        assert_eq!(check(&plain), Availability::NotExecutable);
    }

    #[test]
    fn unexpected_answers_are_unusable() {
        let cases = [
            ("exit 0", "no_result"),
            ("echo 'nope'; exit 0", "malformed"),
            (
                r#"echo '{"status": "ok", "ready": true, "exit_code": 0}'; exit 4"#,
                "mismatch",
            ),
            (
                r#"echo '{"status": "ok", "ready": false, "exit_code": 0}'"#,
                "unexpected",
            ),
            (
                r#"echo '{"status": "babeldoc_error", "error": {"code": "internal_error"}, "exit_code": 4}'; exit 4"#,
                "unexpected",
            ),
            ("kill -9 $$", "crashed"),
            ("sleep 30", "timeout"),
        ];
        for (body, reason) in cases {
            assert_eq!(
                check_script(body),
                Availability::Unusable { reason },
                "{body}"
            );
        }
    }
}
