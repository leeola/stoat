use snafu::Whatever;

pub fn run() -> Result<(), Whatever> {
    stoat_gui::install_panic_hook();
    stoat_gui::run();
    Ok(())
}
