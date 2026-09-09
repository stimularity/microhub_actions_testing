use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent, WebviewWindow};

/// How long to wait for Shiny to start listening before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The R process backing the window, so it can be killed when the app exits.
struct RProcess(Mutex<Option<Child>>);

/// Ask the OS for an unused port, then release it for R to bind.
fn free_port() -> std::io::Result<u16> {
  let listener = TcpListener::bind("127.0.0.1:0")?;
  listener.local_addr().map(|addr| addr.port())
}

/// Locate Rscript. Apps launched from Finder inherit a minimal PATH that omits
/// the usual R install locations, so the known paths are probed explicitly.
fn rscript_path() -> Option<PathBuf> {
  if let Some(configured) = std::env::var_os("MICROHUB_R_BIN") {
    let path = PathBuf::from(configured);
    return path.is_file().then_some(path);
  }

  let candidates = [
    "/Library/Frameworks/R.framework/Resources/bin/Rscript",
    "/opt/homebrew/bin/Rscript",
    "/usr/local/bin/Rscript",
    "/usr/bin/Rscript",
  ];

  candidates
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
    .or_else(|| {
      // Fall back to PATH, which is how `tauri dev` normally finds R.
      std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
          .map(|dir| dir.join("Rscript"))
          .find(|path| path.is_file())
      })
    })
}

/// The Shiny app directory: an override for development, otherwise the copy
/// bundled into the app's resources.
fn app_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
  if let Some(configured) = std::env::var_os("MICROHUB_APP_DIR") {
    let path = PathBuf::from(configured);
    return path.is_dir().then_some(path);
  }

  let bundled = app.path().resource_dir().ok()?.join("app");
  bundled.is_dir().then_some(bundled)
}

fn spawn_shiny(rscript: &PathBuf, app_dir: &PathBuf, port: u16) -> std::io::Result<Child> {
  // RunEvent::Exit covers a clean quit, but not SIGKILL or a crash. The
  // watchdog makes R responsible for noticing that its parent is gone.
  let expr = format!(
    "if (requireNamespace('later', quietly = TRUE)) {{ \
       local({{ \
         parent <- {parent}; \
         watch <- function() {{ \
           if (system2('kill', c('-0', parent), stdout = FALSE, stderr = FALSE) != 0) quit('no'); \
           later::later(watch, 5) \
         }}; \
         later::later(watch, 5) \
       }}) \
     }}; \
     shiny::runApp(appDir = {app_dir:?}, port = {port}, host = '127.0.0.1', launch.browser = FALSE)",
    parent = std::process::id(),
    app_dir = app_dir.to_string_lossy(),
    port = port
  );

  Command::new(rscript)
    .arg("--no-save")
    .arg("--no-restore")
    .arg("-e")
    .arg(expr)
    .current_dir(app_dir)
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .spawn()
}

/// Poll until Shiny accepts connections, or the timeout expires.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
  let addr = SocketAddr::from(([127, 0, 0, 1], port));
  let deadline = Instant::now() + timeout;

  while Instant::now() < deadline {
    if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
      return true;
    }
    std::thread::sleep(Duration::from_millis(250));
  }

  false
}

/// Report a startup failure on the splash page rather than leaving it spinning.
fn show_error(window: &WebviewWindow, message: &str) {
  let escaped = message.replace('\\', "\\\\").replace('\'', "\\'");
  let _ = window.eval(&format!("window.showError('{}')", escaped));
}

fn start_backend(app: &tauri::AppHandle) -> Result<Child, String> {
  let window = app
    .get_webview_window("main")
    .ok_or_else(|| "main window is missing".to_string())?;

  let rscript = rscript_path().ok_or_else(|| {
    "Could not find R. Install R, or set MICROHUB_R_BIN to the Rscript binary.".to_string()
  })?;

  let app_dir = app_dir(app).ok_or_else(|| {
    "Could not find the Shiny app directory. Set MICROHUB_APP_DIR for development.".to_string()
  })?;

  let port = free_port().map_err(|e| format!("Could not reserve a port: {e}"))?;

  log::info!("starting {} in {} on port {}", rscript.display(), app_dir.display(), port);
  let child = spawn_shiny(&rscript, &app_dir, port).map_err(|e| format!("Could not start R: {e}"))?;

  // Polling blocks, so hand off to a worker and let setup() return.
  std::thread::spawn(move || {
    if wait_for_port(port, STARTUP_TIMEOUT) {
      let url = format!("http://127.0.0.1:{port}");
      match tauri::Url::parse(&url) {
        Ok(parsed) => {
          if let Err(e) = window.navigate(parsed) {
            show_error(&window, &format!("Could not open {url}: {e}"));
          }
        }
        Err(e) => show_error(&window, &format!("Invalid URL {url}: {e}")),
      }
    } else {
      show_error(
        &window,
        "R did not start listening in time. Run from a terminal to see its output.",
      );
    }
  });

  Ok(child)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  let app = tauri::Builder::default()
    .manage(RProcess(Mutex::new(None)))
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
        if let Some(window) = app.get_webview_window("main") {
          window.open_devtools();
        }
      }

      match start_backend(app.handle()) {
        Ok(child) => {
          *app.state::<RProcess>().0.lock().unwrap() = Some(child);
        }
        Err(message) => {
          log::error!("{message}");
          if let Some(window) = app.get_webview_window("main") {
            show_error(&window, &message);
          }
        }
      }

      Ok(())
    })
    .build(tauri::generate_context!())
    .expect("error while building tauri application");

  app.run(|app, event| {
    // Without this, closing the window leaves R running in the background.
    if let RunEvent::Exit = event {
      if let Some(mut child) = app.state::<RProcess>().0.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
      }
    }
  });
}
