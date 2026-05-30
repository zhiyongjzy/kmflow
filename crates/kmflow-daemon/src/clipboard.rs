use anyhow::Result;
use kmflow_input::Backend;
use kmflow_proto::{ClipboardContent, ClipboardPayload};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct ClipboardManager {
    backend: Backend,
}

impl ClipboardManager {
    pub fn new(backend: Backend) -> Self {
        Self { backend }
    }

    /// Start a background task that polls the clipboard and sends changes to `tx`.
    /// Returns a sender that can be used to set the local clipboard from incoming payloads.
    pub fn start_sync(
        &self,
        peer_id: String,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> (
        mpsc::Sender<ClipboardPayload>,
        mpsc::Receiver<ClipboardPayload>,
    ) {
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<ClipboardPayload>(8);
        let (incoming_tx, mut incoming_rx) = mpsc::channel::<ClipboardPayload>(8);

        let backend = self.backend;

        // Shared hash to prevent echo loop: when we write the clipboard from
        // an incoming peer payload, we update this hash so the outgoing poller
        // won't re-detect it as a "change" and send it back.
        let shared_hash = Arc::new(AtomicU64::new(0));

        // Outgoing: poll local clipboard for changes
        let out_hash = shared_hash.clone();
        let mut out_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            info!(
                "clipboard monitor started (polling every {:?})",
                POLL_INTERVAL
            );
            let mut error_logged = false;
            loop {
                tokio::select! {
                    _ = out_shutdown.changed() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }

                let content = match read_clipboard(backend) {
                    Ok(c) => {
                        error_logged = false;
                        c
                    }
                    Err(e) => {
                        if !error_logged {
                            warn!("clipboard read failed (will retry silently): {e}");
                            error_logged = true;
                        }
                        continue;
                    }
                };

                let hash = simple_hash(&content);
                let prev = out_hash.load(Ordering::Acquire);
                if hash != prev && !content.is_empty() {
                    out_hash.store(hash, Ordering::Release);
                    let payload = ClipboardPayload {
                        origin_peer_id: peer_id.clone(),
                        content: ClipboardContent::Text(content.clone()),
                    };
                    if outgoing_tx.send(payload).await.is_err() {
                        break;
                    }
                    info!(
                        len = content.len(),
                        "clipboard change detected, sending to peer"
                    );
                }
            }
        });

        // Incoming: set local clipboard from peer data
        let in_hash = shared_hash;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    Some(payload) = incoming_rx.recv() => {
                        match payload.content {
                            ClipboardContent::Text(ref text) => {
                                // Pre-update hash to suppress echo
                                in_hash.store(simple_hash(text), Ordering::Release);
                                if let Err(e) = write_clipboard(backend, text) {
                                    warn!(?e, "failed to set clipboard");
                                } else {
                                    info!(len = text.len(), "clipboard synced from peer");
                                }
                            }
                            ClipboardContent::Image { .. } => {
                                debug!("image clipboard sync not yet implemented");
                            }
                            ClipboardContent::FileUri(_) => {
                                debug!("file URI clipboard sync not yet implemented");
                            }
                        }
                    }
                }
            }
        });

        (incoming_tx, outgoing_rx)
    }
}

/// Auto-detect Wayland environment for clipboard and randr tools.
/// If WAYLAND_DISPLAY or XDG_RUNTIME_DIR are not set, scan the current
/// user's runtime dir for `wayland-*` sockets (Cosmic uses `wayland-1`,
/// GNOME uses `wayland-0`).
pub(crate) fn wayland_env() -> Vec<(String, String)> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Vec<(String, String)>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let mut envs = Vec::new();

            let uid = unsafe { libc::getuid() };
            let runtime_dir =
                std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));

            // Ensure XDG_RUNTIME_DIR is set
            if std::env::var("XDG_RUNTIME_DIR").is_err()
                && std::path::Path::new(&runtime_dir).is_dir()
            {
                envs.push(("XDG_RUNTIME_DIR".into(), runtime_dir.clone()));
            }

            // Set WAYLAND_DISPLAY if missing — scan for socket
            if std::env::var("WAYLAND_DISPLAY")
                .ok()
                .filter(|v| !v.is_empty())
                .is_none()
            {
                if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with("wayland-")
                            && !name.contains("lock")
                            && !name.contains("renderD")
                        {
                            if let Ok(ft) = entry.file_type() {
                                use std::os::unix::fs::FileTypeExt;
                                if ft.is_socket() {
                                    info!(socket = %name, "auto-detected Wayland display");
                                    envs.push(("WAYLAND_DISPLAY".into(), name.into_owned()));
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            envs
        })
        .clone()
}

fn read_clipboard(_backend: Backend) -> Result<String> {
    use std::process::Stdio;

    let wl_envs = wayland_env();

    // Try each clipboard tool in order; skip if binary missing OR if it exits non-zero
    let tools: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];

    for (cmd, args) in tools {
        let mut c = Command::new(cmd);
        c.args(*args).stderr(Stdio::null());
        // Inject Wayland env for wl-* tools
        if cmd.starts_with("wl-") {
            c.envs(wl_envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        match c.output() {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout).to_string();
                return Ok(text);
            }
            Ok(_) => continue,  // command ran but failed
            Err(_) => continue, // binary not found
        }
    }

    // All tools failed — return empty (not an error, just no clipboard access)
    Ok(String::new())
}

fn write_clipboard(_backend: Backend, text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let wl_envs = wayland_env();

    let tools: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard", "-i"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    for (cmd, args) in tools {
        let mut c = Command::new(cmd);
        c.args(*args).stdin(Stdio::piped()).stderr(Stdio::null());
        if cmd.starts_with("wl-") {
            c.envs(wl_envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        match c.spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                if let Ok(status) = child.wait() {
                    if status.success() {
                        return Ok(());
                    }
                }
            }
            Err(_) => continue,
        }
    }

    anyhow::bail!("no working clipboard tool found (install xclip, xsel, or wl-clipboard)")
}

fn simple_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_deterministic() {
        let h1 = simple_hash("hello");
        let h2 = simple_hash("hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_different_content() {
        let h1 = simple_hash("hello");
        let h2 = simple_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_empty_string() {
        // 空字符串也应该产生稳定 hash
        let h1 = simple_hash("");
        let h2 = simple_hash("");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_unicode() {
        let h1 = simple_hash("你好世界🌍");
        let h2 = simple_hash("你好世界🌍");
        assert_eq!(h1, h2);
        assert_ne!(h1, simple_hash("你好世界"));
    }
}
