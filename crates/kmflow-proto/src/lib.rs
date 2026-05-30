use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 4242;
pub const MAX_FRAME_SIZE: usize = 1200;
pub const MDNS_SERVICE_TYPE: &str = "_kmflow._udp.local.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFrame {
    pub seq: u32,
    pub timestamp_us: u64,
    pub events: Vec<InputEvent>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        dx: f64,
        dy: f64,
    },
    MouseButton {
        button: MouseButton,
        state: ButtonState,
    },
    Scroll {
        dx: f64,
        dy: f64,
    },
    Key {
        scancode: u32,
        state: KeyState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    pub position: ScreenPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenPosition {
    pub edge: Edge,
    pub monitor_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlCommand {
    Ping {
        timestamp_us: u64,
    },
    Pong {
        echo_timestamp_us: u64,
    },
    Handshake {
        protocol_version: u32,
        hostname: String,
        screen_info: ScreenInfo,
        edge: Option<Edge>,
    },
    HandshakeAck {
        protocol_version: u32,
        hostname: String,
        screen_info: ScreenInfo,
        edge: Option<Edge>,
    },
    SwitchFocus {
        to_peer: String,
    },
    ReleaseFocus,
    PeerDisconnecting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
    Image { mime: String, data: Vec<u8> },
    FileUri(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardPayload {
    pub origin_peer_id: String,
    pub content: ClipboardContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub fingerprint: String,
    pub hostname: String,
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KmflowConfig {
    pub port: u16,
    pub hostname: Option<String>,
    pub layout: Vec<LayoutEntry>,
    pub known_peers: Vec<PeerIdentity>,
    #[serde(default)]
    pub scale_override: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutEntry {
    pub peer_hostname: String,
    pub edge: Edge,
}

impl Default for KmflowConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: None,
            layout: Vec::new(),
            known_peers: Vec::new(),
            scale_override: None,
        }
    }
}

pub fn encode_frame(frame: &EventFrame) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(frame)
}

pub fn decode_frame(data: &[u8]) -> Result<EventFrame, serde_json::Error> {
    serde_json::from_slice(data)
}

pub fn encode_control(cmd: &ControlCommand) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(cmd)
}

pub fn decode_control(data: &[u8]) -> Result<ControlCommand, serde_json::Error> {
    serde_json::from_slice(data)
}

pub fn encode_clipboard(payload: &ClipboardPayload) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(payload)
}

pub fn decode_clipboard(data: &[u8]) -> Result<ClipboardPayload, serde_json::Error> {
    serde_json::from_slice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_event_frame() {
        let frame = EventFrame {
            seq: 42,
            timestamp_us: 123456789,
            events: vec![
                InputEvent::MouseMove { dx: 10.5, dy: -3.2 },
                InputEvent::Key {
                    scancode: 0x04,
                    state: KeyState::Pressed,
                },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        assert!(bytes.len() < MAX_FRAME_SIZE);
        let decoded = decode_frame(&bytes).unwrap();
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.events.len(), 2);
    }

    #[test]
    fn roundtrip_control_command() {
        let cmd = ControlCommand::Handshake {
            protocol_version: PROTOCOL_VERSION,
            hostname: "my-pc".to_string(),
            screen_info: ScreenInfo {
                width: 1920,
                height: 1080,
                scale_factor: 1.0,
                position: ScreenPosition {
                    edge: Edge::Right,
                    monitor_id: 0,
                },
            },
            edge: Some(Edge::Right),
        };
        let bytes = encode_control(&cmd).unwrap();
        let decoded = decode_control(&bytes).unwrap();
        match decoded {
            ControlCommand::Handshake { hostname, .. } => assert_eq!(hostname, "my-pc"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_clipboard_text() {
        let payload = ClipboardPayload {
            origin_peer_id: "host-a".to_string(),
            content: ClipboardContent::Text("你好世界 hello".to_string()),
        };
        let bytes = encode_clipboard(&payload).unwrap();
        let decoded = decode_clipboard(&bytes).unwrap();
        assert_eq!(decoded.origin_peer_id, "host-a");
        match decoded.content {
            ClipboardContent::Text(t) => assert_eq!(t, "你好世界 hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_clipboard_image() {
        let payload = ClipboardPayload {
            origin_peer_id: "host-b".to_string(),
            content: ClipboardContent::Image {
                mime: "image/png".to_string(),
                data: vec![0x89, 0x50, 0x4E, 0x47],
            },
        };
        let bytes = encode_clipboard(&payload).unwrap();
        let decoded = decode_clipboard(&bytes).unwrap();
        match decoded.content {
            ClipboardContent::Image { mime, data } => {
                assert_eq!(mime, "image/png");
                assert_eq!(data, vec![0x89, 0x50, 0x4E, 0x47]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn clipboard_empty_text_roundtrip() {
        let payload = ClipboardPayload {
            origin_peer_id: String::new(),
            content: ClipboardContent::Text(String::new()),
        };
        let bytes = encode_clipboard(&payload).unwrap();
        let decoded = decode_clipboard(&bytes).unwrap();
        match decoded.content {
            ClipboardContent::Text(t) => assert!(t.is_empty()),
            _ => panic!("wrong variant"),
        }
    }
}
