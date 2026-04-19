//! Bash-compatible stage 2 initialization, mirroring stage-2-init.sh.

use std::{
  fs,
  os::{
    fd::{IntoRawFd, RawFd},
    unix::{
      fs::{chown, symlink},
      process::CommandExt,
    },
  },
  path::Path,
  process::{Command, Stdio},
};

use activation_common::{get_mount_options, is_mounted};
use anyhow::{Context, Result, bail};
use log::info;
use nix::{
  mount::{MsFlags, mount},
  unistd::{Group, getpid},
};

use crate::{
  cli::Args,
  common::{create_directories, log_message, set_permissions},
};

/// Run the bash-compatible stage 2 initialization sequence.
pub fn run(args: &Args) -> Result<()> {
  let log_dest = setup_logging().context("Failed to set up logging")?;

  println!("{}", args.greeting);
  log_message(
    log_dest.as_deref(),
    &format!("stage-2-init: {}", args.greeting),
  );

  setup_path(args).context("Failed to set up PATH")?;

  // Check if we're in systemd stage 1 (initrd already set things up)
  let in_systemd_stage1 =
    std::env::var("IN_NIXOS_SYSTEMD_STAGE1").unwrap_or_default() == "true";

  if in_systemd_stage1 {
    log_message(
      log_dest.as_deref(),
      "stage-2-init: running in systemd stage 1 mode, skipping early mount \
       setup",
    );
  } else {
    if has_kernel_cmdline_flag("boot.debugtrace") {
      // Shell equivalent is `set -x`: dump each activation/post-boot command
      // to stderr. Bump log level so every `info!`/`debug!` reaches kmsg.
      log::set_max_level(log::LevelFilter::Trace);
      log_message(
        log_dest.as_deref(),
        "stage-2-init: boot.debugtrace set; tracing enabled",
      );
    }

    // Stage 2 may be entered directly (no stage 1) - e.g. on systems where
    // the bootloader invokes /sbin/init or the initrd handoff skipped the
    // remount. stage-2-init.sh does the same remount unconditionally outside
    // of containers (systemd / nspawn exports $container to mark those).
    if std::env::var_os("container").is_none() {
      remount_root_rw(&log_dest)
        .context("Failed to remount / rw")?;
    }

    // Upstream `source @earlyMountScript@` path. If the caller supplied the
    // nix-generated script we sourced every specialFileSystems entry via it;
    // otherwise fall back to the tiny hardcoded set, logging that the caller
    // probably wants to pass --early-mount-script so that cgroup2 / efivarfs /
    // etc. don't go missing.
    if !Path::new("/proc").join("1").exists() {
      match args.early_mount_script.as_deref() {
        Some(script) => {
          run_early_mount_script(script, &log_dest)
            .context("Failed to run early mount script")?;
        },
        None => {
          log_message(
            log_dest.as_deref(),
            "stage-2-init: warning: no --early-mount-script; only mounting \
             the hardcoded /proc, /dev, /sys, /dev/pts, /dev/shm set. Any \
             additional boot.specialFileSystems entries will be absent.",
          );
          mount_special_filesystems(&log_dest)
            .context("Failed to mount special filesystems")?;
        },
      }
    }
  }

  // Non-fatal: 9p-mounted read-only stores in VMs reject chown/bind-mount.
  if let Err(e) = setup_nix_store(args, &log_dest) {
    log::warn!("Failed to set up /nix/store: {e} (continuing)");
  }

  create_required_directories(&log_dest)
    .context("Failed to create required directories")?;

  // Match stage-2-init.sh: the useHostResolvConf branch runs only when not in
  // the systemd-stage-1 path (initrd systemd already wires resolv.conf).
  if args.use_host_resolv_conf && !in_systemd_stage1 {
    setup_resolv_conf(&log_dest).context("Failed to set up resolv.conf")?;
  }

  if !args.system_config.exists() {
    bail!(
      "System configuration path does not exist: {}",
      args.system_config.display()
    );
  }

  // Capture fds 1 and 2 from here on so activation, post-boot commands, and
  // anything they spawn also land in /dev/kmsg (or /run/log) - matches the
  // `exec > >(tee ...) 2>&1` block in stage-2-init.sh:110-122. The shell
  // skips this in the systemd-stage-1 path; we do the same.
  let saved = if in_systemd_stage1 {
    None
  } else {
    capture_stdio(&log_dest)
      .context("Failed to set up stdio capture")?
  };

  run_activation_script(&args.system_config, &log_dest)
    .context("Activation script failed")?;

  record_boot_config(&args.system_config, &log_dest)
    .context("Failed to record boot configuration")?;

  if let Some(ref post_boot) = args.post_boot_commands {
    run_post_boot_commands(post_boot, &log_dest)
      .context("Post-boot commands failed")?;
  }

  log_message(
    log_dest.as_deref(),
    "stage-2-init: activation complete, starting systemd",
  );

  // Restore console fds before the exec so systemd inherits the terminal,
  // not the tee pipe. Matches stage-2-init.sh's `exec 1>&$logOutFd` restore.
  if let Some(saved) = saved {
    restore_stdio(saved);
  }

  Ok(())
}

