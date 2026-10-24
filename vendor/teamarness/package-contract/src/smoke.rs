//! A Backend smoke launch: spawn the declared executable and speak the Package Process Protocol
//! far enough to prove it starts, answers, activates and stops.
//!
//! This is deliberately synchronous — `std::process` plus one reader thread — so the author
//! validator needs no async runtime.
//!
//! # Rule names
//!
//! | Rule | What it holds |
//! |---|---|
//! | `smoke.spawn_failed` | the declared Backend could not be spawned |
//! | `smoke.handshake_timeout` | no `initialized` frame within 5 s |
//! | `smoke.handshake_mismatch` | `initialized` did not echo the nonce, generation, identity or UI contract digest |
//! | `smoke.pong_timeout` | no `pong` within 5 s |
//! | `smoke.activate_failed` | `lifecycle.activate` failed or timed out; the Package code is the pointer |
//! | `smoke.dispose_failed` | `lifecycle.dispose` failed or timed out |
//! | `smoke.shutdown_timeout` | no `shutdown_ack` within 3 s, or the process did not exit within 3 s |
//! | `smoke.protocol_violation` | an undecodable frame, or a frame the sequence does not allow |

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use teamarness_package_protocol::{Frame, FrameDecoder, FrameKind, encode_frame};

use crate::error::{ContractError, ContractErrorCode};
use crate::manifest::LoadedManifest;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PONG_TIMEOUT: Duration = Duration::from_secs(5);
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(30);
const DISPOSE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const EXIT_TIMEOUT: Duration = Duration::from_secs(3);

const NONCE: &str = "c21va2U";
const PING_NONCE: &str = "cGluZw";
const CONTINUITY_ID: &str = "dmFsaWRhdG9y";

fn failure(rule: &'static str) -> ContractError {
    ContractError::new(ContractErrorCode::SmokeFailed, rule)
}

/// The note a Package that declares Semantic Capabilities gets instead of an activation.
pub const CAPABILITIES_SKIPPED: &str = "smoke.capabilities_skipped: the Package declares Semantic Capabilities, which the validator does not grant, so activation was not attempted";

/// Spawns the declared Backend and drives it through activation and shutdown.
///
/// The Backend runs with a fresh temporary directory as both its working directory and its data
/// directory, so the smoke never touches the author's files.
///
/// # Errors
///
/// Returns the first smoke rule the Backend breaks.
pub fn smoke(directory: &Path, manifest: &LoadedManifest) -> Result<Vec<String>, ContractError> {
    let temporary = tempfile::tempdir().map_err(|_| failure("smoke.spawn_failed"))?;
    // The Backend runs with a different working directory, so its path must be absolute, and
    // real: a Node Backend compares `process.argv[1]` with its own module URL, which is resolved
    // through symbolic links such as macOS `/var` to `/private/var`.
    let backend = directory
        .join(&manifest.backend.relative_path)
        .canonicalize()
        .map_err(|_| failure("smoke.spawn_failed").at(manifest.backend.relative_path.clone()))?;
    let mut child = Command::new(&backend)
        .current_dir(temporary.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failure("smoke.spawn_failed").at(manifest.backend.relative_path.clone()))?;
    let result = converse(&mut child, temporary.path(), manifest);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn converse(
    child: &mut Child,
    data_directory: &Path,
    manifest: &LoadedManifest,
) -> Result<Vec<String>, ContractError> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| failure("smoke.spawn_failed"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("smoke.spawn_failed"))?;
    let frames = read_frames(stdout);
    let mut notes = Vec::new();

    send(&mut stdin, initialize_frame(data_directory, manifest))?;
    let initialized = await_frame(
        &mut stdin,
        &frames,
        HANDSHAKE_TIMEOUT,
        "smoke.handshake_timeout",
        |frame| frame.kind() == FrameKind::Initialized,
    )?;
    check_handshake(&initialized, manifest)?;

    send(&mut stdin, json!({"kind": "ping", "nonce": PING_NONCE}))?;
    await_frame(
        &mut stdin,
        &frames,
        PONG_TIMEOUT,
        "smoke.pong_timeout",
        |frame| {
            frame.kind() == FrameKind::Pong
                && frame.field("nonce").and_then(Value::as_str) == Some(PING_NONCE)
        },
    )?;

    if manifest.capabilities.is_empty() {
        lifecycle(
            &mut stdin,
            &frames,
            "lifecycle.activate",
            ACTIVATE_TIMEOUT,
            "smoke.activate_failed",
        )?;
        lifecycle(
            &mut stdin,
            &frames,
            "lifecycle.dispose",
            DISPOSE_TIMEOUT,
            "smoke.dispose_failed",
        )?;
        notes.push("the Backend initialized, answered a ping, activated and disposed".to_owned());
    } else {
        notes.push(CAPABILITIES_SKIPPED.to_owned());
    }

    send(&mut stdin, json!({"kind": "shutdown", "reason": "smoke"}))?;
    await_frame(
        &mut stdin,
        &frames,
        SHUTDOWN_TIMEOUT,
        "smoke.shutdown_timeout",
        |frame| frame.kind() == FrameKind::ShutdownAck,
    )?;
    drop(stdin);
    await_exit(child)?;
    Ok(notes)
}

