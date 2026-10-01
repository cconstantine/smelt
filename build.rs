//! Sets `SMELT_BUILD_ID`: a hash of everything that goes into the app, the
//! same for the server binary and the web bundle when both are built from
//! one tree. A tab compares its own with the server's to notice it's
//! running an older bundle than the server (SME-43).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// What the build id covers. A directory is walked; cargo also watches
/// one recursively for `rerun-if-changed`.
const INPUTS: [&str; 4] = ["src", "assets", "Cargo.toml", "Cargo.lock"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    for input in INPUTS {
        println!("cargo:rerun-if-changed={input}");
        collect(Path::new(input), &mut files)?;
    }
    // Sorted, so the id doesn't depend on the order a directory lists in.
    files.sort();
    let mut hasher = Sha256::new();
    for file in &files {
        let path = file.to_string_lossy();
        let contents = std::fs::read(file)?;
        // Lengths first, so no two different trees hash the same bytes.
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((contents.len() as u64).to_le_bytes());
        hasher.update(&contents);
    }
    let id: String = hasher.finalize()[..8].iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=SMELT_BUILD_ID={id}");
    Ok(())
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect(&entry?.path(), files)?;
        }
    } else if path.is_file() {
        files.push(path.to_path_buf());
    }
    Ok(())
}
