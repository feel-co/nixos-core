use std::{
  fs::File,
  io::Write,
  os::unix::fs::PermissionsExt,
  path::Path,
  process::Command,
};

use anyhow::{Context, Result};
use log::info;

/// Create each directory in `dirs` if it does not already exist.
pub fn create_directories(dirs: &[&str]) -> Result<()> {
  for dir in dirs {
    if !Path::new(dir).exists() {
      info!("Creating directory: {dir}");
      std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create directory: {dir}"))?;
    }
  }
  Ok(())
}

/// Set Unix permissions on `path` to `mode` (e.g. `0o1777`).
pub fn set_permissions(path: &Path, mode: u32) -> Result<()> {
  let mut perms = std::fs::metadata(path)
    .with_context(|| format!("Failed to get metadata for {}", path.display()))?
    .permissions();
  perms.set_mode(mode);
  std::fs::set_permissions(path, perms)
    .with_context(|| format!("Failed to chmod {}", path.display()))?;
  Ok(())
}

/// Execute `script` via `/bin/sh`, returning an error if it exits non-zero or
/// is killed by a signal.
pub fn run_shell_script(script: &Path) -> Result<()> {
  let status =
    Command::new("/bin/sh")
      .arg(script)
      .status()
      .with_context(|| {
        format!("Failed to execute script: {}", script.display())
      })?;

  if !status.success() {
    #[cfg(unix)]
    {
      use std::os::unix::process::ExitStatusExt;
      if let Some(code) = status.code() {
        anyhow::bail!("Script failed with exit code: {code}");
      } else if let Some(signal) = status.signal() {
        anyhow::bail!("Script terminated by signal: {signal}");
      }
    }
    #[cfg(not(unix))]
    {
      if let Some(code) = status.code() {
        anyhow::bail!("Script failed with exit code: {}", code);
      }
    }
    anyhow::bail!("Script failed with unknown status");
  }

  Ok(())
}

/// Write `msg` to stderr and, if `log_path` is set, append it to that file.
pub fn log_message(log_path: Option<&Path>, msg: &str) {
  eprintln!("{msg}");
  if let Some(path) = log_path
    && let Ok(mut file) = File::options().create(true).append(true).open(path)
  {
    let _ = writeln!(file, "{msg}");
  }
}