fn lifecycle(
    stdin: &mut ChildStdin,
    frames: &Receiver<Option<Frame>>,
    operation: &str,
    timeout: Duration,
    rule: &'static str,
) -> Result<(), ContractError> {
    let id = format!("h:{operation}-1");
    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(timeout)
        .as_millis();
    send(
        stdin,
        json!({
            "kind": "request",
            "id": id,
            "operation": operation,
            "deadlineUnixMs": deadline.to_string(),
            "payload": {},
        }),
    )?;
    let reply = await_frame(stdin, frames, timeout, rule, |frame| {
        matches!(frame.kind(), FrameKind::Response | FrameKind::Error)
            && frame.field("id").and_then(Value::as_str) == Some(id.as_str())
    })?;
    if reply.kind() == FrameKind::Error {
        let code = reply
            .field("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(failure(rule).at(format!("#/code/{code}")));
    }
    Ok(())
}

fn initialize_frame(data_directory: &Path, manifest: &LoadedManifest) -> Value {
    json!({
        "kind": "initialize",
        "nonce": NONCE,
        "package": {
            "identity": manifest.identity,
            "version": manifest.version.to_string(),
            "manifestDigest": manifest.manifest_digest,
            "backendDigest": manifest.backend.digest,
        },
        "uiContractDigest": manifest.ui.contract.digest,
        "processGeneration": "1",
        "workspace": {"id": "validator", "name": "Validator"},
        "installation": {"identity": "validator", "displayName": "Validator"},
        "dataDirectory": {
            "path": data_directory.to_string_lossy(),
            "continuityId": CONTINUITY_ID,
        },
        "protocol": {"major": 1, "minor": 0},
        "capabilities": [],
        "limits": {
            "maxFrameBytes": 1_048_576,
            "maxInFlightEachDirection": 32,
            "maxWriterFrames": 64,
            "maxWriterBytes": 8_388_608,
            "initialStreamCredit": 32,
        },
    })
}

fn check_handshake(frame: &Frame, manifest: &LoadedManifest) -> Result<(), ContractError> {
    let matches = frame.field("nonce").and_then(Value::as_str) == Some(NONCE)
        && frame.field("processGeneration").and_then(Value::as_str) == Some("1")
        && frame.field("uiContractDigest").and_then(Value::as_str)
            == Some(manifest.ui.contract.digest.as_str())
        && frame
            .field("package")
            .and_then(Value::as_object)
            .and_then(|package| package.get("identity"))
            .and_then(Value::as_str)
            == Some(manifest.identity.as_str());
    if matches {
        Ok(())
    } else {
        Err(failure("smoke.handshake_mismatch"))
    }
}

/// Waits for the first frame matching `wanted`, answering everything the Package sends meanwhile.
///
/// A Package `request` (a Semantic Capability call) is answered with `capability_unavailable`, a
/// `stream_open` is closed with the same code, and a `ping` is answered with `pong`, so a Backend
/// that speaks while it activates is never left waiting.
fn await_frame(
    stdin: &mut ChildStdin,
    frames: &Receiver<Option<Frame>>,
    timeout: Duration,
    rule: &'static str,
    wanted: impl Fn(&Frame) -> bool,
) -> Result<Frame, ContractError> {
    let start = Instant::now();
    loop {
        let remaining = timeout
            .checked_sub(start.elapsed())
            .ok_or_else(|| failure(rule))?;
        match frames.recv_timeout(remaining) {
            Err(RecvTimeoutError::Timeout) => return Err(failure(rule)),
            Err(RecvTimeoutError::Disconnected) | Ok(None) => {
                return Err(failure("smoke.protocol_violation"));
            }
            Ok(Some(frame)) => {
                if wanted(&frame) {
                    return Ok(frame);
                }
                answer(stdin, &frame)?;
            }
        }
    }
}

fn answer(stdin: &mut ChildStdin, frame: &Frame) -> Result<(), ContractError> {
    match answer_frame(frame)? {
        Some(value) => send(stdin, value),
        None => Ok(()),
    }
}

/// The frame the validator owes one the Package sent, if it owes one at all.
///
/// Separated from the write so the answers can be read back in a test: the
/// `stream_open` arm answered nothing for a year because it looked for a field
/// the schema forbids on that frame.
fn answer_frame(frame: &Frame) -> Result<Option<Value>, ContractError> {
    let field = |name: &str| frame.field(name).and_then(Value::as_str);
    match frame.kind() {
        FrameKind::Request => {
            let Some(id) = field("id") else {
                return Err(failure("smoke.protocol_violation"));
            };
            Ok(Some(json!({
                "kind": "error",
                "id": id,
                "code": "capability_unavailable",
                "retryable": false,
            })))
        }
        FrameKind::StreamOpen => {
            // `stream_open` names the new stream in its own `id`. The schema
            // requires `["kind","id","topic","initialCredit","payload"]` with
            // `additionalProperties: false`, so it has no `streamId` and
            // reading one took this arm's error branch on every well-formed
            // frame.
            let Some(stream) = field("id") else {
                return Err(failure("smoke.protocol_violation"));
            };
            Ok(Some(json!({
                "kind": "stream_close",
                "streamId": stream,
                "code": "capability_unavailable",
            })))
        }
        FrameKind::Ping => {
            let Some(nonce) = field("nonce") else {
                return Err(failure("smoke.protocol_violation"));
            };
            Ok(Some(json!({"kind": "pong", "nonce": nonce})))
        }
        FrameKind::StreamClose | FrameKind::Credit | FrameKind::Event => Ok(None),
        _ => Err(failure("smoke.protocol_violation")),
    }
}

fn send(stdin: &mut ChildStdin, value: Value) -> Result<(), ContractError> {
    let frame = Frame::try_from(value).map_err(|_| failure("smoke.protocol_violation"))?;
    let bytes = encode_frame(&frame).map_err(|_| failure("smoke.protocol_violation"))?;
    stdin
        .write_all(&bytes)
        .and_then(|()| stdin.flush())
        .map_err(|_| failure("smoke.protocol_violation"))
}

/// Decodes frames on a thread; `None` marks a decode failure or end of stream.
fn read_frames(mut stdout: impl Read + Send + 'static) -> Receiver<Option<Frame>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut decoder = FrameDecoder::default();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if decoder.push(&buffer[..read]).is_err() {
                let _ = sender.send(None);
                return;
            }
            loop {
                match decoder.next_frame() {
                    Ok(Some(frame)) => {
                        if sender.send(Some(frame)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let _ = sender.send(None);
                        return;
                    }
                }
            }
        }
        let _ = sender.send(None);
    });
    receiver
}

