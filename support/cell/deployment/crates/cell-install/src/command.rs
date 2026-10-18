//! Bounded direct child execution for product-owned installation operations.

use crate::{Disposition, Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const LIMIT: u64 = 1024 * 1024;

/// Execute an explicit program with bounded output and elapsed time.
///
/// # Errors
/// Fails on spawn/I/O errors, excessive output, or timeout. A timeout is an
/// uncertain operation outcome. A nonzero child status remains in the output.
pub fn run(
    executable: &Path,
    args: &[OsString],
    environment: &BTreeMap<OsString, OsString>,
    timeout: Duration,
) -> Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = Command::new(executable)
        .args(args)
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let start = Instant::now();
    let status = loop {
        if stdout.metadata()?.len() > LIMIT || stderr.metadata()?.len() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error {
                message: "product command exceeded its output limit".to_owned(),
                disposition: Disposition::Uncertain,
            });
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error {
                message: "product command timed out; operation outcome is uncertain".to_owned(),
                disposition: Disposition::Uncertain,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.take(LIMIT + 1).read_to_end(&mut out)?;
    stderr.take(LIMIT + 1).read_to_end(&mut err)?;
    if out.len() as u64 > LIMIT || err.len() as u64 > LIMIT {
        return Err(Error::new("product command exceeded its output limit"));
    }
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// Execute an explicit program and require its successful exit.
///
/// # Errors
/// Returns execution failures and nonzero status without exposing private
/// arguments, arbitrary stderr, credentials, or product response bodies.
pub fn checked(
    executable: &Path,
    args: &[OsString],
    environment: &BTreeMap<OsString, OsString>,
    timeout: Duration,
) -> Result<Output> {
    let output = run(executable, args, environment, timeout)?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "product command failed ({})",
            output.status
        )));
    }
    Ok(output)
}

/// Require a successful object-shaped JSON product reply.
///
/// # Errors
/// Returns execution failures, invalid JSON, or an explicit failed product reply.
pub fn json(
    executable: &Path,
    args: &[OsString],
    environment: &BTreeMap<OsString, OsString>,
    timeout: Duration,
) -> Result<Value> {
    let output = checked(executable, args, environment, timeout)?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    if !value.is_object() || value.get("ok") == Some(&Value::Bool(false)) {
        return Err(Error::new("product did not report successful JSON"));
    }
    Ok(value)
}

/// Extract the existing version-one maintained product boundary.
///
/// # Errors
/// Returns an error when the reply has no supported, well-shaped maintenance data.
pub fn maintenance(value: &Value) -> Result<&Value> {
    let mut current = value;
    for _ in 0..4 {
        if current.get("holds").is_some_and(Value::is_array)
            && current.get("drained").is_some_and(Value::is_boolean)
        {
            if current.get("protocol_version") != Some(&serde_json::json!(1)) {
                return Err(Error::new("unsupported maintenance protocol"));
            }
            return Ok(current);
        }
        current = current
            .get("maintenance")
            .or_else(|| current.get("data"))
            .ok_or_else(|| Error::new("product has no supported maintenance result"))?;
    }
    Err(Error::new("product has no supported maintenance result"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Test failure expectations.
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maintenance_requires_supported_version_and_shape() {
        assert!(maintenance(&json!({"holds":[],"drained":true})).is_err());
        assert!(maintenance(&json!({"protocol_version":2,"holds":[],"drained":true})).is_err());
        assert!(
            maintenance(&json!({"data":{"protocol_version":1,"holds":[],"drained":true}})).is_ok()
        );
    }
}
