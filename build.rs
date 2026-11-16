use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/tdm.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let resource = out_dir.join("tdm_icon.o");
    let rc = Path::new("assets/app.rc");

    match Command::new("windres")
        .arg("-O")
        .arg("coff")
        .arg("-i")
        .arg(rc)
        .arg("-o")
        .arg(&resource)
        .status()
    {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg={}", resource.display());
        }
        _ => {
            println!("cargo:warning=windres not found; the .exe will use the default icon");
        }
    }
}