#[derive(Debug, Clone, Copy)]
struct SavedStdio {
  stdout: RawFd,
  stderr: RawFd,
}

/// Redirect fds 1 and 2 through a `tee`-like helper. Returns the saved
/// originals (dup'd fds) so the caller can restore before the systemd
/// handoff.
fn capture_stdio(
  log_dest: &Option<std::path::PathBuf>,
) -> Result<Option<SavedStdio>> {
  // SAFETY: dup, dup2, close on standard fds in a single-threaded context.
  let saved_stdout = unsafe { libc::dup(1) };
  let saved_stderr = unsafe { libc::dup(2) };
  if saved_stdout < 0 || saved_stderr < 0 {
    bail!(
      "dup of stdout/stderr failed: {}",
      std::io::Error::last_os_error()
    );
  }

  let kmsg_writable = fs::OpenOptions::new()
    .append(true)
    .open("/dev/kmsg")
    .is_ok();

  // Keep a copy of every line on the original stdout so the console still
  // sees output, while additionally writing each line to kmsg or log file.
  let shell_cmd = if kmsg_writable {
    format!(
      "exec tee -i /proc/self/fd/{saved_stdout} | while IFS= read -r line; \
       do if [ -n \"$line\" ]; then printf '<7>stage-2-init: %s\\n' \"$line\" \
       > /dev/kmsg; fi; done"
    )
  } else {
    format!(
      "mkdir -p /run/log && exec tee -i /proc/self/fd/{saved_stdout} \
       /run/log/stage-2-init.log"
    )
  };

  let mut child = Command::new("/bin/sh")
    .arg("-c")
    .arg(&shell_cmd)
    .stdin(Stdio::piped())
    .spawn()
    .context("Failed to spawn stdio capture helper")?;

  let pipe_fd = child
    .stdin
    .take()
    .ok_or_else(|| anyhow::anyhow!("capture child missing stdin"))?
    .into_raw_fd();

  // SAFETY: redirect fds 1 and 2 onto the pipe, then close the original.
  let err = unsafe {
    if libc::dup2(pipe_fd, 1) < 0 || libc::dup2(pipe_fd, 2) < 0 {
      Some(std::io::Error::last_os_error())
    } else {
      None
    }
  };
  unsafe { libc::close(pipe_fd) };
  if let Some(e) = err {
    bail!("dup2 onto stdio failed: {e}");
  }

  // Leak the Child handle: it must stay alive until its stdin EOFs, which
  // happens once our fds 1 and 2 get closed (on exec or exit).
  std::mem::forget(child);

  log_message(
    log_dest.as_deref(),
    &format!(
      "stage-2-init: capturing stdio via {}",
      if kmsg_writable { "/dev/kmsg" } else { "/run/log/stage-2-init.log" }
    ),
  );

  Ok(Some(SavedStdio {
    stdout: saved_stdout,
    stderr: saved_stderr,
  }))
}

fn restore_stdio(saved: SavedStdio) {
  // SAFETY: dup2/close on standard fds in a single-threaded context.
  unsafe {
    libc::dup2(saved.stdout, 1);
    libc::dup2(saved.stderr, 2);
    libc::close(saved.stdout);
    libc::close(saved.stderr);
  }
}

fn setup_logging() -> Result<Option<std::path::PathBuf>> {
  if fs::create_dir_all("/run/log").is_ok() {
    return Ok(Some(std::path::PathBuf::from("/run/log/stage-2-init.log")));
  }
  Ok(None)
}

fn setup_path(args: &Args) -> Result<()> {
  info!("Setting PATH to: {}", args.path);
  // SAFETY: single-threaded at this point; no other threads can observe the
  // environment change.
  unsafe {
    std::env::set_var("PATH", &args.path);
  }
  Ok(())
}

