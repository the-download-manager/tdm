//! TDM command-line entry point (console subsystem).
//!
//! Same engine as the GUI: `tdm-cli add|list|info|resume|remove|config|serve`.

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(tdm::cli::run())
}
