//! Binary entry point for the `stoatty` terminal: opens a window running the
//! user's default shell and drives the event loop until the window closes.

fn main() {
    stoatty::app::run();
}
