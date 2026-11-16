// WMF Control (Windows Mac Fan Control) - by melinoe-ux (https://github.com/melinoe-ux) - MIT License
use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Port I/O and physical memory access need the driver, which needs admin rights.
        // WMF_NO_ADMIN=1 builds an as-invoker exe (useful for --demo testing).
        let level = if std::env::var_os("WMF_NO_ADMIN").is_some() {
            ExecutionLevel::AsInvoker
        } else {
            ExecutionLevel::RequireAdministrator
        };
        embed_manifest(new_manifest("WMFControl").requested_execution_level(level))
            .expect("unable to embed manifest");

        // Copy driver/inpoutx64.dll to target directory so the binary works out of the box
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            if let Some(dest_dir) = std::path::Path::new(&out_dir).ancestors().nth(3) {
                let src = std::path::Path::new("driver").join("inpoutx64.dll");
                if src.exists() {
                    let dest = dest_dir.join("inpoutx64.dll");
                    let _ = std::fs::copy(&src, &dest);
                }
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=driver/inpoutx64.dll");
    println!("cargo:rerun-if-env-changed=WMF_NO_ADMIN");
}
