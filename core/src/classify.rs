use crate::model::{AxRole, SnapshotWindow, WindowInfo};
use crate::protocol::WindowKind;

pub fn classify(layer: i64, ax_role: AxRole) -> WindowKind {
    if layer != 0 {
        WindowKind::Transient
    } else if ax_role == AxRole::Sheet {
        WindowKind::Sheet
    } else {
        WindowKind::Normal
    }
}

pub fn parent_for<'a>(
    child: &SnapshotWindow,
    ordered: &'a [SnapshotWindow],
) -> Option<&'a SnapshotWindow> {
    ordered.iter().find(|w| {
        w.info.id != child.info.id
            && w.pid == child.pid
            && classify(w.layer, w.ax_role) == WindowKind::Normal
            && !w.minimized
    })
}

pub fn parent_offset(parent: &WindowInfo, child: &WindowInfo) -> (f64, f64) {
    (child.x - parent.x, child.y - parent.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: u32, pid: i32, layer: i64, role: AxRole, x: f64, y: f64) -> SnapshotWindow {
        SnapshotWindow {
            info: WindowInfo {
                id,
                title: String::new(),
                x,
                y,
                width: 100.0,
                height: 100.0,
            },
            pid,
            layer,
            on_screen: true,
            ax_role: role,
            minimized: false,
        }
    }

    #[test]
    fn layer_zero_no_special_role_is_normal() {
        assert_eq!(classify(0, AxRole::Window), WindowKind::Normal);
        assert_eq!(classify(0, AxRole::Unknown), WindowKind::Normal);
    }

    #[test]
    fn sheet_role_is_sheet_even_at_layer_zero() {
        assert_eq!(classify(0, AxRole::Sheet), WindowKind::Sheet);
    }

    #[test]
    fn nonzero_layer_is_transient_regardless_of_role() {
        assert_eq!(classify(101, AxRole::Unknown), WindowKind::Transient); // menu level
        assert_eq!(classify(3, AxRole::Window), WindowKind::Transient);
    }

    #[test]
    fn parent_is_frontmost_normal_of_same_pid() {
        let ordered = vec![
            snap(10, 1, 101, AxRole::Unknown, 50.0, 60.0), // the menu itself, frontmost
            snap(11, 2, 0, AxRole::Window, 0.0, 0.0),      // other app's window in front
            snap(12, 1, 0, AxRole::Window, 20.0, 30.0),    // frontmost normal of pid 1
            snap(13, 1, 0, AxRole::Window, 500.0, 30.0),
        ];
        let parent = parent_for(&ordered[0], &ordered).unwrap();
        assert_eq!(parent.info.id, 12);
    }

    #[test]
    fn parent_never_self_and_none_when_no_normal_sibling() {
        let ordered = vec![snap(10, 1, 101, AxRole::Unknown, 0.0, 0.0)];
        assert!(parent_for(&ordered[0], &ordered).is_none());
    }

    #[test]
    fn offset_is_child_minus_parent_origin() {
        let p = WindowInfo {
            id: 1,
            title: String::new(),
            x: 100.0,
            y: 50.0,
            width: 800.0,
            height: 600.0,
        };
        let c = WindowInfo {
            id: 2,
            title: String::new(),
            x: 130.0,
            y: 90.0,
            width: 200.0,
            height: 300.0,
        };
        assert_eq!(parent_offset(&p, &c), (30.0, 40.0));
    }
}
