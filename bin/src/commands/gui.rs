/// Launch the GUI application
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("Launching Stoat GUI...");

    // Run the GUI application
    stoat_gui::run_gui().map_err(|e| format!("Failed to run GUI: {e}"))?;

    Ok(())
}
