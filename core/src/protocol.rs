use serde::{Deserialize, Serialize};

/// Host-side window identifier (CGWindowID on macOS).
pub type WindowId = u32;

/// Host → client messages on the control data channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostMessage {
    WindowOpened {
        window_id: WindowId,
        title: String,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        track_id: String,
    },
    WindowMoved { window_id: WindowId, x: f64, y: f64 },
    WindowResized { window_id: WindowId, width: f64, height: f64 },
    WindowTitleChanged { window_id: WindowId, title: String },
    WindowClosed { window_id: WindowId },
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
    CloseWindow { window_id: WindowId },
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
    fn host_messages_roundtrip() {
        let msgs = vec![
            HostMessage::WindowOpened {
                window_id: 42,
                title: "Safari".into(),
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 600.0,
                track_id: "win-42".into(),
            },
            HostMessage::WindowMoved { window_id: 42, x: 15.0, y: 25.0 },
            HostMessage::WindowResized { window_id: 42, width: 640.0, height: 480.0 },
            HostMessage::WindowTitleChanged { window_id: 42, title: "New".into() },
            HostMessage::WindowClosed { window_id: 42 },
        ];
        for m in msgs {
            assert_eq!(roundtrip_host(m.clone()), m);
        }
    }

    #[test]
    fn client_messages_roundtrip() {
        let msgs = vec![
            ClientMessage::MouseInput {
                window_id: 42,
                x: 100.5,
                y: 200.5,
                button: MouseButton::Left,
                action: MouseAction::Down,
            },
            ClientMessage::MouseInput {
                window_id: 42,
                x: 100.5,
                y: 200.5,
                button: MouseButton::Right,
                action: MouseAction::Up,
            },
            ClientMessage::CloseWindow { window_id: 42 },
        ];
        for m in msgs {
            assert_eq!(roundtrip_client(m.clone()), m);
        }
    }

    #[test]
    fn tagged_wire_format_is_stable() {
        let json = serde_json::to_string(&HostMessage::WindowClosed { window_id: 7 }).unwrap();
        assert_eq!(json, r#"{"type":"WindowClosed","window_id":7}"#);
        let json = serde_json::to_string(&ClientMessage::MouseInput {
            window_id: 7,
            x: 1.0,
            y: 2.0,
            button: MouseButton::Left,
            action: MouseAction::Down,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"MouseInput","window_id":7,"x":1.0,"y":2.0,"button":"left","action":"down"}"#
        );
    }
}
