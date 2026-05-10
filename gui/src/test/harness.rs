use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Empty, Entity, TestAppContext, WindowBounds,
    WindowHandle, WindowOptions,
};
use std::time::Duration;

pub struct TestHarness {
    cx: TestAppContext,
    window: AnyWindowHandle,
}

impl TestHarness {
    pub fn new() -> Self {
        let cx = TestAppContext::single();
        let window: WindowHandle<Empty> = cx.update(|cx| {
            let bounds = Bounds::maximized(None, cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| cx.new(|_cx| Empty),
            )
            .expect("open test window")
        });
        Self {
            cx,
            window: window.into(),
        }
    }

    pub fn simulate_keystrokes(&mut self, keystrokes: &str) {
        self.cx.simulate_keystrokes(self.window, keystrokes);
    }

    pub fn run_until_parked(&mut self) {
        self.cx.run_until_parked();
    }

    pub fn advance_clock(&self, duration: Duration) {
        self.cx.executor().advance_clock(duration);
    }

    pub fn read_entity<T: 'static, R>(
        &self,
        entity: &Entity<T>,
        f: impl FnOnce(&T, &App) -> R,
    ) -> R {
        self.cx.read_entity(entity, f)
    }
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new()
    }
}
