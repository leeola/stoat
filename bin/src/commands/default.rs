use stoat::{Action, Key, Stoat};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut stoat = Stoat::new();
    stoat.keymap(Key::char('q'), Action::Exit, |_| true);
    stoat.keymap(Key::esc(), Action::Exit, |_| true);

    Ok(stoat::run(stoat)?)
}
