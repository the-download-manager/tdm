//! TDM GUI entry point.
//!
//! Built with the Windows GUI subsystem, so launching `tdm.exe` shows only the
//! window - no console behind it. Use `tdm-cli.exe` for command-line work.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = tdm::gui::run() {
        // no console to print to: leave a log next to the download database
        let dir = tdm::config::Config::home();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("gui-error.log"), format!("{err:#}\n"));
    }
}
