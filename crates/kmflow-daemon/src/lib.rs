pub mod clipboard;
pub mod edge;
pub mod ipc;
pub mod state;

use anyhow::Result;
use clipboard::ClipboardManager;
use edge::{EdgeDetector, EdgeHit};
use kmflow_input::{Backend, detect_backend};
use kmflow_net::tls::{TlsIdentity, TofuVerifier};
use kmflow_net::{Discovery, QuicTransport};
use kmflow_proto::{
    ClipboardPayload, ControlCommand, EventFrame, InputEvent, KeyState, KmflowConfig,
    PROTOCOL_VERSION, ScreenInfo,
};
use state::ConnectionState;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, error, info, warn};

const EMERGENCY_HOTKEY_CTRL: u32 = 29; // Left Ctrl scancode (USB HID)
const EMERGENCY_HOTKEY_ALT: u32 = 56; // Left Alt scancode
const EMERGENCY_HOTKEY_ESC: u32 = 1; // Escape scancode

const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const RECONNECT_MAX_ATTEMPTS: u32 = 10;
const EVENT_BATCH_LIMIT: usize = 64;

pub struct Daemon {
    config: KmflowConfig,
    transport: QuicTransport,
    discovery: Discovery,
    state: Arc<Mutex<DaemonState>>,
    peers: Arc<Mutex<HashMap<String, ipc::PeerStatus>>>,
    tofu: Arc<TofuVerifier>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    backend: Backend,
    connect_tx: mpsc::Sender<SocketAddr>,
    connect_rx: Mutex<mpsc::Receiver<SocketAddr>>,
}

struct DaemonState {
    connection_state: ConnectionState,
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<kmflow_proto::MouseButton>,
    local_screen: ScreenInfo,
    remote_screen: Option<ScreenInfo>,
    seq_counter: u32,
    focus_is_remote: bool,
    cursor_x: i32,
    cursor_y: i32,
}

impl Daemon {
    pub async fn new(config: KmflowConfig) -> Result<Self> {
        let config_dir = TlsIdentity::config_dir();
        let identity = TlsIdentity::load_or_generate(&config_dir).await?;
        let tofu = Arc::new(TofuVerifier::new(&config_dir));

        let transport = QuicTransport::bind(config.port, &identity).await?;

        let hostname = config
            .hostname
            .clone()
            .unwrap_or_else(|| gethostname().unwrap_or_else(|| "kmflow-node".to_string()));

        let discovery = Discovery::new(&hostname, config.port, &identity.fingerprint)?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (connect_tx, connect_rx) = mpsc::channel(8);

        let backend = detect_backend();
        info!(?backend, "detected display backend");

        let mut screen = query_screen_info(backend);
        info!(
            width = screen.width,
            height = screen.height,
            scale = screen.scale_factor,
            "local screen info"
        );
        if let Some(s) = config.scale_override {
            info!(scale = s, "using scale override from CLI");
            // Recalculate logical resolution if we got physical from DRM
            if s > 1.0 && screen.scale_factor == 1.0 {
                screen.width = (screen.width as f64 / s) as u32;
                screen.height = (screen.height as f64 / s) as u32;
            }
            screen.scale_factor = s;
        }

        let state = Arc::new(Mutex::new(DaemonState {
            connection_state: ConnectionState::Disconnected,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            local_screen: screen.clone(),
            remote_screen: None,
            seq_counter: 0,
            focus_is_remote: false,
            cursor_x: screen.width as i32 / 2,
            cursor_y: screen.height as i32 / 2,
        }));

        Ok(Self {
            config,
            transport,
            discovery,
            state,
            peers: Arc::new(Mutex::new(HashMap::new())),
            tofu,
            shutdown_tx,
            shutdown_rx,
            backend,
            connect_tx,
            connect_rx: Mutex::new(connect_rx),
        })
    }

