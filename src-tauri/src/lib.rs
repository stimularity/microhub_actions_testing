use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent, WebviewWindow};

/// How long to wait for Shiny to start listening before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Fixed location of the bundled runtime. The runtime is built and relocated to
/// this exact path in CI (tools/build-macos-runtime.sh), so the paths compiled
/// into R's binaries are valid on the user's machine without further fixups.
const RUNTIME_PREFIX: &str = "/Users/Shared/MicroHub";

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

  let bundled = PathBuf::from(RUNTIME_PREFIX).join("R.framework/Resources/bin/Rscript");
  if bundled.is_file() {
    return Some(bundled);
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

  let mut command = Command::new(rscript);

  // FourCAT's find_fourcat_python() takes RETICULATE_PYTHON first, so pointing
  // it at the bundled interpreter is all that is needed (see R/FourCAT.R:41).
  let bundled_python = PathBuf::from(RUNTIME_PREFIX).join("python/bin/python3");
  if bundled_python.is_file() {
    command.env("RETICULATE_PYTHON", &bundled_python);
  }

  command
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

/// Update the splash text while a slow step runs.
fn set_status(window: &WebviewWindow, message: &str) {
  let escaped = message.replace('\\', "\\\\").replace('\'', "\\'");
  let _ = window.eval(&format!("window.setStatus('{}')", escaped));
}

/// True when the runtime is present and was installed by this app version.
fn runtime_ready(prefix: &Path, version: &str) -> bool {
  if !prefix.join("R.framework/Resources/bin/Rscript").is_file() {
    return false;
  }
  std::fs::read_to_string(prefix.join(".microhub-runtime-version"))
    .map(|installed| installed.trim() == version)
    .unwrap_or(false)
}

/// Unpack the bundled runtime to its fixed path on first launch (and after an
/// upgrade). The archive is built by tools/build-macos-runtime.sh with paths
/// already baked in, so extraction is all that is required.
fn ensure_runtime(app: &tauri::AppHandle, window: &WebviewWindow) -> Result<(), String> {
  // A developer-supplied R takes priority and needs no bundled runtime.
  if std::env::var_os("MICROHUB_R_BIN").is_some() {
    return Ok(());
  }

  let prefix = PathBuf::from(RUNTIME_PREFIX);
  let version = app.package_info().version.to_string();
  if runtime_ready(&prefix, &version) {
    return Ok(());
  }

  // A zero-byte file is the placeholder a dev checkout uses to satisfy the
  // bundler; only a real archive counts as a bundled runtime.
  let bundled = app
    .path()
    .resource_dir()
    .ok()
    .map(|dir| dir.join("runtime.tar.zst"))
    .filter(|path| {
      std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
    });

  let tarball = match bundled {
    Some(path) => path,
    // No bundled runtime: a dev build. Fall back to whatever R is installed.
    None => return Ok(()),
  };

  let parent = prefix
    .parent()
    .ok_or_else(|| format!("{RUNTIME_PREFIX} has no parent directory"))?;

  if prefix.exists() {
    // Refuse to recurse outside the intended location.
    if !prefix.starts_with("/Users/Shared/") {
      return Err(format!("refusing to remove {}", prefix.display()));
    }
    set_status(window, "Removing the previous runtime\u{2026}");
    std::fs::remove_dir_all(&prefix)
      .map_err(|e| format!("Could not remove the old runtime at {}: {e}", prefix.display()))?;
  }

  set_status(
    window,
    "Installing the R runtime. This happens once and takes a few minutes\u{2026}",
  );
  log::info!("extracting {} to {}", tarball.display(), parent.display());

  std::fs::create_dir_all(parent)
    .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;

  // bsdtar on macOS 13+ handles zstd natively.
  let status = Command::new("/usr/bin/tar")
    .arg("--zstd")
    .arg("-xf")
    .arg(&tarball)
    .arg("-C")
    .arg(parent)
    .status()
    .map_err(|e| format!("Could not run tar: {e}"))?;

  if !status.success() {
    return Err(format!("Unpacking the runtime failed (tar exited with {status})"));
  }

  std::fs::write(prefix.join(".microhub-runtime-version"), &version)
    .map_err(|e| format!("Could not record the runtime version: {e}"))?;

  Ok(())
}

fn start_backend(app: &tauri::AppHandle) -> Result<Child, String> {
  let window = app
    .get_webview_window("main")
    .ok_or_else(|| "main window is missing".to_string())?;

  ensure_runtime(app, &window)?;

  let rscript = rscript_path().ok_or_else(|| {
    "Could not find R. Install R, or set MICROHUB_R_BIN to the Rscript binary.".to_string()
  })?;

  let app_dir = app_dir(app).ok_or_else(|| {
    "Could not find the Shiny app directory. Set MICROHUB_APP_DIR for development.".to_string()
  })?;

  let port = free_port().map_err(|e| format!("Could not reserve a port: {e}"))?;

  log::info!("starting {} in {} on port {}", rscript.display(), app_dir.display(), port);
  let child = spawn_shiny(&rscript, &app_dir, port).map_err(|e| format!("Could not start R: {e}"))?;

  set_status(&window, "Starting the forecasting environment\u{2026}");

  if wait_for_port(port, STARTUP_TIMEOUT) {
    let url = format!("http://127.0.0.1:{port}");
    let parsed = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL {url}: {e}"))?;
    window
      .navigate(parsed)
      .map_err(|e| format!("Could not open {url}: {e}"))?;
  } else {
    return Err(
      "R did not start listening in time. Run from a terminal to see its output.".to_string(),
    );
  }

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

      // Unpacking the runtime and booting R take minutes; keep setup() quick
      // so the splash renders instead of the window hanging blank.
      let handle = app.handle().clone();
      std::thread::spawn(move || match start_backend(&handle) {
        Ok(child) => {
          *handle.state::<RProcess>().0.lock().unwrap() = Some(child);
        }
        Err(message) => {
          log::error!("{message}");
          if let Some(window) = handle.get_webview_window("main") {
            show_error(&window, &message);
          }
        }
      });

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
