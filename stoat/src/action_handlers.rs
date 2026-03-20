use crate::app::UpdateEffect;
use stoat_action::{Action, ActionKind};

pub fn dispatch(action: &dyn Action) -> UpdateEffect {
    match action.kind() {
        ActionKind::Quit => UpdateEffect::Quit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_action::Quit;

    #[test]
    fn dispatch_quit() {
        assert_eq!(dispatch(&Quit), UpdateEffect::Quit);
    }
}
