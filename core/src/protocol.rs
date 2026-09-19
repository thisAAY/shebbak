use serde::{Deserialize, Serialize};

/// Host-side window identifier (CGWindowID on macOS).
pub type WindowId = u32;

/// Window kind/type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    Normal,
    Sheet,
    Transient,
}

/// Host → client messages on the control data channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostMessage {
    WindowOpened {
        window_id: WindowId,
        title: String,
        kind: WindowKind,
        parent_id: Option<WindowId>,
        offset_x: f64,
        offset_y: f64,
        width: f64,
        height: f64,
        track_id: Option<String>,
    },
    WindowResized {
        window_id: WindowId,
        width: f64,
        height: f64,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WindowMinimized {
        window_id: WindowId,
    },
    WindowRestored {
        window_id: WindowId,
    },
    WindowClosed {
        window_id: WindowId,
    },
    /// Change-triggered pointer-coordinate mapping for one window's stream
    /// (`window_local = offset + scale * mirror_point`). Identity is
    /// implicit at window open; sent only when the mapping changes — i.e.
    /// while SCK letterboxes an oversized child window into the frame, and
    /// again when it returns to 1:1.
    InputMapping {
        window_id: WindowId,
        scale_x: f64,
        scale_y: f64,
        offset_x: f64,
        offset_y: f64,
    },
    SdpOffer {
        sdp: String,
    },
}

/// Client → host messages on the control data channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    MouseInput {
        window_id: WindowId,
        x: f64,
        y: f64,
        button: MouseButton,
        action: MouseAction,
    },
    MouseMove {
        window_id: WindowId,
        x: f64,
        y: f64,
    },
    KeyEvent {
        window_id: WindowId,
        key_code: u16,
        down: bool,
        flags: u64,
    },
    FocusChange {
        window_id: WindowId,
    },
    ResizeRequest {
        window_id: WindowId,
        width: f64,
        height: f64,
    },
    CloseRequest {
        window_id: WindowId,
    },
    SdpAnswer {
        sdp: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    Down,
    Up,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_host(msg: HostMessage) -> HostMessage {
        serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap()
    }

    fn roundtrip_client(msg: ClientMessage) -> ClientMessage {
        serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap()
    }

    #[test]
    fn host_messages_roundtrip_v2() {
        let msgs = vec![
            HostMessage::WindowOpened {
                window_id: 42,
                title: "Safari".into(),
                kind: WindowKind::Normal,
                parent_id: None,
                offset_x: 0.0,
                offset_y: 0.0,
                width: 800.0,
                height: 600.0,
                track_id: Some("win-42".into()),
            },
            HostMessage::WindowOpened {
                window_id: 43,
                title: "".into(),
                kind: WindowKind::Transient,
                parent_id: Some(42),
                offset_x: 15.0,
                offset_y: 30.0,
                width: 200.0,
                height: 340.0,
                track_id: None,
            },
            HostMessage::WindowResized {
                window_id: 42,
                width: 640.0,
                height: 480.0,
            },
            HostMessage::WindowTitleChanged {
                window_id: 42,
                title: "New".into(),
            },
            HostMessage::WindowMinimized { window_id: 42 },
            HostMessage::WindowRestored { window_id: 42 },
            HostMessage::WindowClosed { window_id: 42 },
            HostMessage::InputMapping {
                window_id: 42,
                scale_x: 7.0 / 6.0,
                scale_y: 7.0 / 6.0,
                offset_x: -66.67,
                offset_y: -100.0,
            },
            HostMessage::SdpOffer {
                sdp: "{\"type\":\"offer\"}".into(),
            },
        ];
        for m in msgs {
            assert_eq!(roundtrip_host(m.clone()), m);
        }
    }

    #[test]
    fn client_messages_roundtrip_v2() {
        let msgs = vec![
            ClientMessage::MouseInput {
                window_id: 42,
                x: 1.0,
                y: 2.0,
                button: MouseButton::Left,
                action: MouseAction::Down,
            },
            ClientMessage::MouseMove {
                window_id: 42,
                x: 3.0,
                y: 4.0,
            },
            ClientMessage::KeyEvent {
                window_id: 42,
                key_code: 0,
                down: true,
                flags: 0x0010_0000,
            },
            ClientMessage::FocusChange { window_id: 42 },
            ClientMessage::ResizeRequest {
                window_id: 42,
                width: 500.0,
                height: 400.0,
            },
            ClientMessage::CloseRequest { window_id: 42 },
            ClientMessage::SdpAnswer {
                sdp: "{\"type\":\"answer\"}".into(),
            },
        ];
        for m in msgs {
            assert_eq!(roundtrip_client(m.clone()), m);
        }
    }

    #[test]
    fn tagged_wire_format_is_stable_v2() {
        let json = serde_json::to_string(&HostMessage::WindowMinimized { window_id: 7 }).unwrap();
        assert_eq!(json, r#"{"type":"WindowMinimized","window_id":7}"#);
        let json = serde_json::to_string(&ClientMessage::KeyEvent {
            window_id: 7,
            key_code: 12,
            down: false,
            flags: 0,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"KeyEvent","window_id":7,"key_code":12,"down":false,"flags":0}"#
        );
    }
}
