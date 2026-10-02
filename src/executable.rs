//! Locate and re-execute Panoptes when Android's system linker launched it.

use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

pub fn current_exe() -> Result<PathBuf> {
    let image = std::env::current_exe()?;
    #[cfg(target_os = "android")]
    if is_linker(&image) {
        return android_program();
    }
    Ok(image)
}

pub fn command() -> Result<Command> {
    let image = std::env::current_exe()?;
    #[cfg(target_os = "android")]
    if is_linker(&image) {
        // Apps targeting recent Android versions cannot directly exec binaries
        // in writable app storage. Preserve the system-linker launch route,
        // with the real program as its first argument, even without LD_PRELOAD.
        let mut command = Command::new(image);
        command.arg(android_program()?);
        return Ok(command);
    }
    Ok(Command::new(image))
}

#[cfg(target_os = "android")]
fn is_linker(path: &std::path::Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("linker" | "linker64")
    )
}

#[cfg(target_os = "android")]
fn android_program() -> Result<PathBuf> {
    use anyhow::{Context, bail};
    let argv0 = std::env::args_os()
        .next()
        .context("missing Panoptes argv[0]")?;
    let path = PathBuf::from(argv0);
    if path.is_absolute() || path.components().count() > 1 {
        return path
            .canonicalize()
            .context("resolve Panoptes path from argv[0]");
    }
    // A bare argv[0] follows PATH lookup, rather than trusting an inherited
    // TERMUX_EXEC__PROC_SELF_EXE value that may belong to the parent process.
    if let Some(search) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&search) {
            let candidate = directory.join(&path);
            if candidate.is_file() {
                return candidate.canonicalize().context("resolve Panoptes on PATH");
            }
        }
    }
    bail!(
        "cannot locate Panoptes executable from argv[0]: {}",
        path.display()
    )
}