fn await_exit(child: &mut Child) -> Result<(), ContractError> {
    let start = Instant::now();
    while start.elapsed() < EXIT_TIMEOUT {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return Err(failure("smoke.shutdown_timeout")),
        }
    }
    Err(failure("smoke.shutdown_timeout"))
}

#[cfg(test)]
mod tests {
    use super::{answer_frame, failure};
    use serde_json::{Value, json};
    use teamarness_package_protocol::Frame;

    fn frame(value: Value) -> Frame {
        Frame::try_from(value).expect("valid frame")
    }

    /// A Package that opens a stream while it activates is answered, not left
    /// waiting on a validator that read the wrong field.
    #[test]
    fn a_stream_open_is_closed_by_the_id_it_names() {
        let answer = answer_frame(&frame(json!({
            "kind":"stream_open", "id":"p:smoke-1", "topic":"capability.stream",
            "initialCredit":32, "payload":{}
        })))
        .expect("answered");
        assert_eq!(
            answer,
            Some(json!({
                "kind":"stream_close", "streamId":"p:smoke-1",
                "code":"capability_unavailable"
            }))
        );
    }

    #[test]
    fn a_request_is_answered_and_a_ping_is_ponged() {
        assert_eq!(
            answer_frame(&frame(json!({
                "kind":"request", "id":"p:smoke-call", "operation":"capability.call",
                "deadlineUnixMs":"9007199254740998", "payload":{}
            })))
            .expect("answered"),
            Some(json!({
                "kind":"error", "id":"p:smoke-call",
                "code":"capability_unavailable", "retryable":false
            }))
        );
        assert_eq!(
            answer_frame(&frame(json!({"kind":"ping","nonce":"cGluZw"}))).expect("answered"),
            Some(json!({"kind":"pong","nonce":"cGluZw"}))
        );
    }

    #[test]
    fn a_frame_the_sequence_does_not_allow_is_a_protocol_violation() {
        let error = answer_frame(&frame(json!({"kind":"shutdown_ack"}))).unwrap_err();
        assert_eq!(error.rule, failure("smoke.protocol_violation").rule);
    }
}
