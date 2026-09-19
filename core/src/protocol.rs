use serde::{Deserialize, Serialize};

/// Host-side window identifier (CGWindowID on macOS).
pub type WindowId = u32;

/// Host-side application identifier: the shared app's pid (stable per session).
pub type AppId = i32;

/// Host → client messages on the control data channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HostMessage {
    WindowOpened {
        window_id: WindowId,
        title: String,
        width: f64,
        height: f64,
        track_id: String,
        app_id: AppId,
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
    /// Sent once per shared app, before its first `WindowOpened`.
    /// `icon_png` is base64 PNG at up to 256 px; empty when the host app
    /// has no usable icon.
    AppAnnounced {
        app_id: AppId,
        name: String,
        icon_png: String,
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
    /// Stop mirroring this app: remove its tracks and encode pipelines.
    /// The host app itself is untouched.
    UnsubscribeApp {
        app_id: AppId,
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
                width: 800.0,
                height: 600.0,
                track_id: "win-42".into(),
                app_id: 501,
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
            HostMessage::AppAnnounced {
                app_id: 501,
                name: "Safari".into(),
                icon_png: "aGVsbG8=".into(),
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
            ClientMessage::UnsubscribeApp { app_id: 501 },
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

    #[test]
    fn app_identity_wire_format_is_stable_v3() {
        let json = serde_json::to_string(&HostMessage::AppAnnounced {
            app_id: 7,
            name: "Safari".into(),
            icon_png: String::new(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"AppAnnounced","app_id":7,"name":"Safari","icon_png":""}"#
        );
        let json = serde_json::to_string(&ClientMessage::UnsubscribeApp { app_id: 7 }).unwrap();
        assert_eq!(json, r#"{"type":"UnsubscribeApp","app_id":7}"#);
    }
}
