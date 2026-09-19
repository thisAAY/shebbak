use crate::model::{AxRole, WindowKind};

pub fn classify(layer: i64, ax_role: AxRole) -> WindowKind {
    if layer != 0 {
        WindowKind::Transient
    } else if ax_role == AxRole::Sheet {
        WindowKind::Sheet
    } else {
        WindowKind::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
