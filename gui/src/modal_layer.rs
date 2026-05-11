use gpui::{Context, FocusHandle};

/// Placeholder modal layer entity. The full implementation --
/// modal lifecycle, focus subscription, dismiss-on-blur, modal
/// stack, and the `ModalView` trait -- lives under the
/// "Foundation: ModalLayer" parent in `.todo-plans/TODO.md`
/// (lines 58-62). For now `Workspace` owns one of these so its
/// field set is stable; the entity carries only a focus handle.
pub struct ModalLayer {
    focus_handle: FocusHandle,
}

impl ModalLayer {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[test]
    fn constructs_with_focus_handle() {
        let cx = TestAppContext::single();
        let modal = cx.update(|cx| cx.new(ModalLayer::new));
        modal.read_with(&cx, |m, _| {
            let _ = m.focus_handle();
        });
    }
}
