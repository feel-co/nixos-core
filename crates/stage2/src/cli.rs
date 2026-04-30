use std::path::PathBuf;

use clap::Parser;

/// NixOS stage 2 initialization.
///
/// By default, this tool behaves identically to the original stage-2-init.sh
/// bash script. Optional flags enable improvements borrowed from nixos-init for
/// advanced use cases.
#[derive(Parser, Debug)]
#[command(name = "stage2")]
#[command(about = "NixOS stage 2 initialization")]
pub struct Args {
  /// Path to system configuration
  #[arg(long, env = "SYSTEM_CONFIG")]
  pub system_config: PathBuf,

  /// Greeting message to print
  #[arg(
    long,
    env = "STAGE2_GREETING",
    default_value = "<<< NixOS Stage 2 >>>"
  )]
  pub greeting: String,

  /// Mount options for /nix/store (comma-separated)
  #[arg(long, env = "NIX_STORE_MOUNT_OPTS", default_value = "ro,nosuid,nodev")]
  pub nix_store_mount_opts: String,

  /// Path to systemd executable
  #[arg(
    long,
    env = "SYSTEMD_EXECUTABLE",
    default_value = "/run/current-system/systemd/lib/systemd/systemd"
  )]
  pub systemd_executable: PathBuf,

  /// Path to post-boot commands script (optional)
  #[arg(long, env = "POST_BOOT_COMMANDS")]
  pub post_boot_commands: Option<PathBuf>,

  /// Shell used to invoke --post-boot-commands. The upstream script hard-
  /// wires this to bash via `@shell@ @postBootCommands@`; the Nix module
  /// should point this at the same binary so bash-isms in the user's hook
  /// keep working. Defaults to /bin/sh which may be dash on minimal systems.
  #[arg(long, env = "POST_BOOT_SHELL", default_value = "/bin/sh")]
  pub post_boot_shell: PathBuf,

  /// Path to the nix-generated early mount script (equivalent to
  /// `@earlyMountScript@` in stage-2-init.sh). When set, this file is sourced
  /// with a `specialMount` shell helper in scope, so it can set up any
  /// special-filesystem entry declared in `boot.specialFileSystems`. When
  /// unset, stage 2 falls back to a small hardcoded set covering /proc, /dev,
  /// /sys, /dev/pts, /dev/shm.
  #[arg(long, env = "EARLY_MOUNT_SCRIPT")]
  pub early_mount_script: Option<PathBuf>,

  /// Use host resolv.conf
  #[arg(long, env = "USE_HOST_RESOLV_CONF")]
  pub use_host_resolv_conf: bool,

  /// PATH environment value to set
  #[arg(
    long,
    env = "STAGE2_PATH",
    default_value = "/run/current-system/sw/bin"
  )]
  pub path: String,

  /// Use retry-based atomic rename when replacing /run/booted-system.
  #[arg(long)]
  pub atomic_symlinks: bool,

  /// Set up /usr/bin/env and /bin/sh compatibility symlinks.
  /// Normally handled by activation scripts; enable this if running
  /// in an environment without activation script support.
  #[arg(long)]
  pub setup_fhs: bool,

  /// Configure /proc/sys/kernel/modprobe to point to the wrapped modprobe.
  /// Normally handled by the modprobe activation script.
  #[arg(long)]
  pub setup_modprobe: bool,

  /// Configure the kernel firmware search path.
  /// Normally handled by activation scripts or initrd setup.
  #[arg(long)]
  pub setup_firmware: bool,

  /// Create /run/current-system symlink in addition to /run/booted-system.
  /// This matches nixos-init behavior and ensures proper GC roots.
  #[arg(long)]
  pub create_current_system: bool,

  /// Fail if `$systemConfig/activate` is missing.
  /// By default, missing activation script is a warning so non-NixOS targets
  /// can still complete stage 2 and run post-boot hooks.
  #[arg(long, env = "STAGE2_STRICT_ACTIVATION")]
  pub strict_activation: bool,

  /// Path to the modprobe binary for --setup-modprobe
  #[arg(
    long,
    env = "MODPROBE_BINARY",
    default_value = "/run/current-system/sw/bin/modprobe"
  )]
  pub modprobe_binary: PathBuf,

  /// Path to the firmware directory for --setup-firmware
  #[arg(
    long,
    env = "FIRMWARE_PATH",
    default_value = "/run/current-system/firmware"
  )]
  pub firmware_path: PathBuf,

  /// Path to the /usr/bin/env target for --setup-fhs
  #[arg(long, env = "ENV_BINARY")]
  pub env_binary: Option<PathBuf>,

  /// Path to the /bin/sh target for --setup-fhs
  #[arg(long, env = "SH_BINARY")]
  pub sh_binary: Option<PathBuf>,

  /// Trailing arguments passed unchanged to systemd on handoff. Mirrors the
  /// `exec @systemdExecutable@ "$@"` at the end of stage-2-init.sh: kernel
  /// parameters like `systemd.unit=rescue.target` arrive here when the
  /// bootloader forwards argv to init.
  #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
  pub systemd_args: Vec<String>,

  /// Use bootspec JSON (boot.json) for configuration instead of env vars.
  /// Requires the `bootspec` feature to be enabled at compile time.
  #[cfg(feature = "bootspec")]
  #[arg(long = "use-bootspec")]
  pub use_bootspec_flag: bool,

  /// Path to boot.json when --use-bootspec is enabled
  #[cfg(feature = "bootspec")]
  #[arg(long, default_value = "/run/booted-system/boot.json")]
  pub bootspec_path_field: PathBuf,

  /// Use `systemctl switch-root` instead of raw execv for systemd handoff.
  /// Requires the `systemd-integration` feature to be enabled at compile time.
  #[cfg(feature = "systemd-integration")]
  #[arg(long = "use-systemctl-handoff")]
  pub use_systemctl_handoff_flag: bool,
}

#[cfg(not(feature = "bootspec"))]
impl Args {
  #[must_use]
  pub const fn use_bootspec(&self) -> bool {
    false
  }

  #[must_use]
  pub fn bootspec_path(&self) -> &std::path::Path {
    std::path::Path::new("")
  }
}

#[cfg(feature = "bootspec")]
impl Args {
  pub fn use_bootspec(&self) -> bool {
    self.use_bootspec_flag
  }

  pub fn bootspec_path(&self) -> &std::path::Path {
    &self.bootspec_path_field
  }
}

#[cfg(not(feature = "systemd-integration"))]
impl Args {
  #[must_use]
  pub const fn use_systemctl_handoff(&self) -> bool {
    false
  }
}

#[cfg(feature = "systemd-integration")]
impl Args {
  pub fn use_systemctl_handoff(&self) -> bool {
    self.use_systemctl_handoff_flag
  }
}
