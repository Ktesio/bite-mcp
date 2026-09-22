//! Long-lived bridge client: spawns the Swift helper, performs the handshake,
//! correlates requests/responses, restarts on crash.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Callback invoked (reader thread) for every helper notification.
pub type NotificationHandler = Arc<dyn Fn(&str, &Value) + Send + Sync>;

use serde_json::Value;

use crate::protocol::{
    decode_line, encode_request, BridgeError, FromHelper, Hello, PROTOCOL_VERSION,
};

/// Default per-request timeout. Apple Event calls against slow apps can take a
/// while; doctor probes use a shorter one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

struct Inner {
    stdin: Mutex<Option<BufWriter<ChildStdin>>>,
    pending: Mutex<HashMap<i64, SyncSender<Result<Value, BridgeError>>>>,
    next_id: std::sync::atomic::AtomicI64,
    hello: Mutex<Option<Hello>>,
    on_notification: Mutex<Option<NotificationHandler>>,
}

pub struct Bridge {
    inner: Arc<Inner>,
    child: Arc<Mutex<Option<Child>>>,
    pub helper_path: String,
    /// Called (from the reader thread) for every helper notification.
    pub on_notification: Option<NotificationHandler>,
}

impl Bridge {
    /// Spawn the helper binary and wait for its `hello` handshake.
    pub fn spawn(helper_path: impl Into<String>) -> Result<Self, BridgeError> {
        Self::spawn_args(helper_path, &[])
    }

    /// Spawn with extra argv (used by the scripted fake helper in tests).
    pub fn spawn_args(
        helper_path: impl Into<String>,
        args: &[String],
    ) -> Result<Self, BridgeError> {
        let path = helper_path.into();
        let mut child = Command::new(&path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BridgeError::spawn(format!("cannot spawn {}: {e}", path)))?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take().expect("piped stdin");

        // helper stderr is diagnostics only; mirror to our stderr
        std::thread::Builder::new()
            .name("bite-helper-stderr".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("[bite-helper] {line}");
                }
            })
            .map_err(|e| BridgeError::spawn(e.to_string()))?;

        let bridge = Bridge {
            on_notification: None,
            inner: Arc::new(Inner {
                stdin: Mutex::new(Some(BufWriter::new(stdin))),
                pending: Mutex::new(HashMap::new()),
                next_id: std::sync::atomic::AtomicI64::new(1),
                hello: Mutex::new(None),
                on_notification: Mutex::new(None),
            }),
            child: Arc::new(Mutex::new(Some(child))),
            helper_path: path,
        };

        // reader thread: dispatch hello/responses/notifications
        let inner = Arc::clone(&bridge.inner);
        let child_slot = Arc::clone(&bridge.child);
        std::thread::Builder::new()
            .name("bite-helper-reader".to_owned())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    match decode_line(&line) {
                        Ok(Some(FromHelper::Hello(hello))) => {
                            *inner.hello.lock().unwrap() = Some(hello);
                        }
                        Ok(Some(FromHelper::Response { id, result, error })) => {
                            if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(match (result, error) {
                                    (Some(r), _) => Ok(r),
                                    (None, Some(e)) => Err(e),
                                    (None, None) => Err(BridgeError::protocol("empty response")),
                                });
                            }
                        }
                        Ok(Some(FromHelper::Notification { method, params })) => {
                            if let Some(cb) = inner.on_notification.lock().unwrap().as_ref() {
                                cb(&method, &params);
                            }
                            let msg = params.get("message").and_then(|m| m.as_str()).unwrap_or("");
                            eprintln!("[bite-helper:{method}] {msg}");
                        }
                        Ok(None) | Err(_) => {
                            eprintln!("[bite-helper] unparseable line: {line}");
                        }
                    }
                }
                // stdout closed: helper is gone; fail every pending request
                if let Some(mut child) = child_slot.lock().unwrap().take() {
                    let _ = child.wait();
                }
                let pending: HashMap<i64, SyncSender<Result<Value, BridgeError>>> =
                    std::mem::take(&mut *inner.pending.lock().unwrap());
                for (_, tx) in pending {
                    let _ = tx.send(Err(BridgeError::helper_exited()));
                }
                *inner.stdin.lock().unwrap() = None;
            })
            .map_err(|e| BridgeError::spawn(e.to_string()))?;

        // wait for hello
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while bridge.inner.hello.lock().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                return Err(BridgeError::timeout("helper did not send hello within 15s"));
            }
            if !bridge.is_alive() {
                return Err(BridgeError::helper_exited());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let hello = bridge.inner.hello.lock().unwrap().clone().expect("hello");
        if hello.protocol != PROTOCOL_VERSION {
            return Err(BridgeError::protocol(format!(
                "helper protocol {} != expected {PROTOCOL_VERSION}; run `bite install-helper --force`",
                hello.protocol
            )));
        }
        // hello raced with an exit: fail now rather than on the first call
        if !bridge.is_alive() {
            return Err(BridgeError::helper_exited());
        }
        Ok(bridge)
    }

    pub fn hello(&self) -> Option<Hello> {
        self.inner.hello.lock().unwrap().clone()
    }

    pub fn capabilities(&self) -> Vec<String> {
        self.hello().map(|h| h.capabilities).unwrap_or_default()
    }

    /// Register a notification handler (job progress, batch-ready, …).
    pub fn set_notification_handler(&self, cb: NotificationHandler) {
        *self.inner.on_notification.lock().unwrap() = Some(cb);
    }

    pub fn is_alive(&self) -> bool {
        match &mut *self.child.lock().unwrap() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Send one request and block for the correlated response.
    pub fn call(&self, method: &str, params: &Value) -> Result<Value, BridgeError> {
        self.call_timeout(method, params, DEFAULT_TIMEOUT)
    }

    pub fn call_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, BridgeError> {
        let id = self
            .inner
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = sync_channel(1);
        self.inner.pending.lock().unwrap().insert(id, tx);

        {
            let mut guard = self.inner.stdin.lock().unwrap();
            match guard.as_mut() {
                Some(writer) => {
                    let line = encode_request(id, method, params);
                    let write_ok = writer
                        .write_all(line.as_bytes())
                        .and_then(|_| writer.flush())
                        .is_ok();
                    if !write_ok {
                        drop(guard);
                        self.inner.pending.lock().unwrap().remove(&id);
                        return Err(BridgeError::helper_exited());
                    }
                }
                None => {
                    drop(guard);
                    self.inner.pending.lock().unwrap().remove(&id);
                    return Err(BridgeError::helper_exited());
                }
            }
        }

        match rx.recv_timeout(timeout) {
            Ok(res) => res,
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                if !self.is_alive() {
                    Err(BridgeError::helper_exited())
                } else {
                    Err(BridgeError::timeout(format!(
                        "'{method}' timed out after {}s",
                        timeout.as_secs()
                    )))
                }
            }
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