fn run_early_mount_script(
  script: &Path,
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  log_message(
    log_dest.as_deref(),
    &format!(
      "stage-2-init: sourcing early mount script: {}",
      script.display()
    ),
  );

  // Inlines the specialMount helper that stage-2-init.sh defines before the
  // source, so the nix-generated script can be consumed verbatim.
  let wrapper = format!(
    r#"set -e
specialMount() {{
    local device="$1"
    local mountPoint="$2"
    local options="$3"
    local fsType="$4"
    if [ "${{IN_NIXOS_SYSTEMD_STAGE1:-}}" = "true" ] && [ "$mountPoint" = "/run" ]; then
        return
    fi
    install -m 0755 -d "$mountPoint"
    mount -n -t "$fsType" -o "$options" "$device" "$mountPoint"
}}
. {}
"#,
    shell_escape(&script.to_string_lossy()),
  );

  let status = Command::new("/bin/sh")
    .arg("-c")
    .arg(&wrapper)
    .status()
    .with_context(|| {
      format!("Failed to invoke /bin/sh to run {}", script.display())
    })?;

  if !status.success() {
    bail!(
      "early mount script {} exited with status {status}",
      script.display()
    );
  }
  Ok(())
}

fn shell_escape(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('\'');
  for c in s.chars() {
    if c == '\'' {
      out.push_str("'\\''");
    } else {
      out.push(c);
    }
  }
  out.push('\'');
  out
}

/// Whitespace-tokenized scan of /proc/cmdline for a bare flag. Matches the
/// shell's `for o in $(</proc/cmdline); do case $o in flag) ... esac done`
/// idiom: only the exact token counts, not prefixes or `key=value` matches.
fn has_kernel_cmdline_flag(flag: &str) -> bool {
  let Ok(cmdline) = fs::read_to_string("/proc/cmdline") else {
    return false;
  };
  cmdline.split_whitespace().any(|tok| tok == flag)
}

fn remount_root_rw(log_dest: &Option<std::path::PathBuf>) -> Result<()> {
  let root = Path::new("/");
  // /proc/mounts isn't available yet in the no-stage-1 path; skip the
  // already-rw check and let the remount be idempotent.
  log_message(
    log_dest.as_deref(),
    "stage-2-init: remounting / read-write",
  );
  mount(
    None::<&str>,
    root,
    None::<&str>,
    MsFlags::MS_REMOUNT,
    None::<&str>,
  )
  .context("mount -o remount,rw / failed")
}

