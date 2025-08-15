/// Launch the GUI application
pub fn run(_input_sequence: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Launching Stoat GUI...");

    // Note: input sequence support will be added back in a future version
    // if let Some(input) = &input_sequence {
    //     tracing::info!("Input sequence provided: {}", input);
    // }

    // Run the GUI application
    stoat_gui::run().map_err(|e| format!("Failed to run GUI: {e}"))?;

    Ok(())
}
