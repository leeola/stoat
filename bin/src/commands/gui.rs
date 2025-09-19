/// Launch the GUI application
pub fn run(
    paths: Vec<std::path::PathBuf>,
    _input_sequence: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Launching Stoat GUI...");

    if !paths.is_empty() {
        tracing::info!("Opening {} file(s):", paths.len());
        for path in &paths {
            tracing::info!("  - {}", path.display());
        }
    }

    // Note: input sequence support will be added back in a future version
    // if let Some(input) = &input_sequence {
    //     tracing::info!("Input sequence provided: {}", input);
    // }

    // Run the GUI application
    // TODO: Pass the paths to stoat_gui::run() when it's updated to accept them
    stoat_gui::run().map_err(|e| format!("Failed to run GUI: {e}"))?;

    Ok(())
}
