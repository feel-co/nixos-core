//! Optional improvements over the bash-compatible stage 2 path.

use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow};
use log::info;

/// Atomically create or replace a symlink.
pub fn atomic_symlink(
  original: impl AsRef<Path>,
  link: impl AsRef<Path>,
) -> Result<()> {
  let mut i = 0;

  let tmp_path = loop {
    let parent = link.as_ref().parent().ok_or_else(|| {
      anyhow!("Failed to determine parent of {:?}", link.as_ref())
    })?;

    if !parent.exists() {
      fs::create_dir_all(parent).with_context(|| {
        format!("Failed to create directory: {}", parent.display())
      })?;
    }

    let mut tmp_name = link
      .as_ref()
      .file_name()
      .ok_or_else(|| anyhow!("Failed to get file name of {:?}", link.as_ref()))?
      .to_os_string();
    tmp_name.push(format!(".tmp{i}"));
    let tmp_path = parent.join(&tmp_name);

    match std::os::unix::fs::symlink(&original, &tmp_path) {
      Ok(()) => break tmp_path,
      Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
        i += 1;
        if i > 100 {
          return Err(anyhow!(
            "Failed to find temporary symlink name after 100 attempts"
          ));
        }
      },
      Err(err) => {
        return Err(err).with_context(|| {
          format!(
            "Failed to create temporary symlink at {}",
            tmp_path.display()
          )
        });
      },
    }
  };

  fs::rename(&tmp_path, link.as_ref()).with_context(|| {
    format!(
      "Failed to rename {} to {}",
      tmp_path.display(),
      link.as_ref().display()
    )
  })?;

  Ok(())
}

/// Create `/run/current-system` symlink atomically.
pub fn create_current_system(toplevel: impl AsRef<Path>) -> Result<()> {
  let system_path = Path::new("/run/current-system");
  info!("Setting up /run/current-system...");
  atomic_symlink(&toplevel, system_path).with_context(|| {
    format!(
      "Failed to create /run/current-system symlink to {}",
      toplevel.as_ref().display()
    )
  })
}

/// Set up `/usr/bin/env` symlink.
pub fn setup_usrbinenv(
  prefix: &str,
  env_binary: impl AsRef<Path>,
) -> Result<()> {
  // /usr/bin/env is load-bearing for NixOS and many tools expect it.
  let usrbin_path = Path::new(prefix).join("usr/bin");
  info!("Setting up /usr/bin/env...");
  fs::create_dir_all(&usrbin_path).context("Failed to create /usr/bin")?;
  atomic_symlink(env_binary, usrbin_path.join("env"))
    .context("Failed to symlink /usr/bin/env")
}

/// Set up `/bin/sh` symlink.
pub fn setup_binsh(prefix: &str, sh_binary: impl AsRef<Path>) -> Result<()> {
  // /bin/sh is essential for the libc system() call.
  let binsh_path = Path::new(prefix).join("bin/sh");
  info!("Setting up /bin/sh...");
  fs::create_dir_all(binsh_path.parent().unwrap())
    .context("Failed to create /bin")?;
  atomic_symlink(sh_binary, binsh_path).context("Failed to symlink /bin/sh")
}

/// Configure the kernel modprobe path.
///
/// Writes to `/proc/sys/kernel/modprobe` so the kernel can find the wrapped
/// modprobe binary.
pub fn setup_modprobe(modprobe_binary: impl AsRef<Path>) -> Result<()> {
  const MODPROBE_PATH: &str = "/proc/sys/kernel/modprobe";
  info!("Setting up modprobe...");

  if Path::new(MODPROBE_PATH).exists() {
    fs::write(
      MODPROBE_PATH,
      modprobe_binary.as_ref().as_os_str().as_encoded_bytes(),
    )
    .with_context(|| {
      format!(
        "Failed to populate modprobe path with {}",
        modprobe_binary.as_ref().display()
      )
    })?;
  } else {
    info!("{MODPROBE_PATH} doesn't exist. Not populating it...");
  }

  Ok(())
}

/// Configure the kernel firmware search path.
pub fn setup_firmware_search_path(firmware: impl AsRef<Path>) -> Result<()> {
  const FIRMWARE_SEARCH_PATH: &str =
    "/sys/module/firmware_class/parameters/path";
  info!("Setting up firmware search path...");

  if Path::new(FIRMWARE_SEARCH_PATH).exists() {
    fs::write(
      FIRMWARE_SEARCH_PATH,
      firmware.as_ref().as_os_str().as_encoded_bytes(),
    )
    .with_context(|| {
      format!(
        "Failed to populate firmware search path with {}",
        firmware.as_ref().display()
      )
    })?;
  } else {
    info!("{FIRMWARE_SEARCH_PATH} doesn't exist. Not populating it...");
  }

  Ok(())
}

#[cfg(feature = "systemd-integration")]
/// Hand off to systemd using `systemctl switch-root`.
///
/// This is the nixos-init approach. It ensures a clean transition and lets
/// systemd handle mount propagation and service state correctly.
pub fn systemctl_switch_root(sysroot: &str, init: Option<&Path>) -> Result<()> {
  use std::process::Command;

  info!("Switching root to {sysroot} via systemctl...");

  let mut cmd = Command::new("systemctl");
  cmd.arg("--no-block").arg("switch-root").arg(sysroot);

  if let Some(init) = init {
    info!("Using init {}.", init.display());
    cmd.arg(init);
  } else {
    // Passing `""` is NOT equivalent to omitting the INIT arg:
    // `systemctl switch-root ROOT ""` tells systemd "exec this literal empty
    // path" and fails. Omitting the arg makes systemd auto-detect.
    info!("Using built-in systemd as init.");
  }

  let output = cmd.output().context(
    "Failed to run systemctl switch-root. Most likely the binary is not on \
     PATH",
  )?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("systemctl switch-root exited unsuccessfully: {stderr}");
  }

  Ok(())
}
