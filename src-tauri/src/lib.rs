#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      // Run the bundled app with MICROHUB_DEBUG=1 to open the web inspector.
      if std::env::var("MICROHUB_DEBUG").is_ok() {
        use tauri::Manager;
        if let Some(window) = app.get_webview_window("main") {
          window.open_devtools();
        }
      }

      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
