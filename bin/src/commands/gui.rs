/// Launch the GUI application
pub fn run(
    paths: Vec<std::path::PathBuf>,
    input_sequence: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Launching Stoat GUI...");

    if let Some(input) = &input_sequence {
        tracing::info!("Input sequence provided: {}", input);
    }

    // Run the GUI application - it will create Stoat with proper App context
    stoat_gui::run_with_paths(paths, input_sequence)
        .map_err(|e| format!("Failed to run GUI: {e}"))?;

    Ok(())
}
