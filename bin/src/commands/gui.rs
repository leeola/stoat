/// Launch the GUI application
pub fn run(
    stoat: stoat::Stoat,
    _input_sequence: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Launching Stoat GUI...");

    // Log if we have loaded content
    let content_len = stoat.buffer_contents().len();
    if content_len > 0 {
        tracing::info!(
            "Starting GUI with {} characters of loaded content",
            content_len
        );
    }

    // Note: input sequence support will be added back in a future version
    // if let Some(input) = &input_sequence {
    //     tracing::info!("Input sequence provided: {}", input);
    // }

    // Run the GUI application with the Stoat instance
    stoat_gui::run_with_stoat(Some(stoat)).map_err(|e| format!("Failed to run GUI: {e}"))?;

    Ok(())
}
