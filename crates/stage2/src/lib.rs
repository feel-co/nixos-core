//! NixOS stage 2 initialization library.
//!
//! This library provides bash-compatible stage 2 initialization with opt-in
//! improvements borrowed from nixos-init. It is designed to be called from
//! the nixos-core multicall binary.
use std::path::Path;

use anyhow::Context;
use clap::Parser;

pub mod bash_compat;
pub mod cli;
pub mod common;
pub mod nixos_init_compat;

/// Run stage 2 initialization (without systemd handoff).
pub fn run(args: &cli::Args) -> anyhow::Result<()> {
  #[cfg(feature = "bootspec")]
  if args.use_bootspec() {
    log::info!(
      "Bootspec support is enabled but purely informational unless additional \
       opt-in flags are set."
    );
    let _bootspec = load_bootspec(args.bootspec_path()).with_context(|| {
      format!(
        "Failed to load bootspec from {}",
        args.bootspec_path().display()
      )
    })?;
  }

  bash_compat::run(args).context("Core stage 2 initialization failed")?;

  if args.atomic_symlinks {
    log::info!("Using atomic symlinks for boot configuration");
    recreate_booted_system_atomically(&args.system_config)
      .context("Failed to atomically recreate /run/booted-system")?;
  }

  if args.create_current_system {
    nixos_init_compat::create_current_system(&args.system_config)
      .context("Failed to create /run/current-system")?;
  }

  if args.setup_fhs {
    if let Some(ref env_binary) = args.env_binary {
      nixos_init_compat::setup_usrbinenv("", env_binary)
        .context("Failed to set up /usr/bin/env")?;
    } else {
      log::warn!(
        "--setup-fhs requested but --env-binary not provided. Skipping \
         /usr/bin/env."
      );
    }

    if let Some(ref sh_binary) = args.sh_binary {
      nixos_init_compat::setup_binsh("", sh_binary)
        .context("Failed to set up /bin/sh")?;
    } else {
      log::warn!(
        "--setup-fhs requested but --sh-binary not provided. Skipping /bin/sh."
      );
    }
  }

  if args.setup_modprobe {
    nixos_init_compat::setup_modprobe(&args.modprobe_binary)
      .context("Failed to set up modprobe")?;
  }

  if args.setup_firmware {
    nixos_init_compat::setup_firmware_search_path(&args.firmware_path)
      .context("Failed to set up firmware search path")?;
  }

  Ok(())
}

/// Run stage 2 initialization and hand off to systemd.
pub fn run_and_handoff(args: &cli::Args) -> ! {
  run_and_handoff_inner(args);
}

/// Parse args from a slice and run stage 2 initialization with systemd handoff.
pub fn run_from_args_and_handoff(args: &[String]) -> ! {
  let parsed = cli::Args::parse_from(args);
  run_and_handoff_inner(&parsed);
}

fn run_and_handoff_inner(args: &cli::Args) -> ! {
  if let Err(e) = run(args) {
    // Write to kmsg as a safety net in case the console fd is not usable.
    let _ = std::fs::write("/dev/kmsg", format!("stage2: FATAL: {e:#}\n"));
    eprintln!("stage-2-init: FATAL: {e:#}");
    std::process::exit(1);
  }

  log::info!("stage-2-init: activation complete");

  // When called as prepare-root from the systemd initrd
  // (IN_NIXOS_SYSTEMD_STAGE1), we must exit cleanly. Systemd's
  // initrd-switch-root service handles the switch-root and the systemd exec.
  // Exec-ing systemd from within the chroot would be wrong.
  if std::env::var("IN_NIXOS_SYSTEMD_STAGE1").is_ok_and(|var| var == "true") {
    log::info!(
      "stage-2-init: systemd initrd path; returning to initrd-switch-root"
    );
    std::process::exit(0);
  }

  log::info!("stage-2-init: starting systemd");

  if args.use_systemctl_handoff() {
    #[cfg(feature = "systemd-integration")]
    {
      if let Err(e) = nixos_init_compat::systemctl_switch_root("/sysroot", None)
      {
        log::warn!(
          "systemctl switch-root failed ({}), falling back to raw exec",
          e
        );
        bash_compat::exec_systemd(&args.systemd_executable, &args.systemd_args);
      }
      log::error!("systemctl switch-root returned unexpectedly");
      std::process::exit(1);
    }

    #[cfg(not(feature = "systemd-integration"))]
    {
      log::warn!(
        "--use-systemctl-handoff requires the systemd-integration feature. \
         Falling back to raw exec."
      );
      bash_compat::exec_systemd(&args.systemd_executable, &args.systemd_args);
    }
  } else {
    bash_compat::exec_systemd(&args.systemd_executable, &args.systemd_args);
  }
}

fn recreate_booted_system_atomically(
  system_config: &Path,
) -> anyhow::Result<()> {
  let booted_system = Path::new("/run/booted-system");

  nixos_init_compat::atomic_symlink(system_config, booted_system).with_context(
    || {
      format!(
        "Failed to create atomic symlink: {} -> {}",
        booted_system.display(),
        system_config.display()
      )
    },
  )
}

#[cfg(feature = "bootspec")]
fn load_bootspec(path: &Path) -> anyhow::Result<serde_json::Value> {
  use std::fs;
  let raw = fs::read_to_string(path).with_context(|| {
    format!("Failed to read bootspec file: {}", path.display())
  })?;
  let value: serde_json::Value =
    serde_json::from_str(&raw).with_context(|| {
      format!("Failed to parse bootspec JSON: {}", path.display())
    })?;
  Ok(value)
}