    pub async fn run(&self) -> Result<()> {
        self.discovery
            .browse_with_notify(Some(self.connect_tx.clone()))?;

        let ipc_server = ipc::IpcServer::bind().await?;
        let state_for_ipc = self.state.clone();
        let peers_for_ipc = self.peers.clone();
        let shutdown_tx_for_ipc = self.shutdown_tx.clone();
        let connect_tx_for_ipc = self.connect_tx.clone();
        tokio::spawn(async move {
            loop {
                match ipc_server.accept().await {
                    Ok(mut conn) => {
                        let state = state_for_ipc.clone();
                        let peers = peers_for_ipc.clone();
                        let shutdown_tx = shutdown_tx_for_ipc.clone();
                        let connect_tx = connect_tx_for_ipc.clone();
                        tokio::spawn(async move {
                            if let Ok(req) = conn.read_request().await {
                                let resp = handle_ipc_request(
                                    req,
                                    &state,
                                    &peers,
                                    &shutdown_tx,
                                    &connect_tx,
                                )
                                .await;
                                let _ = conn.write_response(&resp).await;
                            }
                        });
                    }
                    Err(e) => {
                        error!(?e, "IPC accept error");
                        break;
                    }
                }
            }
        });

        info!("daemon running, waiting for connections...");

        let mut shutdown_rx = self.shutdown_rx.clone();
        let mut connect_rx = self.connect_rx.lock().await;

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    info!("shutdown signal received");
                    break;
                }
                result = self.transport.accept() => {
                    match result {
                        Ok(peer) => {
                            info!(addr = %peer.remote_addr, "peer connected");
                            self.handle_accepted_peer(peer).await?;
                        }
                        Err(e) => {
                            error!(?e, "accept error");
                        }
                    }
                }
                Some(addr) = connect_rx.recv() => {
                    info!(%addr, "connecting to peer via IPC request");
                    if let Err(e) = self.connect_to(addr).await {
                        error!(?e, "failed to connect to peer");
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_accepted_peer(&self, peer: kmflow_net::quic::PeerConnection) -> Result<()> {
        // TOFU: check if peer is known, auto-trust if not
        if !self.tofu.is_known(&peer.fingerprint).await.unwrap_or(false) {
            info!(fingerprint = %peer.fingerprint, addr = %peer.remote_addr, "new peer, auto-trusting (TOFU)");
            let identity = kmflow_proto::PeerIdentity {
                fingerprint: peer.fingerprint.clone(),
                hostname: String::new(), // filled after handshake
                last_seen: now_us(),
            };
            let _ = self.tofu.trust_peer(&identity).await;
        }

        let state = self.state.clone();
        let peers = self.peers.clone();
        let shutdown_rx = self.shutdown_rx.clone();
        let backend = self.backend;
        let configured_edge = self.config.layout.first().map(|l| l.edge);

        tokio::spawn(async move {
            if let Err(e) = Self::peer_session(
                peer,
                state,
                peers,
                shutdown_rx,
                backend,
                configured_edge,
                false,
            )
            .await
            {
                error!(?e, "peer session ended");
            }
        });

        Ok(())
    }

    async fn handle_initiated_peer(&self, peer: kmflow_net::quic::PeerConnection) -> Result<()> {
        let state = self.state.clone();
        let peers = self.peers.clone();
        let shutdown_rx = self.shutdown_rx.clone();
        let backend = self.backend;
        let configured_edge = self.config.layout.first().map(|l| l.edge);

        tokio::spawn(async move {
            if let Err(e) = Self::peer_session(
                peer,
                state,
                peers,
                shutdown_rx,
                backend,
                configured_edge,
                true,
            )
            .await
            {
                error!(?e, "peer session ended");
            }
        });

        Ok(())
    }

    async fn peer_session(
        peer: kmflow_net::quic::PeerConnection,
        state: Arc<Mutex<DaemonState>>,
        peers: Arc<Mutex<HashMap<String, ipc::PeerStatus>>>,
        mut shutdown_rx: watch::Receiver<bool>,
        backend: Backend,
        configured_edge: Option<kmflow_proto::Edge>,
        is_initiator: bool,
    ) -> Result<()> {
        let mut target_edge = configured_edge.unwrap_or(kmflow_proto::Edge::Right);

        let (mut send, mut recv) = if is_initiator {
            // We initiated: open stream, send Handshake with our edge (if configured), expect HandshakeAck
            let (mut send, mut recv) = peer.open_control_stream().await?;
            {
                let s = state.lock().await;
                let cmd = ControlCommand::Handshake {
                    protocol_version: PROTOCOL_VERSION,
                    hostname: gethostname().unwrap_or_default(),
                    screen_info: s.local_screen.clone(),
                    edge: configured_edge,
                };
                peer.send_control(&mut send, &cmd).await?;
            }
            let reply = peer.recv_control(&mut recv).await?;
            match reply {
                ControlCommand::HandshakeAck {
                    protocol_version,
                    hostname: peer_hostname,
                    screen_info,
                    edge: peer_edge,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        anyhow::bail!(
                            "protocol version mismatch: local={PROTOCOL_VERSION}, remote={protocol_version}"
                        );
                    }
                    // If we have no configured edge but peer told us theirs, infer ours
                    if configured_edge.is_none() {
                        if let Some(pe) = peer_edge {
                            target_edge = opposite_edge(pe);
                            info!(
                                ?pe,
                                ?target_edge,
                                "initiator: inferred edge from peer's ack"
                            );
                        }
                    }
                    let mut s = state.lock().await;
                    s.remote_screen = Some(screen_info.clone());
                    let _ = s.connection_state.transition_to(ConnectionState::Active);
                    info!(
                        ?target_edge,
                        "handshake complete (initiator), connection active"
                    );

                    // Register peer info for IPC status
                    peers.lock().await.insert(
                        peer_hostname.clone(),
                        ipc::PeerStatus {
                            hostname: peer_hostname,
                            addr: peer.remote_addr.to_string(),
                            state: "active".to_string(),
                            rtt_ms: peer.rtt().as_secs_f64() * 1000.0,
                            screen: format!(
                                "{}x{} @{}x",
                                screen_info.width, screen_info.height, screen_info.scale_factor
                            ),
                        },
                    );
                }
                _ => anyhow::bail!("unexpected handshake reply"),
            }
            (send, recv)
        } else {
            // We accepted: accept stream, receive Handshake, infer our edge, send HandshakeAck
            let (mut send, mut recv) = peer.accept_control_stream().await?;
            let msg = peer.recv_control(&mut recv).await?;
            match msg {
                ControlCommand::Handshake {
                    protocol_version,
                    screen_info,
                    hostname,
                    edge: peer_edge,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        anyhow::bail!(
                            "protocol version mismatch: local={PROTOCOL_VERSION}, remote={protocol_version}"
                        );
                    }
                    // Infer our edge: prefer our config, else derive from peer's
                    if configured_edge.is_none() {
                        if let Some(pe) = peer_edge {
                            target_edge = opposite_edge(pe);
                            info!(%hostname, ?pe, ?target_edge, "acceptor: inferred edge from peer");
                        }
                    }
                    let s = state.lock().await;
                    let ack = ControlCommand::HandshakeAck {
                        protocol_version: PROTOCOL_VERSION,
                        hostname: gethostname().unwrap_or_default(),
                        screen_info: s.local_screen.clone(),
                        edge: Some(target_edge),
                    };
                    drop(s);
                    peer.send_control(&mut send, &ack).await?;

                    let mut s = state.lock().await;
                    s.remote_screen = Some(screen_info.clone());
                    let _ = s.connection_state.transition_to(ConnectionState::Active);
                    info!(
                        ?target_edge,
                        "handshake complete (acceptor), connection active"
                    );

                    // Register peer info for IPC status
                    peers.lock().await.insert(
                        hostname.clone(),
                        ipc::PeerStatus {
                            hostname: hostname.clone(),
                            addr: peer.remote_addr.to_string(),
                            state: "active".to_string(),
                            rtt_ms: peer.rtt().as_secs_f64() * 1000.0,
                            screen: format!(
                                "{}x{} @{}x",
                                screen_info.width, screen_info.height, screen_info.scale_factor
                            ),
                        },
                    );
                }
                _ => anyhow::bail!("unexpected first control message"),
            }
            (send, recv)
        };

        // Set up edge detector
        let local_screen = {
            let s = state.lock().await;
            s.local_screen.clone()
        };
        let edge_detector = EdgeDetector::new(local_screen.clone(), target_edge);

        // Channel for captured input events from the blocking capture thread
        let (capture_tx, mut capture_rx) = mpsc::channel::<InputEvent>(256);
        // Channel to notify capture thread of grab/ungrab
        let (grab_tx, grab_rx) = watch::channel(false);

        // Spawn blocking capture thread (non-fatal if it fails)
        let capture_backend = backend;
        let capture_shutdown = shutdown_rx.clone();
        let capture_handle = tokio::task::spawn_blocking(move || {
            run_capture_loop(capture_backend, capture_tx, capture_shutdown, grab_rx)
        });

        // Create emulator for replaying remote events locally (non-fatal)
        let mut emulator: Option<Box<dyn kmflow_input::InputEmulator>> =
            match kmflow_input::create_emulator(backend) {
                Ok(e) => {
                    info!("input emulator created");
                    Some(e)
                }
                Err(e) => {
                    warn!(
                        ?e,
                        "failed to create emulator, incoming events won't be replayed"
                    );
                    None
                }
            };

        // Create clipboard manager for sync
        let clipboard = ClipboardManager::new(backend);
        let local_hostname = gethostname().unwrap_or_else(|| "unknown".to_string());
        let (clipboard_incoming_tx, mut clipboard_outgoing_rx) =
            clipboard.start_sync(local_hostname, shutdown_rx.clone());

        // Spawn task to accept incoming QUIC bi-streams (used for clipboard)
        let (clip_recv_tx, mut clip_recv_rx) = mpsc::channel::<ClipboardPayload>(8);
        {
            let peer_clone = peer.clone();
            let mut clip_shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = clip_shutdown.changed() => break,
                        stream = peer_clone.accept_control_stream() => {
                            match stream {
                                Ok((_send, mut recv)) => {
                                    // Read tag byte
                                    let mut tag = [0u8; 1];
                                    if recv.read_exact(&mut tag).await.is_err() { continue; }
                                    if tag[0] != 0xCB { continue; } // not clipboard

                                    let mut len_buf = [0u8; 4];
                                    if recv.read_exact(&mut len_buf).await.is_err() { continue; }
                                    let len = u32::from_le_bytes(len_buf) as usize;
                                    if len > 10 * 1024 * 1024 { continue; } // 10MB safety limit

                                    let mut data = vec![0u8; len];
                                    if recv.read_exact(&mut data).await.is_err() { continue; }

                                    match kmflow_proto::decode_clipboard(&data) {
                                        Ok(payload) => {
                                            let _ = clip_recv_tx.send(payload).await;
                                        }
                                        Err(e) => {
                                            warn!(?e, "failed to decode clipboard payload");
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
            });
        }

        // Main event loop
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    info!("peer session: shutdown signal");
                    break;
                }
                // Send outgoing clipboard data to peer via QUIC stream
                Some(payload) = clipboard_outgoing_rx.recv() => {
                    match kmflow_proto::encode_clipboard(&payload) {
                        Ok(bytes) => {
                            if let Ok((mut clip_send, _)) = peer.open_control_stream().await {
                                let len = (bytes.len() as u32).to_le_bytes();
                                let tag = [0xCBu8]; // clipboard tag to distinguish from control
                                let _ = clip_send.write_all(&tag).await;
                                let _ = clip_send.write_all(&len).await;
                                let _ = clip_send.write_all(&bytes).await;
                                let _ = clip_send.finish();
                                info!("clipboard sent to peer ({} bytes)", bytes.len());
                            }
                        }
                        Err(e) => warn!(?e, "failed to encode clipboard"),
                    }
                }
                // Receive clipboard data from peer
                Some(payload) = clip_recv_rx.recv() => {
                    let _ = clipboard_incoming_tx.send(payload).await;
                }
                // Receive captured local input events
                captured = capture_rx.recv() => {
                    let Some(event) = captured else {
                        info!("capture channel closed, running as receive-only");
                        // Keep event loop alive with edge detection on received events
                        loop {
                            tokio::select! {
                                _ = shutdown_rx.changed() => break,
                                // Clipboard receive in receive-only mode
                                Some(payload) = clip_recv_rx.recv() => {
                                    let _ = clipboard_incoming_tx.send(payload).await;
                                }
                                // Clipboard send in receive-only mode
                                Some(payload) = clipboard_outgoing_rx.recv() => {
                                    match kmflow_proto::encode_clipboard(&payload) {
                                        Ok(bytes) => {
                                            if let Ok((mut clip_send, _)) = peer.open_control_stream().await {
                                                let len = (bytes.len() as u32).to_le_bytes();
                                                let tag = [0xCBu8];
                                                let _ = clip_send.write_all(&tag).await;
                                                let _ = clip_send.write_all(&len).await;
                                                let _ = clip_send.write_all(&bytes).await;
                                                let _ = clip_send.finish();
                                                info!("clipboard sent to peer ({} bytes)", bytes.len());
                                            }
                                        }
                                        Err(e) => warn!(?e, "failed to encode clipboard"),
                                    }
                                }
                                result = peer.recv_datagram() => {
                                    match result {
                                        Ok(frame) => {
                                            let mut s = state.lock().await;
                                            if !s.focus_is_remote {
                                                // Track cursor for edge detection
                                                let mut hit_edge = false;
                                                for event in &frame.events {
                                                    if let InputEvent::MouseMove { dx, dy } = event {
                                                        s.cursor_x = (s.cursor_x + *dx as i32)
                                                            .clamp(0, local_screen.width as i32 - 1);
                                                        s.cursor_y = (s.cursor_y + *dy as i32)
                                                            .clamp(0, local_screen.height as i32 - 1);
                                                        if edge_detector.check(s.cursor_x, s.cursor_y) != EdgeHit::None {
                                                            hit_edge = true;
                                                            break;
                                                        }
                                                    }
                                                }
                                                if hit_edge {
                                                    info!(x = s.cursor_x, y = s.cursor_y, "edge hit (receive-only), returning focus to peer");
                                                    s.focus_is_remote = true;
                                                    s.cursor_x = local_screen.width as i32 / 2;
                                                    s.cursor_y = local_screen.height as i32 / 2;
                                                    drop(s);
                                                    let _ = peer.send_control(
                                                        &mut send,
                                                        &ControlCommand::SwitchFocus { to_peer: "remote".to_string() },
                                                    ).await;
                                                } else {
                                                    drop(s);
                                                    if let Some(ref mut emu) = emulator {
                                                        for ev in &frame.events {
                                                            let _ = emu.emit(ev);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                                ctrl = peer.recv_control(&mut recv) => {
                                    match ctrl {
                                        Ok(ControlCommand::SwitchFocus { .. }) => {
                                            let mut s = state.lock().await;
                                            s.focus_is_remote = false;
                                            info!("peer switched focus to us (receive-only)");
                                        }
                                        Ok(ControlCommand::ReleaseFocus) => {
                                            let mut s = state.lock().await;
                                            s.focus_is_remote = false;
                                            info!("peer released focus (receive-only)");
                                        }
                                        Ok(ControlCommand::PeerDisconnecting) => break,
                                        Ok(ControlCommand::Ping { timestamp_us }) => {
                                            let _ = peer.send_control(&mut send, &ControlCommand::Pong { echo_timestamp_us: timestamp_us }).await;
                                        }
                                        Err(_) => break,
                                        _ => {}
                                    }
                                }
                            }
                        }
                        break;
                    };
                    let mut s = state.lock().await;

                    // Track key state for emergency hotkey detection
                    track_key_state(&mut s, &event);

                    // Check emergency hotkey: Ctrl+Alt+Esc
                    if check_emergency_hotkey(&s) && s.focus_is_remote {
                        info!("emergency hotkey triggered, releasing focus");
                        s.focus_is_remote = false;
                        let _ = grab_tx.send(false);
                        let releases = flush_pressed_keys(&s);
                        let seq = next_seq(&mut s);
                        drop(s);
                        // Send release events to peer
                        if !releases.is_empty() {
                            let frame = EventFrame {
                                seq,
                                timestamp_us: now_us(),
                                events: releases,
                            };
                            let _ = peer.send_datagram(&frame);
                        }
                        // Send ReleaseFocus control message
                        let _ = peer.send_control(&mut send, &ControlCommand::ReleaseFocus).await;
                        continue;
                    }

                    if s.focus_is_remote {
                        // Batch: drain additional pending events from the channel
                        let mut batch = vec![event];
                        while batch.len() < EVENT_BATCH_LIMIT {
                            match capture_rx.try_recv() {
                                Ok(ev) => {
                                    track_key_state(&mut s, &ev);
                                    batch.push(ev);
                                }
                                Err(_) => break,
                            }
                        }

                        let seq = next_seq(&mut s);
                        drop(s);
                        let frame = EventFrame {
                            seq,
                            timestamp_us: now_us(),
                            events: batch,
                        };
                        if let Err(e) = peer.send_datagram(&frame) {
                            warn!(?e, "failed to send datagram");
                            break;
                        }
                    } else {
                        // Focus is local — track cursor for edge detection
                        if let InputEvent::MouseMove { dx, dy } = &event {
                            s.cursor_x = (s.cursor_x + *dx as i32)
                                .clamp(0, local_screen.width as i32 - 1);
                            s.cursor_y = (s.cursor_y + *dy as i32)
                                .clamp(0, local_screen.height as i32 - 1);

                            if edge_detector.check(s.cursor_x, s.cursor_y) != EdgeHit::None {
                                info!(x = s.cursor_x, y = s.cursor_y, "edge hit, switching focus to remote");
                                s.focus_is_remote = true;
                                let _ = grab_tx.send(true);
                                // Reset cursor to center so next switch requires reaching edge again
                                s.cursor_x = local_screen.width as i32 / 2;
                                s.cursor_y = local_screen.height as i32 / 2;
                                drop(s);
                                // Notify peer that we're sending focus
                                let _ = peer.send_control(
                                    &mut send,
                                    &ControlCommand::SwitchFocus { to_peer: "remote".to_string() },
                                ).await;
                            }
                        }
                    }
                }
                // Receive datagrams from remote peer (they send events to us)
                result = peer.recv_datagram() => {
                    match result {
                        Ok(frame) => {
                            let mut s = state.lock().await;
                            if !s.focus_is_remote {
                                // Track cursor and check edge hit for focus return
                                let mut hit_edge = false;
                                for event in &frame.events {
                                    if let InputEvent::MouseMove { dx, dy } = event {
                                        s.cursor_x = (s.cursor_x + *dx as i32)
                                            .clamp(0, local_screen.width as i32 - 1);
                                        s.cursor_y = (s.cursor_y + *dy as i32)
                                            .clamp(0, local_screen.height as i32 - 1);

                                        if edge_detector.check(s.cursor_x, s.cursor_y) != EdgeHit::None {
                                            hit_edge = true;
                                            break;
                                        }
                                    }
                                }

                                if hit_edge {
                                    info!(x = s.cursor_x, y = s.cursor_y, "edge hit on received events, returning focus to peer");
                                    s.focus_is_remote = true;
                                    let _ = grab_tx.send(true);
                                    s.cursor_x = local_screen.width as i32 / 2;
                                    s.cursor_y = local_screen.height as i32 / 2;
                                    drop(s);
                                    let _ = peer.send_control(
                                        &mut send,
                                        &ControlCommand::SwitchFocus { to_peer: "remote".to_string() },
                                    ).await;
                                } else {
                                    drop(s);
                                    if let Some(ref mut emu) = emulator {
                                        for event in &frame.events {
                                            if let Err(e) = emu.emit(event) {
                                                warn!(?e, "emulator error");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(?e, "datagram recv error");
                            break;
                        }
                    }
                }
                // Receive control messages from peer
                ctrl = peer.recv_control(&mut recv) => {
                    match ctrl {
                        Ok(ControlCommand::SwitchFocus { .. }) => {
                            // Peer is sending focus to us — we become the receiver
                            let mut s = state.lock().await;
                            s.focus_is_remote = false;
                            let _ = grab_tx.send(false);
                            info!("peer switched focus to us");
                        }
                        Ok(ControlCommand::ReleaseFocus) => {
                            // Peer released focus back
                            let mut s = state.lock().await;
                            s.focus_is_remote = false;
                            let _ = grab_tx.send(false);
                            info!("peer released focus");
                        }
                        Ok(ControlCommand::PeerDisconnecting) => {
                            info!("peer disconnecting");
                            break;
                        }
                        Ok(ControlCommand::Ping { timestamp_us }) => {
                            let _ = peer.send_control(
                                &mut send,
                                &ControlCommand::Pong { echo_timestamp_us: timestamp_us },
                            ).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(?e, "control recv error");
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup: release all pressed keys locally, notify peer
        let mut s = state.lock().await;
        if s.focus_is_remote {
            let releases = flush_pressed_keys(&s);
            if !releases.is_empty() {
                let seq = next_seq(&mut s);
                let frame = EventFrame {
                    seq,
                    timestamp_us: now_us(),
                    events: releases,
                };
                let _ = peer.send_datagram(&frame);
            }
        }
        let _ = s
            .connection_state
            .transition_to(ConnectionState::Disconnected);
        s.remote_screen = None;
        s.focus_is_remote = false;
        s.pressed_keys.clear();
        s.pressed_buttons.clear();
        drop(s);

        // Unregister peer from shared map
        peers.lock().await.clear();

        // Wait for capture thread to finish (it should exit when channel drops)
        drop(capture_rx);
        let _ = capture_handle.await;

        Ok(())
    }

    pub async fn connect_to(&self, addr: SocketAddr) -> Result<()> {
        // Skip if already connected
        let s = self.state.lock().await;
        if s.connection_state == ConnectionState::Active {
            info!(%addr, "already connected, skipping");
            return Ok(());
        }
        drop(s);

        let mut delay = RECONNECT_BASE_DELAY;
        for attempt in 1..=RECONNECT_MAX_ATTEMPTS {
            match self.transport.connect(addr).await {
                Ok(peer) => {
                    info!(addr = %peer.remote_addr, fingerprint = %peer.fingerprint, "connected to peer");

                    // TOFU: check and trust
                    if !self.tofu.is_known(&peer.fingerprint).await.unwrap_or(false) {
                        info!(fingerprint = %peer.fingerprint, "new peer, auto-trusting (TOFU)");
                        let identity = kmflow_proto::PeerIdentity {
                            fingerprint: peer.fingerprint.clone(),
                            hostname: String::new(),
                            last_seen: now_us(),
                        };
                        let _ = self.tofu.trust_peer(&identity).await;
                    }

                    return self.handle_initiated_peer(peer).await;
                }
                Err(e) => {
                    if attempt == RECONNECT_MAX_ATTEMPTS {
                        return Err(e);
                    }
                    warn!(
                        attempt,
                        max = RECONNECT_MAX_ATTEMPTS,
                        ?delay,
                        ?e,
                        "connection failed, retrying"
                    );
                    {
                        let mut s = self.state.lock().await;
                        let _ = s
                            .connection_state
                            .transition_to(ConnectionState::Reconnecting);
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RECONNECT_MAX_DELAY);
                }
            }
        }
        unreachable!()
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn status(&self) -> ConnectionState {
        self.state.lock().await.connection_state
    }
}

fn run_capture_loop(
    backend: Backend,
    tx: mpsc::Sender<InputEvent>,
    shutdown_rx: watch::Receiver<bool>,
    mut grab_rx: watch::Receiver<bool>,
) {
    let mut capture = match kmflow_input::create_capture(backend) {
        Ok(c) => c,
        Err(e) => {
            info!(?e, "no input capture available, will run as receive-only");
            return;
        }
    };

    // Set up shutdown watcher: when shutdown signal arrives, write to the
    // capture's eventfd to interrupt the blocking epoll_wait in next_event().
    if let Some(fd) = capture.shutdown_fd() {
        let srx = shutdown_rx.clone();
        std::thread::spawn(move || {
            loop {
                if *srx.borrow() {
                    let val: u64 = 1;
                    unsafe {
                        libc::write(fd, &val as *const u64 as *const libc::c_void, 8);
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
    }

    let mut grabbed = false;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        // Check if grab state changed
        if grab_rx.has_changed().unwrap_or(false) {
            let should_grab = *grab_rx.borrow_and_update();
            if should_grab && !grabbed {
                if let Err(e) = capture.grab_pointer() {
                    warn!(?e, "grab pointer failed");
                } else {
                    grabbed = true;
                    info!("pointer grabbed — forwarding to remote");
                }
            } else if !should_grab && grabbed {
                if let Err(e) = capture.ungrab_pointer() {
                    warn!(?e, "ungrab pointer failed");
                } else {
                    grabbed = false;
                    info!("pointer ungrabbed — focus returned to local");
                }
            }
        }
        match capture.next_event() {
            Ok(event) => {
                if tx.blocking_send(event).is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!(?e, "capture error");
                break;
            }
        }
    }
}

fn check_emergency_hotkey(state: &DaemonState) -> bool {
    state.pressed_keys.contains(&EMERGENCY_HOTKEY_CTRL)
        && state.pressed_keys.contains(&EMERGENCY_HOTKEY_ALT)
        && state.pressed_keys.contains(&EMERGENCY_HOTKEY_ESC)
}

fn next_seq(state: &mut DaemonState) -> u32 {
    state.seq_counter = state.seq_counter.wrapping_add(1);
    state.seq_counter
}

async fn handle_ipc_request(
    req: ipc::IpcRequest,
    state: &Mutex<DaemonState>,
    peers: &Mutex<HashMap<String, ipc::PeerStatus>>,
    shutdown_tx: &watch::Sender<bool>,
    connect_tx: &mpsc::Sender<SocketAddr>,
) -> ipc::IpcResponse {
    match req {
        ipc::IpcRequest::Status => {
            let state_str = state.lock().await.connection_state.to_string();
            let peer_list: Vec<ipc::PeerStatus> = peers.lock().await.values().cloned().collect();
            ipc::IpcResponse::Status {
                state: state_str,
                peers: peer_list,
            }
        }
        ipc::IpcRequest::Stop => {
            let _ = shutdown_tx.send(true);
            ipc::IpcResponse::Ok {
                message: "daemon stopping".to_string(),
            }
        }
        ipc::IpcRequest::Pair { addr } => {
            let socket_addr = if addr.contains(':') {
                addr.parse::<SocketAddr>()
            } else {
                format!("{addr}:{}", kmflow_proto::DEFAULT_PORT).parse::<SocketAddr>()
            };
            match socket_addr {
                Ok(sa) => {
                    if connect_tx.send(sa).await.is_ok() {
                        ipc::IpcResponse::Ok {
                            message: format!("connecting to {sa}..."),
                        }
                    } else {
                        ipc::IpcResponse::Error {
                            message: "daemon channel closed".to_string(),
                        }
                    }
                }
                Err(e) => ipc::IpcResponse::Error {
                    message: format!("invalid address '{addr}': {e}"),
                },
            }
        }
        ipc::IpcRequest::Layout { peer, edge } => ipc::IpcResponse::Ok {
            message: format!("layout set: {peer} on {edge}"),
        },
        ipc::IpcRequest::SetupFirewall => ipc::IpcResponse::Ok {
            message: "use `kmflow setup-firewall` directly".to_string(),
        },
    }
}

fn track_key_state(state: &mut DaemonState, event: &InputEvent) {
    match event {
        InputEvent::Key {
            scancode,
            state: KeyState::Pressed,
        } => {
            state.pressed_keys.insert(*scancode);
        }
        InputEvent::Key {
            scancode,
            state: KeyState::Released,
        } => {
            state.pressed_keys.remove(scancode);
        }
        InputEvent::MouseButton {
            button,
            state: kmflow_proto::ButtonState::Pressed,
        } => {
            state.pressed_buttons.insert(*button);
        }
        InputEvent::MouseButton {
            button,
            state: kmflow_proto::ButtonState::Released,
        } => {
            state.pressed_buttons.remove(button);
        }
        _ => {}
    }
}

fn flush_pressed_keys(state: &DaemonState) -> Vec<InputEvent> {
    let mut releases = Vec::new();
    for &scancode in &state.pressed_keys {
        releases.push(InputEvent::Key {
            scancode,
            state: KeyState::Released,
        });
    }
    for &button in &state.pressed_buttons {
        releases.push(InputEvent::MouseButton {
            button,
            state: kmflow_proto::ButtonState::Released,
        });
    }
    releases
}

fn opposite_edge(edge: kmflow_proto::Edge) -> kmflow_proto::Edge {
    match edge {
        kmflow_proto::Edge::Right => kmflow_proto::Edge::Left,
        kmflow_proto::Edge::Left => kmflow_proto::Edge::Right,
        kmflow_proto::Edge::Top => kmflow_proto::Edge::Bottom,
        kmflow_proto::Edge::Bottom => kmflow_proto::Edge::Top,
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn gethostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
}

fn query_screen_info(_backend: Backend) -> ScreenInfo {
    // Always try Wayland compositor tools first (cosmic-randr, wlr-randr)
    // — they report accurate scale even under sudo
    if let Some(info) = detect_screen_from_randr() {
        return info;
    }

    #[cfg(feature = "x11")]
    if _backend == Backend::X11 {
        use x11rb::connection::Connection;
        use x11rb::rust_connection::RustConnection;

        if let Ok((conn, screen_num)) = RustConnection::connect(None) {
            let screen = &conn.setup().roots[screen_num];
            let scale = detect_x11_scale();
            return ScreenInfo {
                width: screen.width_in_pixels as u32,
                height: screen.height_in_pixels as u32,
                scale_factor: scale,
                position: kmflow_proto::ScreenPosition {
                    edge: kmflow_proto::Edge::Right,
                    monitor_id: 0,
                },
            };
        }
        warn!("cannot connect to X11 display, falling back to DRM");
    }

    if let Some(info) = detect_screen_from_drm() {
        return info;
    }

    ScreenInfo {
        width: 1920,
        height: 1080,
        scale_factor: 1.0,
        position: kmflow_proto::ScreenPosition {
            edge: kmflow_proto::Edge::Right,
            monitor_id: 0,
        },
    }
}

fn detect_screen_from_randr() -> Option<ScreenInfo> {
    let wl_envs = clipboard::wayland_env();
    debug!(?wl_envs, "detect_screen_from_randr: wayland env");

    // Try cosmic-randr first (COSMIC desktop)
    let mut cmd = std::process::Command::new("cosmic-randr");
    cmd.arg("list");
    cmd.envs(wl_envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    match cmd.output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            debug!(%stdout, "cosmic-randr output");
            return parse_cosmic_randr(&stdout);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            debug!(status = %output.status, %stderr, "cosmic-randr failed");
        }
        Err(e) => {
            debug!(?e, "cosmic-randr not found");
        }
    }

    // Try wlr-randr
    let mut cmd = std::process::Command::new("wlr-randr");
    cmd.envs(wl_envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    match cmd.output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return parse_wlr_randr(&stdout);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            debug!(status = %output.status, %stderr, "wlr-randr failed");
        }
        Err(e) => {
            debug!(?e, "wlr-randr not found");
        }
    }

    None
}

fn parse_cosmic_randr(output: &str) -> Option<ScreenInfo> {
    let mut scale = 1.0f64;
    let mut width = 0u32;
    let mut height = 0u32;

    // Strip ANSI color codes (cosmic-randr outputs colored text)
    let ansi_re = |s: &str| -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip until 'm' (end of ANSI escape)
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
            } else {
                result.push(c);
            }
        }
        result
    };

    let clean = ansi_re(output);

    for line in clean.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Scale:") {
            if let Some(pct_str) = trimmed.strip_prefix("Scale:") {
                let pct_str = pct_str.trim().trim_end_matches('%');
                if let Ok(pct) = pct_str.parse::<f64>() {
                    scale = pct / 100.0;
                    debug!(%scale, "parsed scale");
                }
            }
        }
        if trimmed.contains("(current)") {
            debug!(line = trimmed, "found current mode line");
            if let Some(res) = trimmed.split_whitespace().next() {
                if let Some((w, h)) = res.split_once('x') {
                    width = w.parse().unwrap_or(0);
                    height = h.parse().unwrap_or(0);
                    debug!(%width, %height, "parsed resolution");
                }
            }
        }
    }

    debug!(%width, %height, %scale, "parse_cosmic_randr result");

    if width > 0 && height > 0 {
        // Report logical resolution (physical / scale) since that's what the compositor uses
        let logical_w = (width as f64 / scale) as u32;
        let logical_h = (height as f64 / scale) as u32;
        info!(physical_w = width, physical_h = height, %scale, logical_w, logical_h, "detected screen via cosmic-randr");
        Some(ScreenInfo {
            width: logical_w,
            height: logical_h,
            scale_factor: scale,
            position: kmflow_proto::ScreenPosition {
                edge: kmflow_proto::Edge::Right,
                monitor_id: 0,
            },
        })
    } else {
        None
    }
}

fn parse_wlr_randr(output: &str) -> Option<ScreenInfo> {
    let mut scale = 1.0f64;
    let mut width = 0u32;
    let mut height = 0u32;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Scale:") {
            if let Some(s) = trimmed.strip_prefix("Scale:") {
                if let Ok(v) = s.trim().parse::<f64>() {
                    scale = v;
                }
            }
        }
        // "1920x1080 px, 60.000000 Hz (current)"
        if trimmed.contains("current") && trimmed.contains('x') {
            if let Some(res) = trimmed.split_whitespace().next() {
                if let Some((w, h)) = res.split_once('x') {
                    width = w.parse().unwrap_or(0);
                    height = h.parse().unwrap_or(0);
                }
            }
        }
    }

    if width > 0 && height > 0 {
        let logical_w = (width as f64 / scale) as u32;
        let logical_h = (height as f64 / scale) as u32;
        info!(physical_w = width, physical_h = height, %scale, logical_w, logical_h, "detected screen via wlr-randr");
        Some(ScreenInfo {
            width: logical_w,
            height: logical_h,
            scale_factor: scale,
            position: kmflow_proto::ScreenPosition {
                edge: kmflow_proto::Edge::Right,
                monitor_id: 0,
            },
        })
    } else {
        None
    }
}

fn detect_screen_from_drm() -> Option<ScreenInfo> {
    // Read physical resolution from DRM/KMS as last resort
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().to_string();
        if !name.contains("HDMI") && !name.contains("DP") && !name.contains("eDP") {
            continue;
        }
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        if !status.trim().eq_ignore_ascii_case("connected") {
            continue;
        }
        let modes = std::fs::read_to_string(path.join("modes")).unwrap_or_default();
        if let Some(first_mode) = modes.lines().next() {
            if let Some((w, h)) = first_mode.split_once('x') {
                let width: u32 = w.parse().unwrap_or(0);
                let height: u32 = h.parse().unwrap_or(0);
                if width > 0 && height > 0 {
                    info!(
                        width,
                        height, "detected screen from DRM (scale unknown, assuming 1.0)"
                    );
                    return Some(ScreenInfo {
                        width,
                        height,
                        scale_factor: 1.0,
                        position: kmflow_proto::ScreenPosition {
                            edge: kmflow_proto::Edge::Right,
                            monitor_id: 0,
                        },
                    });
                }
            }
        }
    }
    None
}

#[cfg(feature = "x11")]
fn detect_x11_scale() -> f64 {
    // Check GDK_SCALE or QT_SCALE_FACTOR env vars
    if let Ok(s) = std::env::var("GDK_SCALE") {
        if let Ok(v) = s.parse::<f64>() {
            return v;
        }
    }
    if let Ok(s) = std::env::var("QT_SCALE_FACTOR") {
        if let Ok(v) = s.parse::<f64>() {
            return v;
        }
    }
    // Check Xft.dpi from xrdb
    if let Ok(output) = std::process::Command::new("xrdb").arg("-query").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.starts_with("Xft.dpi:") {
                if let Some(dpi_str) = line.strip_prefix("Xft.dpi:") {
                    if let Ok(dpi) = dpi_str.trim().parse::<f64>() {
                        return dpi / 96.0;
                    }
                }
            }
        }
    }
    1.0
}
