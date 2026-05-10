use std::{backtrace::Backtrace, panic, sync::Once};

/// Install a process-global panic hook that captures `panic_message`,
/// `location`, and a forced backtrace via [`tracing::error`] so the same
/// information is preserved in `stoat-<pid>.log` after the GPUI window is
/// gone. Chains to the prior hook so the default stderr message still
/// prints. Idempotent across repeated calls.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let prior = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let panic_message = match info.payload().downcast_ref::<&'static str>() {
                Some(message) => *message,
                None => match info.payload().downcast_ref::<String>() {
                    Some(message) => message.as_str(),
                    None => "Box<Any>",
                },
            };
            let location = info
                .location()
                .map(|loc| format!("{}:{}", loc.file(), loc.line()));
            let backtrace = Backtrace::force_capture();
            tracing::error!(panic = true, ?location, %panic_message, %backtrace, "stoat panic");

            prior(info);
        }));
    });
}