fn mount_special_filesystems(
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  log_message(
    log_dest.as_deref(),
    "stage-2-init: mounting special filesystems",
  );

  if !is_mounted(Path::new("/proc")) {
    fs::create_dir_all("/proc")?;
    mount(
      Some("proc"),
      Path::new("/proc"),
      Some("proc"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
      None::<&str>,
    )
    .context("Failed to mount /proc")?;
  }

  if !is_mounted(Path::new("/dev")) {
    fs::create_dir_all("/dev")?;
    // devtmpfs must NOT have MS_NODEV: device nodes need to be accessible
    mount(
      Some("devtmpfs"),
      Path::new("/dev"),
      Some("devtmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
      None::<&str>,
    )
    .context("Failed to mount /dev")?;
  }

  if !is_mounted(Path::new("/sys")) {
    fs::create_dir_all("/sys")?;
    mount(
      Some("sysfs"),
      Path::new("/sys"),
      Some("sysfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
      None::<&str>,
    )
    .context("Failed to mount /sys")?;
  }

  let dev_pts = Path::new("/dev/pts");
  if !is_mounted(dev_pts) {
    fs::create_dir_all(dev_pts)?;
    mount(
      Some("devpts"),
      dev_pts,
      Some("devpts"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
      Some("mode=620,ptmxmode=0666"),
    )
    .context("Failed to mount /dev/pts")?;
  }

  let dev_shm = Path::new("/dev/shm");
  if !is_mounted(dev_shm) {
    fs::create_dir_all(dev_shm)?;
    mount(
      Some("tmpfs"),
      dev_shm,
      Some("tmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
      Some("mode=1777"),
    )
    .context("Failed to mount /dev/shm")?;
  }

  Ok(())
}

fn setup_nix_store(
  args: &Args,
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  let store_path = Path::new("/nix/store");

  if !store_path.exists() {
    log_message(
      log_dest.as_deref(),
      "stage-2-init: /nix/store does not exist, skipping setup",
    );
    return Ok(());
  }

  log_message(
    log_dest.as_deref(),
    "stage-2-init: setting up /nix/store permissions",
  );

  // Look up the nixbld group dynamically; fall back to GID 30000
  let nixbld_gid = Group::from_name("nixbld")
    .ok()
    .flatten()
    .map_or(30000, |g| g.gid.as_raw());

  // Non-fatal: the store may be on a read-only or 9p-mounted filesystem (e.g.
  // in VM tests) where chown/chmod are not supported.
  if let Err(e) = chown(store_path, Some(0u32), Some(nixbld_gid)) {
    log::warn!(
      "Failed to chown {}: {} (continuing)",
      store_path.display(),
      e
    );
  }

  if let Err(e) = set_permissions(store_path, 0o1775) {
    log::warn!(
      "Failed to chmod {}: {} (continuing)",
      store_path.display(),
      e
    );
  }

  // Apply mount options if /nix/store is a separate mount
  if is_mounted(store_path) {
    let desired_opts: Vec<String> = args
      .nix_store_mount_opts
      .split(',')
      .map(|s| s.trim().to_string())
      .filter(|s| !s.is_empty())
      .collect();

    apply_nix_store_mount_opts(store_path, &desired_opts, log_dest)
      .context("Failed to apply /nix/store mount options")?;
  }

  Ok(())
}

fn apply_nix_store_mount_opts(
  store_path: &Path,
  desired_opts: &[String],
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  let current_opts = get_mount_options(store_path)?;

  fn opt_to_flag(opt: &str) -> Option<MsFlags> {
    match opt {
      "nosuid" => Some(MsFlags::MS_NOSUID),
      "noexec" => Some(MsFlags::MS_NOEXEC),
      "nodev" => Some(MsFlags::MS_NODEV),
      "ro" | "rdonly" => Some(MsFlags::MS_RDONLY),
      "noatime" => Some(MsFlags::MS_NOATIME),
      "nodiratime" => Some(MsFlags::MS_NODIRATIME),
      _ => None,
    }
  }

  // Separate standard flags from filesystem-specific options
  let mut missing_flags = MsFlags::empty();
  let mut missing_data: Vec<&str> = Vec::new();

  for opt in desired_opts
    .iter()
    .filter(|opt| !current_opts.contains(opt))
  {
    if let Some(flag) = opt_to_flag(opt) {
      missing_flags |= flag;
    } else {
      missing_data.push(opt);
    }
  }

  if missing_flags.is_empty() && missing_data.is_empty() {
    return Ok(());
  }

  log_message(
    log_dest.as_deref(),
    &format!(
      "stage-2-init: applying mount options to /nix/store: \
       flags={missing_flags:?} data={missing_data:?}"
    ),
  );

  // Bind mount the store onto itself, then remount with options. In a
  // container /nix/store can have submounts (e.g. nix-daemon bind mounts,
  // per-profile binds) whose propagation must be preserved, so use rbind
  // there - matching stage-2-init.sh:93-97.
  let bind_flags = if std::env::var_os("container").is_some() {
    MsFlags::MS_BIND | MsFlags::MS_REC
  } else {
    MsFlags::MS_BIND
  };
  mount(
    Some(store_path),
    store_path,
    None::<&str>,
    bind_flags,
    None::<&str>,
  )
  .with_context(|| format!("Failed to bind mount {}", store_path.display()))?;

  let data_string = if missing_data.is_empty() {
    None
  } else {
    Some(missing_data.join(","))
  };

  mount(
    None::<&str>,
    store_path,
    None::<&str>,
    MsFlags::MS_REMOUNT | MsFlags::MS_BIND | missing_flags,
    data_string.as_deref(),
  )
  .with_context(|| format!("Failed to remount {}", store_path.display()))?;

  Ok(())
}

fn create_required_directories(
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  let dirs = ["/etc", "/etc/nixos", "/tmp", "/run/keys"];

  for dir in &dirs {
    log_message(
      log_dest.as_deref(),
      &format!("stage-2-init: creating directory {dir}"),
    );
    create_directories(&[dir])
      .with_context(|| format!("Failed to create directory: {dir}"))?;
  }

  set_permissions(Path::new("/tmp"), 0o1777).context("Failed to chmod /tmp")?;

  Ok(())
}

/// Register the host's resolv.conf with resolvconf, matching the upstream
/// `resolvconf -m 1000 -a host </etc/resolv.conf` invocation from
/// stage-2-init.sh. systemd-nspawn bind-mounts the host file at /etc/resolv.conf
/// inside the container; we feed that file as resolvconf's stdin.
fn setup_resolv_conf(log_dest: &Option<std::path::PathBuf>) -> Result<()> {
  let resolv_conf = Path::new("/etc/resolv.conf");

  if !resolv_conf.exists() {
    return Ok(());
  }

  let status = Command::new("resolvconf")
    .args(["-m", "1000", "-a", "host"])
    .stdin(fs::File::open(resolv_conf).with_context(|| {
      format!("Failed to open {} for resolvconf", resolv_conf.display())
    })?)
    .status();

  match status {
    Ok(s) if s.success() => {
      log_message(
        log_dest.as_deref(),
        "stage-2-init: registered host resolv.conf via resolvconf",
      );
    },
    Ok(s) => {
      log_message(
        log_dest.as_deref(),
        &format!("stage-2-init: warning: resolvconf exited with {s}"),
      );
    },
    Err(e) => {
      log_message(
        log_dest.as_deref(),
        &format!("stage-2-init: warning: failed to invoke resolvconf: {e}"),
      );
    },
  }

  Ok(())
}

fn run_activation_script(
  system_config: &Path,
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  let activate_script = system_config.join("activate");

  if !activate_script.exists() {
    bail!("Activation script not found: {}", activate_script.display());
  }

  log_message(
    log_dest.as_deref(),
    &format!(
      "stage-2-init: running activation script: {}",
      activate_script.display()
    ),
  );

  // SAFETY: single-threaded at this point; no other threads can observe the
  // environment change.
  unsafe {
    std::env::set_var("NIXOS_SYSTEM_CONFIG", system_config);
  }

  let status = Command::new(&activate_script).status().with_context(|| {
    format!(
      "Failed to execute activation script: {}",
      activate_script.display()
    )
  })?;

  if !status.success() {
    // Match original bash stage-2-init behavior: run activate and continue
    // regardless of exit code. Some snippets (e.g. specialfs remounts) may
    // exit non-zero on valid configurations.
    log::warn!(
      "Activation script exited with code {:?} (continuing)",
      status.code()
    );
  }

  Ok(())
}

fn record_boot_config(
  system_config: &Path,
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  let booted_system = Path::new("/run/booted-system");

  log_message(
    log_dest.as_deref(),
    &format!(
      "stage-2-init: recording boot configuration: {} -> {}",
      booted_system.display(),
      system_config.display()
    ),
  );

  if booted_system.exists() || booted_system.is_symlink() {
    fs::remove_file(booted_system).with_context(|| {
      format!("Failed to remove old {}", booted_system.display())
    })?;
  }

  symlink(system_config, booted_system).with_context(|| {
    format!(
      "Failed to create symlink: {} -> {}",
      booted_system.display(),
      system_config.display()
    )
  })?;

  Ok(())
}

fn run_post_boot_commands(
  commands_path: &Path,
  log_dest: &Option<std::path::PathBuf>,
) -> Result<()> {
  if !commands_path.exists() {
    return Ok(());
  }

  log_message(
    log_dest.as_deref(),
    &format!(
      "stage-2-init: running post-boot commands: {}",
      commands_path.display()
    ),
  );

  // Run via shell: the file is stored in the nix store which may be noexec,
  // and the file itself may not have the execute bit set (e.g. pkgs.writeText).
  let status = Command::new("/bin/sh")
    .arg(commands_path)
    .status()
    .with_context(|| {
      format!(
        "Failed to execute post-boot commands: {}",
        commands_path.display()
      )
    })?;

  if !status.success() {
    log_message(
      log_dest.as_deref(),
      &format!(
        "stage-2-init: warning: post-boot commands failed with exit code: {:?}",
        status.code()
      ),
    );
  }

  Ok(())
}

/// Hand off to systemd via execv.
pub fn exec_systemd(systemd_path: &Path, systemd_args: &[String]) -> ! {
  info!(
    "Exec-ing systemd: {} {:?}",
    systemd_path.display(),
    systemd_args
  );

  if getpid().as_raw() != 1 {
    log::warn!("Not running as PID 1, but continuing anyway");
  }

  for var in [
    "SYSTEM_CONFIG",
    "STAGE2_GREETING",
    "NIX_STORE_MOUNT_OPTS",
    "POST_BOOT_COMMANDS",
    "USE_HOST_RESOLV_CONF",
    "STAGE2_PATH",
    "SYSTEMD_EXECUTABLE",
    "EARLY_MOUNT_SCRIPT",
  ] {
    // SAFETY: single-threaded at this point; no other threads can observe the
    // environment change.
    unsafe {
      std::env::remove_var(var);
    }
  }

  // Exec systemd with the trailing argv that was passed to us - matches
  // `exec @systemdExecutable@ "$@"` in stage-2-init.sh.
  let err = Command::new(systemd_path).args(systemd_args).exec();

  eprintln!(
    "FATAL: Failed to exec systemd at {}: {}",
    systemd_path.display(),
    err
  );

  // emergency fallback
  let _ = Command::new("/bin/sh").exec();

  std::process::exit(1);
}
