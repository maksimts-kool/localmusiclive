use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

/// Seconds since the Unix epoch.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn round_to(value: f64, digits: i32) -> f64 {
    let k = 10f64.powi(digits);
    (value * k).round() / k
}

pub struct Output {
    /// -1 if the program could not start or timed out.
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs a program to completion, killing it after `timeout`.
pub async fn run(program: &str, args: &[String], timeout: Duration, capture_stderr: bool) -> Output {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr { Stdio::piped() } else { Stdio::null() })
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Output { code: -1, stdout: Vec::new(), stderr: format!("{program}: {err}").into_bytes() }
        }
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Output { code: out.status.code().unwrap_or(-1), stdout: out.stdout, stderr: out.stderr },
        Ok(Err(err)) => Output { code: -1, stdout: Vec::new(), stderr: err.to_string().into_bytes() },
        // Dropping the future kills the child (kill_on_drop).
        Err(_) => Output { code: -1, stdout: Vec::new(), stderr: format!("{program} timed out").into_bytes() },
    }
}
