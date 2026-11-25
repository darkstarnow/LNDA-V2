#[cfg(not(windows))]
fn main() {
    eprintln!("Leonida shader preloading requires Windows.");
}

#[cfg(windows)]
#[path = "../PlayLauncher.rs"]
mod windows_launcher;

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_launcher::run(true) {
        eprintln!("Shader preload failed\n{error:#}");
        std::process::exit(1);
    }
}
