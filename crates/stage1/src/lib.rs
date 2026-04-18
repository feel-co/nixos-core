use std::{
  collections::HashMap,
  env,
  ffi::CString,
  fs::{self, File, OpenOptions, Permissions},
  io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
  os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
  path::{Path, PathBuf},
  process::Command,
  thread,
  time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nix::{
  libc,
  mount::{MsFlags, mount},
  sys::stat::{Mode, SFlag, makedev, mknod},
  unistd::{chdir, chroot, execv, getpid},
};

#[derive(Debug, Default)]
struct Stage1Config {
  target_root:          PathBuf,
  extra_utils:          Option<PathBuf>,
  kernel_modules:       Vec<String>,
  resume_device:        Option<String>,
  resume_devices:       Vec<String>,
  fs_info:              Option<PathBuf>,
  pre_fail_commands:    Option<PathBuf>,
  pre_device_commands:  Option<PathBuf>,
  post_device_commands: Option<PathBuf>,
  post_resume_commands: Option<PathBuf>,
  post_mount_commands:  Option<PathBuf>,
  early_mount_script:   Option<PathBuf>,
  udev_rules:           Option<PathBuf>,
  link_units:           Option<PathBuf>,
  check_journaling_fs:  bool,
  set_host_id:          Option<String>,
  distro_name:          String,
}

#[derive(Debug, Default)]
struct KernelCmdline {
  root:          Option<String>,
  init:          Option<String>,
  console:       Vec<String>,
  shell_on_fail: bool,
  debug1:        bool,
  debug:         bool,
  trace:         bool,
  panic_on_fail: bool,
  no_modprobe:   bool,
  copy_to_ram:   bool,
  persistence:   Option<String>,
  resume:        Option<String>,
  boot_gfx_mode: Option<String>,
  quiet:         bool,
  params:        HashMap<String, Option<String>>,
}

#[derive(Debug, Clone)]
struct FsInfo {
  device:     String,
  mountpoint: PathBuf,
  fstype:     String,
  options:    Vec<String>,
}

impl KernelCmdline {
  fn parse() -> Result<Self> {
    let content = fs::read_to_string("/proc/cmdline")
      .context("Failed to read /proc/cmdline")?;

    let mut cmdline = Self::default();

    for token in content.split_whitespace() {
      let mut parts = token.splitn(2, '=');
      let key = parts.next().unwrap_or("");
      let value = parts.next().map(String::from);

      match key {
        "root" => cmdline.root = value,
        "init" => cmdline.init = value,
        "console" => {
          if let Some(v) = value {
            cmdline.console.push(v);
          }
        },
        "boot.shell_on_fail" => {
          cmdline.shell_on_fail =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.debug1" => {
          cmdline.debug1 =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.debug" => {
          cmdline.debug =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.trace" => {
          cmdline.trace =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.panic_on_fail" => {
          cmdline.panic_on_fail =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.no_modprobe" => {
          cmdline.no_modprobe =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.copytoram" => {
          cmdline.copy_to_ram =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.persistence" => cmdline.persistence = value,
        "resume" => cmdline.resume = value,
        "boot.gfx_mode" => cmdline.boot_gfx_mode = value,
        "quiet" => {
          cmdline.quiet =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        _ => {
          cmdline.params.insert(key.to_string(), value);
        },
      }
    }

    Ok(cmdline)
  }

  fn get(&self, key: &str) -> Option<&String> {
    self.params.get(key).and_then(|v| v.as_ref())
  }
}

impl Stage1Config {
  fn from_env() -> Self {
    Self {
      target_root:          env::var("targetRoot")
        .map_or_else(|_| PathBuf::from("/mnt-root"), PathBuf::from),
      extra_utils:          env::var("extraUtils").ok().map(PathBuf::from),
      kernel_modules:       env::var("kernelModules")
        .map(|mods| mods.split_whitespace().map(String::from).collect())
        .unwrap_or_default(),
      resume_device:        env::var("resumeDevice").ok(),
      resume_devices:       env::var("resumeDevices")
        .map(|devs| devs.split_whitespace().map(String::from).collect())
        .unwrap_or_default(),
      fs_info:              env::var("fsInfo").ok().map(PathBuf::from),
      pre_fail_commands:    env::var("preFailCommands").ok().map(PathBuf::from),
      pre_device_commands:  env::var("preDeviceCommands")
        .ok()
        .map(PathBuf::from),
      post_device_commands: env::var("postDeviceCommands")
        .ok()
        .map(PathBuf::from),
      post_resume_commands: env::var("postResumeCommands")
        .ok()
        .map(PathBuf::from),
      post_mount_commands:  env::var("postMountCommands")
        .ok()
        .map(PathBuf::from),
      early_mount_script:   env::var("earlyMountScript")
        .ok()
        .map(PathBuf::from),
      udev_rules:           env::var("udevRules").ok().map(PathBuf::from),
      link_units:           env::var("linkUnits").ok().map(PathBuf::from),
      check_journaling_fs:  env::var("checkJournalingFS")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true),
      set_host_id:          env::var("HOST_ID").ok(),
      distro_name:          env::var("distroName")
        .unwrap_or_else(|_| "NixOS".to_string()),
    }
  }
}

fn log_message(msg: &str, to_kmsg: bool) {
  eprintln!("stage-1-init: {msg}");

  if to_kmsg
    && let Ok(mut file) = OpenOptions::new().write(true).open("/dev/kmsg")
  {
    let _ = writeln!(file, "stage-1-init: {msg}");
  }
}

fn setup_environment(extra_utils: Option<&Path>) -> Result<()> {
  // Set PATH
  let path = if let Some(utils) = extra_utils {
    format!("{}/bin:{}/sbin", utils.display(), utils.display())
  } else {
    "/bin:/sbin:/usr/bin:/usr/sbin".to_string()
  };
  // SAFETY: single-threaded at this point; no other threads can observe the
  // environment change.
  unsafe {
    env::set_var("PATH", &path);
  }

  // Create /bin and /sbin symlinks if extra_utils is provided
  if let Some(utils) = extra_utils {
    let bin_dir = Path::new("/bin");
    let sbin_dir = Path::new("/sbin");

    if !bin_dir.exists() {
      let _ = fs::remove_file(bin_dir);
      symlink(utils.join("bin"), bin_dir)
        .context("Failed to create /bin symlink")?;
    }

    if !sbin_dir.exists() {
      let _ = fs::remove_file(sbin_dir);
      symlink(utils.join("sbin"), sbin_dir)
        .context("Failed to create /sbin symlink")?;
    }
  }

  Ok(())
}

fn create_directories() -> Result<()> {
  let dirs = [
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/tmp",
    "/mnt",
    "/mnt-root",
    "/var",
    "/var/log",
  ];

  for dir in &dirs {
    fs::create_dir_all(dir)
      .with_context(|| format!("Failed to create directory: {dir}"))?;
  }

  Ok(())
}

fn create_essential_devices() -> Result<()> {
  // Create /dev/console if it doesn't exist
  if !Path::new("/dev/console").exists() {
    mknod(
      "/dev/console",
      SFlag::S_IFCHR,
      Mode::from_bits_truncate(0o600),
      makedev(5, 1),
    )
    .context("Failed to create /dev/console")?;
  }

  // Create /dev/null if it doesn't exist
  if !Path::new("/dev/null").exists() {
    mknod(
      "/dev/null",
      SFlag::S_IFCHR,
      Mode::from_bits_truncate(0o666),
      makedev(1, 3),
    )
    .context("Failed to create /dev/null")?;
  }

  // Create /dev/kmsg if it doesn't exist
  if !Path::new("/dev/kmsg").exists() {
    mknod(
      "/dev/kmsg",
      SFlag::S_IFCHR,
      Mode::from_bits_truncate(0o600),
      makedev(1, 11),
    )
    .ok(); // Non-critical
  }

  Ok(())
}

fn create_essential_files() -> Result<()> {
  // Create empty /etc/fstab
  let fstab = Path::new("/etc/fstab");
  if !fstab.exists() {
    fs::write(fstab, "# Initial fstab\n")
      .context("Failed to create /etc/fstab")?;
  }

  // Create /etc/mtab as symlink to /proc/mounts
  let mtab = Path::new("/etc/mtab");
  if !mtab.exists() && !mtab.is_symlink() {
    let _ = fs::remove_file(mtab);
    symlink("/proc/mounts", mtab)
      .context("Failed to create /etc/mtab symlink")?;
  }

  // Create /var/log/messages for logging
  let log_file = Path::new("/var/log/messages");
  if !log_file.exists() {
    fs::write(log_file, "").context("Failed to create /var/log/messages")?;
  }

  Ok(())
}

fn mount_essential_filesystems() -> Result<()> {
  // Mount proc
  let proc_path = Path::new("/proc");
  if !is_mounted(proc_path) {
    fs::create_dir_all(proc_path)?;
    mount(
      Some("proc"),
      proc_path,
      Some("proc"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
      None::<&str>,
    )
    .context("Failed to mount /proc")?;
  }

  // Mount sysfs
  let sys_path = Path::new("/sys");
  if !is_mounted(sys_path) {
    fs::create_dir_all(sys_path)?;
    mount(
      Some("sysfs"),
      sys_path,
      Some("sysfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
      None::<&str>,
    )
    .context("Failed to mount /sys")?;
  }

  // Mount devtmpfs
  let dev_path = Path::new("/dev");
  if !is_mounted(dev_path) {
    mount(
      Some("devtmpfs"),
      dev_path,
      Some("devtmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
      Some("mode=0755"),
    )
    .context("Failed to mount devtmpfs")?;
  }

  // Mount devpts
  let devpts_path = Path::new("/dev/pts");
  if !is_mounted(devpts_path) {
    fs::create_dir_all(devpts_path)?;
    mount(
      Some("devpts"),
      devpts_path,
      Some("devpts"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
      Some("mode=0620,gid=5"),
    )
    .ok(); // Non-critical
  }

  // Mount tmpfs on /run
  let run_path = Path::new("/run");
  if !is_mounted(run_path) {
    mount(
      Some("tmpfs"),
      run_path,
      Some("tmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_STRICTATIME,
      Some("mode=0755"),
    )
    .ok(); // May already be mounted
  }

  // Mount tmpfs on /tmp
  let tmp_path = Path::new("/tmp");
  if !is_mounted(tmp_path) {
    mount(
      Some("tmpfs"),
      tmp_path,
      Some("tmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_STRICTATIME,
      Some("mode=1777"),
    )
    .ok();
  }

  Ok(())
}

fn is_mounted(path: &Path) -> bool {
  if let Ok(file) = File::open("/proc/mounts") {
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
      let parts: Vec<&str> = line.split_whitespace().collect();
      if parts.len() >= 2 && parts[1] == path.to_string_lossy().as_ref() {
        return true;
      }
    }
  }
  false
}

// Wait up to timeout_secs for device to appear; re-triggers udev periodically.
fn wait_for_device(device: &str, timeout_secs: u64) -> Result<()> {
  let device_path = Path::new(device);
  let start = Instant::now();
  let timeout = Duration::from_secs(timeout_secs);
  let mut last_retrigger = Instant::now();

  log_message(&format!("Waiting for device: {device}"), true);

  while start.elapsed() < timeout {
    if device_is_ready(device_path) {
      log_message(&format!("Device {device} is ready"), true);
      return Ok(());
    }

    // Re-trigger udev every 5 seconds so block events aren't missed.
    if last_retrigger.elapsed() >= Duration::from_secs(5) {
      let _ = Command::new("udevadm")
        .args(["trigger", "--subsystem-match=block", "--action=change"])
        .status();
      let _ = Command::new("udevadm")
        .args(["settle", "--timeout=3"])
        .status();
      last_retrigger = Instant::now();
    }

    thread::sleep(Duration::from_millis(100));
  }

  bail!("Failed to wait for root device")
}

fn device_is_ready(device_path: &Path) -> bool {
  if device_path.exists()
    && let Ok(metadata) = fs::metadata(device_path)
  {
    return metadata.file_type().is_block_device();
  }
  false
}

fn load_module(module: &str) -> Result<()> {
  log_message(&format!("Loading module: {module}"), true);

  let status = Command::new("modprobe")
    .arg("-q")
    .arg(module)
    .status()
    .context("Failed to run modprobe")?;

  if !status.success() {
    log_message(&format!("Warning: Failed to load module: {module}"), true);
  }

  Ok(())
}

fn load_kernel_modules(modules: &[String], no_modprobe: bool) -> Result<()> {
  if no_modprobe {
    log_message("Skipping module loading (boot.no_modprobe)", true);
    return Ok(());
  }

  for module in modules {
    load_module(module).ok();
  }

  Ok(())
}

fn setup_link_units(link_units: &Path) -> Result<()> {
  fs::create_dir_all("/etc/systemd")?;
  let dest = Path::new("/etc/systemd/network");
  if dest.is_symlink() || dest.exists() {
    fs::remove_file(dest)?;
  }
  symlink(link_units, dest)?;
  Ok(())
}

// Mirrors stage-1-init.sh line 280: `systemd-udevd --daemon`
// (systemd-udevd is a symlink to udevadm in extra-utils; there is no udevd
// binary)
fn start_udev(
  rules_path: Option<&Path>,
  extra_utils: Option<&Path>,
) -> Result<()> {
  log_message("Starting udevd...", true);

  if let Some(rules) = rules_path {
    let udev_rules_dir = Path::new("/etc/udev/rules.d");
    fs::create_dir_all(udev_rules_dir)?;

    if rules.is_dir() {
      for entry in fs::read_dir(rules)? {
        let entry = entry?;
        let dest = udev_rules_dir.join(entry.file_name());
        fs::copy(entry.path(), dest)?;
      }
    }
  }

  let udev_conf = Path::new("/etc/udev/udev.conf");
  if !udev_conf.exists() {
    fs::create_dir_all(udev_conf.parent().unwrap())?;
    fs::write(udev_conf, "udev_log=err\n")?;
  }

  let udevd = extra_utils
    .map(|u| u.join("bin/systemd-udevd"))
    .unwrap_or_else(|| PathBuf::from("systemd-udevd"));

  Command::new(&udevd)
    .arg("--daemon")
    .status()
    .map_err(|e| {
      log_message(&format!("systemd-udevd spawn failed: {e}"), true);
      e
    })
    .context("Failed to start systemd-udevd")?;

  log_message("udevd started", true);
  Ok(())
}

fn trigger_udev() -> Result<()> {
  log_message("Triggering udev events...", true);

  Command::new("udevadm")
    .arg("trigger")
    .arg("--action=add")
    .status()
    .context("Failed to trigger udev events")?;

  Ok(())
}

fn settle_udev() -> Result<()> {
  log_message("Waiting for udev to settle...", true);

  let status = Command::new("udevadm")
    .arg("settle")
    .arg("--timeout=30")
    .status()
    .context("Failed to settle udev")?;

  if !status.success() {
    log_message("Warning: udev settle timed out", true);
  }

  Ok(())
}

fn activate_lvm() -> Result<()> {
  log_message("Activating LVM volumes...", true);

  // Run vgchange to activate volume groups
  let status = Command::new("vgchange").arg("-ay").status();

  if let Ok(status) = status {
    if status.success() {
      log_message("LVM volumes activated", true);
    } else {
      log_message("No LVM volumes found or activation failed", true);
    }
  } else {
    log_message("vgchange not available, skipping LVM activation", true);
  }

  Ok(())
}

// Read the filesystem type for a block device from udev's property database.
fn udev_fs_type(device: &str) -> Option<String> {
  let meta = fs::metadata(device).ok()?;
  let rdev = meta.rdev();
  let major = ((rdev >> 8) & 0xFFF) | ((rdev >> 32) & !0xFFF);
  let minor = (rdev & 0xFF) | ((rdev >> 12) & !0xFF);
  let content =
    fs::read_to_string(format!("/run/udev/data/b{major}:{minor}")).ok()?;
  content.lines().find_map(|line| {
    let fstype = line.strip_prefix("E:ID_FS_TYPE=")?;
    if fstype.is_empty() {
      None
    } else {
      Some(fstype.to_string())
    }
  })
}

fn has_swap_signature(device: &str) -> bool {
  // Swap header magic ("SWAPSPACE2") sits at the last 10 bytes of page 0.
  let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
  let offset = page_size.saturating_sub(10);
  let Ok(mut f) = File::open(device) else {
    return false;
  };
  if f.seek(SeekFrom::Start(offset)).is_err() {
    return false;
  }
  let mut magic = [0u8; 10];
  f.read_exact(&mut magic).is_ok()
    && (&magic == b"SWAPSPACE2" || &magic == b"PAGESIZE\0\0")
}

/// Handle resume from hibernation
fn handle_resume(
  resume_device: Option<&str>,
  resume_devices: &[String],
) -> Result<()> {
  let mut resume_dev: Option<String> = None;

  // First check explicit resume device from cmdline
  if let Some(dev) = resume_device
    && Path::new(dev).exists()
  {
    resume_dev = Some(dev.to_string());
  }

  // If not found, check the configured resume devices
  if resume_dev.is_none() {
    for dev in resume_devices {
      if Path::new(dev).exists() && has_swap_signature(dev) {
        resume_dev = Some(dev.clone());
        break;
      }
    }
  }

  let resume_dev = if let Some(d) = resume_dev {
    d
  } else {
    log_message("No resume device found", true);
    return Ok(());
  };

  log_message(&format!("Attempting resume from: {resume_dev}"), true);

  // Try to resume
  let resume_path = Path::new(&resume_dev);
  let resume_result = if resume_path.exists() {
    // The kernel expects major:minor (decimal), not the device path.
    // Use stat to extract the raw device number and decompose it.
    fs::metadata(resume_path)
      .context("Failed to stat resume device")
      .and_then(|meta| {
        let rdev = meta.rdev();
        // Standard Linux major/minor extraction.
        let major = ((rdev >> 8) & 0xFFF) | ((rdev >> 32) & !0xFFF);
        let minor = (rdev & 0xFF) | ((rdev >> 12) & !0xFF);
        fs::write("/sys/power/resume", format!("{major}:{minor}"))
          .context("Failed to write to /sys/power/resume")
      })
  } else {
    Err(anyhow::anyhow!(
      "Resume device does not exist: {resume_dev}"
    ))
  };

  if let Err(e) = resume_result {
    log_message(
      &format!("Resume failed (this is normal if not resuming): {e}"),
      true,
    );
  } else {
    log_message("Resume completed", true);
  }

  Ok(())
}

fn parse_fs_info(path: &Path) -> Result<Vec<FsInfo>> {
  let mut fs_infos = Vec::new();

  if !path.exists() {
    return Ok(fs_infos);
  }

  let content =
    fs::read_to_string(path).context("Failed to read fsInfo file")?;

  // Format: 4 lines per entry - mountPoint, device, fsType, options
  // (comma-separated). This matches how nixpkgs' stage-1.nix writes the file.
  let mut lines = content.lines();
  loop {
    let mount_point = match lines.next() {
      Some(l) if !l.is_empty() => l,
      _ => break,
    };
    let device = match lines.next() {
      Some(l) => l,
      None => break,
    };
    let fstype = match lines.next() {
      Some(l) => l,
      None => break,
    };
    let options = match lines.next() {
      Some(l) => l,
      None => break,
    };

    fs_infos.push(FsInfo {
      device:     device.to_string(),
      mountpoint: PathBuf::from(mount_point),
      fstype:     fstype.to_string(),
      options:    if options.is_empty() {
        Vec::new()
      } else {
        options.split(',').map(String::from).collect()
      },
    });
  }

  Ok(fs_infos)
}

fn needs_fsck(fstype: &str, check_journaling: bool) -> bool {
  match fstype {
    // ext2 has no journal - always check it.
    "ext2" => true,
    // ext3/ext4 have journaling; checking is optional.
    "ext3" | "ext4" => check_journaling,
    // fat/ntfs always need checking.
    "vfat" | "msdos" | "ntfs" => true,
    // btrfs and xfs have their own dedicated check tools (btrfs check /
    // xfs_repair); generic fsck does not support them and must not be run
    // on them.
    "btrfs" | "xfs" => false,
    _ => false,
  }
}

fn run_fsck(device: &str, fstype: &str, _options: &[String]) -> Result<bool> {
  log_message(&format!("Checking {fstype} filesystem on {device}"), true);

  // Skip if device is a pseudo-device
  if device.starts_with("/dev/loop") || device.starts_with("/dev/zram") {
    return Ok(true);
  }

  let mut cmd = Command::new("fsck");
  cmd.arg("-a").arg("-T").arg(device);

  // Add filesystem type specific options
  match fstype {
    "ext2" | "ext3" | "ext4" => {
      cmd.arg("-C0"); // Show progress on stdout
    },
    _ => {},
  }

  let status = cmd.status().context("Failed to run fsck")?;

  // fsck exit codes: 0 = OK, 1 = errors corrected, 2 = system should be
  // rebooted
  match status.code() {
    Some(0 | 1) => Ok(true),
    Some(2) => {
      log_message("Filesystem errors corrected, reboot recommended", true);
      Ok(true)
    },
    Some(4) => {
      log_message("Filesystem errors left uncorrected", true);
      Ok(false)
    },
    Some(8) => {
      bail!("fsck: operational error");
    },
    Some(16) => {
      bail!("fsck: usage or syntax error");
    },
    Some(32) => {
      bail!("fsck: checking canceled by user request");
    },
    Some(128) => {
      bail!("fsck: shared library error");
    },
    _ => {
      log_message("fsck returned unknown exit code", true);
      Ok(true) // Continue anyway
    },
  }
}

fn mount_filesystem(fs_info: &FsInfo) -> Result<()> {
  log_message(
    &format!(
      "Mounting {} ({}) at {:?}",
      fs_info.device, fs_info.fstype, fs_info.mountpoint
    ),
    true,
  );

  // Create mountpoint
  fs::create_dir_all(&fs_info.mountpoint).with_context(|| {
    format!("Failed to create mountpoint: {:?}", fs_info.mountpoint)
  })?;

  // Handle special filesystem types
  match fs_info.fstype.as_str() {
    "zfs" | "bcachefs" => {
      // These are handled specially - they may already be mounted
      log_message(
        &format!("Skipping mount of {} (handled by kernel)", fs_info.fstype),
        true,
      );
      return Ok(());
    },
    "bind" => {
      // Bind mount
      mount(
        Some(fs_info.device.as_str()),
        &fs_info.mountpoint,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
      )
      .with_context(|| {
        format!(
          "Failed to bind mount {} to {:?}",
          fs_info.device, fs_info.mountpoint
        )
      })?;
    },
    "overlay" => {
      // Overlay mount - options are in the format:
      // lowerdir=...,upperdir=...,workdir=... Filter x- options (kernel
      // doesn't understand them) and ensure upperdir/workdir exist.
      let filtered_opts: Vec<&str> = fs_info
        .options
        .iter()
        .filter(|o| !o.starts_with("x-"))
        .map(std::string::String::as_str)
        .collect();
      for opt in &filtered_opts {
        for prefix in &["upperdir=", "workdir="] {
          if let Some(path) = opt.strip_prefix(prefix) {
            fs::create_dir_all(path).ok();
          }
        }
      }
      mount(
        Some("overlay"),
        &fs_info.mountpoint,
        Some("overlay"),
        MsFlags::empty(),
        Some(filtered_opts.join(",").as_str()),
      )
      .with_context(|| {
        format!("Failed to mount overlay at {:?}", fs_info.mountpoint)
      })?;
    },
    _ => {
      let (flags, opts_str) =
        parse_mount_options(fs_info.options.iter().map(String::as_str));
      mount(
        Some(fs_info.device.as_str()),
        &fs_info.mountpoint,
        Some(fs_info.fstype.as_str()),
        flags,
        opts_str.as_deref(),
      )
      .with_context(|| {
        format!(
          "Failed to mount {} ({}) at {:?}",
          fs_info.device, fs_info.fstype, fs_info.mountpoint
        )
      })?;
    },
  }

  Ok(())
}

fn mount_root(
  cmdline: &KernelCmdline,
  target_root: &Path,
  fs_infos: &[FsInfo],
) -> Result<()> {
  log_message("Mounting root filesystem...", true);

  // Prefer root= from cmdline; fall back to the "/" entry in fsInfo.
  let (root_device_owned, fsinfo_fstype): (String, Option<String>) =
    if let Some(r) = cmdline.root.as_ref() {
      (r.clone(), None)
    } else {
      let entry = fs_infos
        .iter()
        .find(|f| f.mountpoint == Path::new("/"))
        .context(
          "No root= parameter specified on kernel command line and no '/' \
           entry in fsInfo",
        )?;
      (entry.device.clone(), Some(entry.fstype.clone()))
    };
  let root_device = &root_device_owned;

  // Handle special root devices
  if root_device == "tmpfs" {
    // Root on tmpfs (e.g., for live systems)
    fs::create_dir_all(target_root)?;
    mount(
      Some("tmpfs"),
      target_root,
      Some("tmpfs"),
      MsFlags::empty(),
      Some("mode=0755"),
    )
    .context("Failed to mount tmpfs root")?;
    return Ok(());
  }

  if root_device.starts_with("/dev/nfs") || root_device.starts_with("nfs:") {
    // NFS root
    fs::create_dir_all(target_root)?;
    let nfs_opts = cmdline
      .get("rootflags")
      .map_or("nolock", std::string::String::as_str);
    mount(
      Some(root_device.as_str()),
      target_root,
      Some("nfs"),
      MsFlags::empty(),
      Some(nfs_opts),
    )
    .context("Failed to mount NFS root")?;
    return Ok(());
  }

  if root_device.starts_with("//") {
    // CIFS root
    fs::create_dir_all(target_root)?;
    let cifs_opts = cmdline
      .get("rootflags")
      .map_or("", std::string::String::as_str);
    mount(
      Some(root_device.as_str()),
      target_root,
      Some("cifs"),
      MsFlags::empty(),
      Some(cifs_opts),
    )
    .context("Failed to mount CIFS root")?;
    return Ok(());
  }

  // Resolve the device to a concrete path and wait for it to be ready.
  // by-label and by-uuid paths are udev-managed symlinks; resolve them
  // directly.
  let mount_device_owned: String = {
    let start = Instant::now();
    let timeout = Duration::from_secs(30);
    let mut last_retrigger = Instant::now();
    let mut resolved: Option<String> = None;

    while start.elapsed() < timeout {
      let candidate: Option<String> = if root_device
        .starts_with("/dev/disk/by-label/")
        || root_device.starts_with("/dev/disk/by-uuid/")
      {
        fs::canonicalize(root_device)
          .ok()
          .map(|p| p.to_string_lossy().into_owned())
      } else {
        Some(root_device.clone())
      };

      if let Some(dev) = candidate
        && device_is_ready(Path::new(&dev))
      {
        resolved = Some(dev);
        break;
      }

      // Periodically re-trigger block events so udev populates by-* symlinks.
      if last_retrigger.elapsed() >= Duration::from_secs(3) {
        let _ = Command::new("udevadm")
          .args(["trigger", "--subsystem-match=block", "--action=change"])
          .status();
        let _ = Command::new("udevadm")
          .args(["settle", "--timeout=3"])
          .status();
        last_retrigger = Instant::now();
      }

      thread::sleep(Duration::from_millis(100));
    }

    resolved.ok_or_else(|| {
      anyhow::anyhow!("Timed out waiting for root device: {root_device}")
    })?
  };
  let mount_device = mount_device_owned.as_str();
  log_message(&format!("Using root device: {mount_device}"), true);

  let fstype = cmdline
    .get("rootfstype")
    .cloned()
    .or_else(|| udev_fs_type(mount_device))
    .or(fsinfo_fstype)
    .unwrap_or_else(|| "auto".to_string());

  // Parse mount options
  let mut mount_opts: Vec<String> = cmdline
    .get("rootflags")
    .map(|s| s.split(',').map(String::from).collect())
    .unwrap_or_default();

  // Default to rw if not specified
  if !mount_opts.iter().any(|o| o == "ro" || o == "rw") {
    mount_opts.push("rw".to_string());
  }

  // Check and run fsck if needed
  if needs_fsck(&fstype, true) {
    run_fsck(mount_device, &fstype, &mount_opts).ok();
  }

  // Mount the root filesystem
  fs::create_dir_all(target_root)?;

  let (flags, opts_str) =
    parse_mount_options(mount_opts.iter().map(String::as_str));
  mount(
    Some(mount_device),
    target_root,
    Some(fstype.as_str()),
    flags,
    opts_str.as_deref(),
  )
  .with_context(|| {
    format!("Failed to mount root filesystem {mount_device} at {target_root:?}")
  })?;

  log_message(&format!("Root filesystem mounted at {target_root:?}"), true);

  Ok(())
}

fn mount_additional_filesystems(
  fs_infos: &[FsInfo],
  target_root: &Path,
) -> Result<()> {
  for fs_info in fs_infos {
    if fs_info.mountpoint == Path::new("/") {
      continue; // Skip root, already mounted
    }

    // Adjust mountpoint to be under target_root
    let adjusted_mountpoint = if fs_info.mountpoint.is_absolute() {
      target_root.join(
        fs_info
          .mountpoint
          .strip_prefix("/")
          .unwrap_or(&fs_info.mountpoint),
      )
    } else {
      target_root.join(&fs_info.mountpoint)
    };

    let mut adjusted_fs_info = fs_info.clone();
    adjusted_fs_info.mountpoint = adjusted_mountpoint;

    // For overlay mounts, rewrite lowerdir/upperdir/workdir to be under
    // target_root.
    if fs_info.fstype == "overlay" {
      let target_root_str = target_root.to_string_lossy();
      adjusted_fs_info.options = fs_info
        .options
        .iter()
        .map(|opt| {
          for prefix in &["lowerdir=", "upperdir=", "workdir="] {
            if let Some(rest) = opt.strip_prefix(prefix) {
              // Rewrite each colon-separated path component.
              let adjusted = rest
                .split(':')
                .map(|p| {
                  if p.starts_with('/') {
                    format!("{target_root_str}{p}")
                  } else {
                    p.to_string()
                  }
                })
                .collect::<Vec<_>>()
                .join(":");
              return format!("{prefix}{adjusted}");
            }
          }
          opt.clone()
        })
        .collect();
    }

    if let Err(e) = mount_filesystem(&adjusted_fs_info) {
      log_message(
        &format!(
          "Warning: failed to mount {:?}: {:#}",
          adjusted_fs_info.mountpoint, e
        ),
        true,
      );
    }
  }

  Ok(())
}

fn copy_iso_to_ram(cmdline: &KernelCmdline, target_root: &Path) -> Result<()> {
  if !cmdline.copy_to_ram {
    return Ok(());
  }

  log_message("Copying ISO to RAM...", true);

  let iso_source = cmdline
    .get("iso_source")
    .map_or("/run/iso", std::string::String::as_str);

  let iso_dest = target_root.join("iso");
  fs::create_dir_all(&iso_dest)?;

  match copy_dir_recursive(&PathBuf::from(iso_source), &iso_dest) {
    Ok(()) => log_message("ISO copied to RAM", true),
    Err(e) => {
      log_message(&format!("Warning: Failed to copy ISO to RAM: {e}"), true);
    },
  }

  Ok(())
}

fn handle_persistence(
  cmdline: &KernelCmdline,
  target_root: &Path,
) -> Result<()> {
  let persist_opt = match &cmdline.persistence {
    Some(p) => p.clone(),
    None => return Ok(()),
  };

  log_message(&format!("Setting up persistence: {persist_opt}"), true);

  let (device, path) = if persist_opt.contains(':') {
    let mut parts = persist_opt.splitn(2, ':');
    let dev = parts.next().unwrap().to_string();
    let p = parts.next().unwrap().to_string();
    (Some(dev), p)
  } else {
    (None, persist_opt)
  };

  if let Some(dev) = device {
    wait_for_device(&dev, 10).ok();

    let persist_mount = Path::new("/run/persistence");
    fs::create_dir_all(persist_mount)?;

    mount(
      Some(dev.as_str()),
      persist_mount,
      Some("auto"),
      MsFlags::empty(),
      None::<&str>,
    )
    .context("Failed to mount persistence device")?;

    let persist_source =
      persist_mount.join(path.strip_prefix('/').unwrap_or(&path));
    if persist_source.exists() {
      log_message(
        &format!("Bind-mounting persistence from {persist_source:?}"),
        true,
      );
      mount(
        Some(&persist_source),
        target_root,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
      )
      .context("Failed to bind-mount persistence")?;
    } else {
      log_message(
        &format!("Warning: persistence path {persist_source:?} not found"),
        true,
      );
    }
  }

  Ok(())
}

fn move_path(src: &Path, dst: &Path) -> Result<()> {
  if fs::rename(src, dst).is_ok() {
    return Ok(());
  }
  // rename failed (likely EXDEV across filesystems); fall back to copy + delete
  if src.is_dir() {
    copy_dir_recursive(src, dst)?;
    fs::remove_dir_all(src)
      .with_context(|| format!("Failed to remove {src:?} after copy"))?;
  } else {
    fs::copy(src, dst)
      .with_context(|| format!("Failed to copy {src:?} to {dst:?}"))?;
    fs::remove_file(src)
      .with_context(|| format!("Failed to remove {src:?} after copy"))?;
  }
  Ok(())
}

// Handle NIXOS_LUSTRATE: move old root aside and restore selected entries.
fn handle_lustrate(target_root: &Path) -> Result<()> {
  let lustrate_file = target_root.join("nixos-lustrate");

  if !lustrate_file.exists() {
    return Ok(());
  }

  log_message("Handling NIXOS_LUSTRATE...", true);

  let content = fs::read_to_string(&lustrate_file)?;

  let backup_dir = target_root.join("old-root");
  fs::create_dir_all(&backup_dir)?;

  for entry in fs::read_dir(target_root)? {
    let entry = entry?;
    let name = entry.file_name();
    let name_str = name.to_string_lossy();

    if name_str.starts_with("nix")
      || name_str.starts_with("boot")
      || name_str == "old-root"
    {
      continue;
    }

    let dest = backup_dir.join(&name);
    if let Err(e) = move_path(&entry.path(), &dest) {
      log_message(
        &format!("Warning: move failed for {:?}: {}", entry.path(), e),
        true,
      );
    }
  }

  // Restore entries listed in the lustrate file (mirrors original bash read
  // loop)
  for keeper in content.lines() {
    let keeper = keeper.trim();
    if keeper.is_empty() || keeper.starts_with('#') {
      continue;
    }
    let stripped = keeper.strip_prefix('/').unwrap_or(keeper);
    let src = backup_dir.join(stripped);
    let dst = target_root.join(stripped);
    if src.exists() {
      if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
      }
      if let Err(e) = copy_dir_recursive(&src, &dst) {
        log_message(&format!("Warning: failed to restore {src:?}: {e}"), true);
      }
    }
  }

  fs::remove_file(&lustrate_file)?;
  log_message("Lustrate complete", true);

  Ok(())
}

// Recursively copy src's contents into dest.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
  fs::create_dir_all(dest)
    .with_context(|| format!("Failed to create directory {dest:?}"))?;

  // Restrict directory permissions: secret directories must not be
  // world-readable.
  fs::set_permissions(dest, Permissions::from_mode(0o700))
    .with_context(|| format!("Failed to set permissions on {dest:?}"))?;

  for entry in fs::read_dir(src)
    .with_context(|| format!("Failed to read directory {src:?}"))?
  {
    let entry = entry?;
    let src_path = entry.path();
    let dest_path = dest.join(entry.file_name());

    let file_type = entry.file_type()?;
    if file_type.is_dir() {
      copy_dir_recursive(&src_path, &dest_path)?;
    } else {
      // Includes regular files and symlinks (copy resolves symlinks).
      fs::copy(&src_path, &dest_path).with_context(|| {
        format!("Failed to copy {src_path:?} to {dest_path:?}")
      })?;
    }
  }
  Ok(())
}

fn copy_initrd_secrets(target_root: &Path) -> Result<()> {
  let secrets_dir = Path::new("/secrets");

  if !secrets_dir.exists() {
    return Ok(());
  }

  log_message("Copying initrd secrets...", true);

  for entry in fs::read_dir(secrets_dir)? {
    let entry = entry?;
    let source = entry.path();

    // Get relative path and construct destination
    let rel_path = source.strip_prefix(secrets_dir)?;
    let dest = target_root.join(rel_path);

    let file_type = entry.file_type()?;
    if file_type.is_dir() {
      // Recursively copy the directory tree.
      copy_dir_recursive(&source, &dest).with_context(|| {
        format!("Failed to copy secret directory {source:?} to {dest:?}")
      })?;
    } else {
      // Create parent directory with restricted permissions and copy the file.
      if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
        // Secret parent directories must not be world-readable.
        fs::set_permissions(parent, Permissions::from_mode(0o700))?;
      }
      fs::copy(&source, &dest).with_context(|| {
        format!("Failed to copy secret from {source:?} to {dest:?}")
      })?;

      // Set secure permissions on copied file.
      let mut perms = fs::metadata(&dest)?.permissions();
      perms.set_mode(0o600);
      fs::set_permissions(&dest, perms)?;
    }
  }

  Ok(())
}

fn kill_remaining_processes() -> Result<()> {
  log_message("Killing remaining processes...", true);

  // Signal all processes except ourselves and storage daemons to terminate
  // Storage daemons are distinguished by an @ in front of their command line:
  // https://www.freedesktop.org/wiki/Software/systemd/RootStorageDaemons/
  let my_pid = getpid().as_raw();

  // First try SIGTERM
  for entry in fs::read_dir("/proc")? {
    let entry = entry?;
    if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>()
      && pid > 1
      && pid != my_pid
      && !is_storage_daemon(pid)
    {
      unsafe {
        libc::kill(pid, libc::SIGTERM);
      }
    }
  }

  // Wait a bit
  thread::sleep(Duration::from_millis(500));

  // Then SIGKILL remaining processes (still excluding storage daemons)
  for entry in fs::read_dir("/proc")? {
    let entry = entry?;
    if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>()
      && pid > 1
      && pid != my_pid
      && !is_storage_daemon(pid)
    {
      unsafe {
        libc::kill(pid, libc::SIGKILL);
      }
    }
  }

  Ok(())
}

fn is_storage_daemon(pid: i32) -> bool {
  let cmdline_path = format!("/proc/{pid}/cmdline");
  if let Ok(content) = fs::read_to_string(&cmdline_path) {
    // cmdline is null-separated; check if first argument starts with @
    if let Some(first_arg) = content.split('\0').next() {
      return first_arg.starts_with('@');
    }
  }
  false
}

fn start_recovery_shell(reason: &str, pre_fail_commands: Option<&Path>) -> ! {
  eprintln!("\n");
  eprintln!("========================================");
  eprintln!("Boot failed: {reason}");
  eprintln!("Starting recovery shell...");
  eprintln!("========================================");
  eprintln!("\n");

  // Run pre-fail commands if available
  if let Some(commands) = pre_fail_commands
    && commands.exists()
  {
    let _ = Command::new(commands).status();
  }

  // Try to spawn a shell
  let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

  let _ = Command::new(&shell).env("PS1", "(initrd) $ ").status();

  // If shell exits or fails, halt
  eprintln!("Shell exited. Halting...");
  loop {
    unsafe {
      libc::sync();
      libc::reboot(libc::RB_HALT_SYSTEM);
    }
  }
}

fn fail(reason: &str, cmdline: &KernelCmdline, config: &Stage1Config) -> ! {
  log_message(&format!("FAIL: {reason}"), true);

  if cmdline.shell_on_fail || cmdline.debug1 {
    start_recovery_shell(reason, config.pre_fail_commands.as_deref());
  } else if cmdline.panic_on_fail {
    // Trigger kernel panic
    let _ = fs::write("/proc/sysrq-trigger", "c");
    std::process::exit(1);
  } else {
    // Reboot
    eprintln!("Boot failed. Rebooting in 10 seconds...");
    thread::sleep(Duration::from_secs(10));
    unsafe {
      libc::reboot(libc::RB_AUTOBOOT);
    }
    std::process::exit(1);
  }
}

fn switch_root(
  target_root: &Path,
  init: &str,
  cmdline: &KernelCmdline,
) -> Result<()> {
  log_message(&format!("Switching root to {target_root:?}"), true);

  // Check that init exists
  let init_path = target_root.join(init.trim_start_matches('/'));
  if !init_path.exists() {
    bail!("Init program not found: {init}");
  }

  // Move essential mounts into the new root. The early mount script may have
  // already mounted these at target_root/{dev,proc,sys,run}; MS_MOVE on an
  // already-occupied destination returns EBUSY, which we swallow.
  let essential_mounts = ["/dev", "/proc", "/sys", "/run"];
  for mountpoint in &essential_mounts {
    let old_path = Path::new(mountpoint);
    let new_path = target_root.join(mountpoint.trim_start_matches('/'));
    fs::create_dir_all(&new_path).ok();
    mount(
      Some(old_path),
      &new_path,
      None::<&str>,
      MsFlags::MS_MOVE,
      None::<&str>,
    )
    .ok();
  }

  // Change to the new root
  chdir(target_root)
    .with_context(|| format!("Failed to chdir to {target_root:?}"))?;

  // The initrd root is a ramfs; pivot_root(2) does not work on ramfs.
  // Move the new root filesystem onto / with MS_MOVE, then chroot into it.
  mount(
    Some("."),
    Path::new("/"),
    None::<&str>,
    MsFlags::MS_MOVE,
    None::<&str>,
  )
  .context("Failed to move new root to /")?;

  chroot(Path::new(".")).context("Failed to chroot into new root")?;

  chdir("/").context("Failed to chdir to new /")?;

  // Set up console
  setup_console(cmdline)?;

  for fd in 3..1024 {
    unsafe {
      libc::close(fd);
    }
  }

  log_message(&format!("Executing init: {init}"), true);

  let argv = [CString::new(init).context("Invalid init path")?];

  execv(&argv[0], &argv)
    .with_context(|| format!("Failed to exec init: {init}"))?;

  bail!("execv returned unexpectedly")
}

fn setup_console(cmdline: &KernelCmdline) -> Result<()> {
  unsafe {
    libc::close(0);
    libc::close(1);
    libc::close(2);
  }

  // Build a /dev/<device> path from the first console= entry. Strip any
  // baud/mode suffix (e.g. "ttyS0,115200n8" -> "ttyS0").
  let console_path: String = cmdline.console.first().map_or_else(
    || "/dev/console".to_string(),
    |s| {
      let dev = s.split(',').next().unwrap_or(s);
      if dev.starts_with('/') {
        dev.to_string()
      } else {
        format!("/dev/{dev}")
      }
    },
  );

  // SAFETY: CString ensures null termination required by libc::open.
  let c_console = CString::new(console_path.as_str())
    .context("console path contains a null byte")?;
  let mut fd =
    unsafe { libc::open(c_console.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };

  if fd < 0 {
    fd = unsafe {
      libc::open(c"/dev/console".as_ptr(), libc::O_RDWR | libc::O_NOCTTY)
    };
    if fd < 0 {
      bail!("Failed to open console");
    }
  }

  unsafe {
    libc::dup2(fd, 0);
    libc::dup2(fd, 1);
    libc::dup2(fd, 2);
    if fd > 2 {
      libc::close(fd);
    }
  }

  Ok(())
}

fn run_hook_script(script: Option<&Path>, description: &str) -> Result<()> {
  if let Some(script) = script
    && script.exists()
    && fs::metadata(script).map(|m| m.len() > 0).unwrap_or(false)
  {
    log_message(&format!("Running {description}: {script:?}"), true);

    // Run via sh since hook files are plain text without a shebang.
    let status = Command::new("sh")
      .arg(script)
      .status()
      .with_context(|| format!("Failed to run {description}"))?;

    if !status.success() {
      log_message(
        &format!(
          "Warning: {} exited with status: {:?}",
          description,
          status.code()
        ),
        true,
      );
    }
  }
  Ok(())
}

fn set_host_id(hex_id: Option<&str>) -> Result<()> {
  let Some(hex) = hex_id else {
    return Ok(());
  };
  let hex = hex.trim();
  if hex.len() != 8 {
    bail!("HOST_ID must be an 8-character hex string, got: '{hex}'");
  }
  let n = u32::from_str_radix(hex, 16)
    .with_context(|| format!("Invalid HOST_ID hex string: '{hex}'"))?;
  let bytes = n.to_ne_bytes();
  log_message(&format!("Setting host ID: {hex}"), true);
  fs::write("/etc/hostid", bytes).context("Failed to write /etc/hostid")?;
  Ok(())
}

fn parse_args(args: &[String]) -> Stage1Config {
  let mut config = Stage1Config::from_env();

  // Parse CLI args (override env vars)
  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "--target-root" | "-t" => {
        if i + 1 < args.len() {
          config.target_root = PathBuf::from(&args[i + 1]);
          i += 1;
        }
      },
      "--extra-utils" => {
        if i + 1 < args.len() {
          config.extra_utils = Some(PathBuf::from(&args[i + 1]));
          i += 1;
        }
      },
      "--distro-name" => {
        if i + 1 < args.len() {
          config.distro_name = args[i + 1].clone();
          i += 1;
        }
      },
      _ => {},
    }
    i += 1;
  }

  config
}

fn parse_shell_args(line: &str) -> Vec<String> {
  let mut args = Vec::new();
  let mut current = String::new();
  let mut chars = line.chars().peekable();

  while let Some(c) = chars.next() {
    match c {
      '\'' => {
        // Single-quoted string: take everything until closing '
        for c2 in chars.by_ref() {
          if c2 == '\'' {
            break;
          }
          current.push(c2);
        }
      },
      '"' => {
        for c2 in chars.by_ref() {
          if c2 == '"' {
            break;
          }
          current.push(c2);
        }
      },
      ' ' | '\t' => {
        if !current.is_empty() {
          args.push(std::mem::take(&mut current));
        }
      },
      _ => current.push(c),
    }
  }
  if !current.is_empty() {
    args.push(current);
  }
  args
}

// Parse mount options into MsFlags bits and a leftover data string.
// Options that map to kernel flags are consumed; the rest are rejoined for the
// data parameter. Accepts any iterator of option strings (e.g. a comma-split
// &str or a pre-split Vec<String> slice).
fn parse_mount_options<'a>(
  opts: impl Iterator<Item = &'a str>,
) -> (MsFlags, Option<String>) {
  let mut flags = MsFlags::empty();
  let mut data: Vec<&'a str> = Vec::new();

  for opt in opts {
    match opt {
      "ro" => flags |= MsFlags::MS_RDONLY,
      "rw" | "exec" | "async" | "" => {},
      "nosuid" => flags |= MsFlags::MS_NOSUID,
      "nodev" => flags |= MsFlags::MS_NODEV,
      "noexec" => flags |= MsFlags::MS_NOEXEC,
      "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
      "noatime" => flags |= MsFlags::MS_NOATIME,
      "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
      "relatime" => flags |= MsFlags::MS_RELATIME,
      "strictatime" => flags |= MsFlags::MS_STRICTATIME,
      "lazytime" => flags |= MsFlags::MS_LAZYTIME,
      "bind" => flags |= MsFlags::MS_BIND,
      "remount" => flags |= MsFlags::MS_REMOUNT,
      "silent" => flags |= MsFlags::MS_SILENT,
      "dirsync" => flags |= MsFlags::MS_DIRSYNC,
      o if o.starts_with("x-") => {},
      _ => data.push(opt),
    }
  }

  let data_str = if data.is_empty() {
    None
  } else {
    Some(data.join(","))
  };
  (flags, data_str)
}

/// Main entry point for stage 1 initialization
pub fn run(args: &[String]) -> Result<()> {
  // Mount /proc early so KernelCmdline::parse() can read /proc/cmdline.
  // The rest of the essential mounts happen later in
  // mount_essential_filesystems().
  {
    let proc_path = Path::new("/proc");
    let _ = fs::create_dir_all(proc_path);
    if !is_mounted(proc_path) {
      let _ = mount(
        Some("proc"),
        proc_path,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None::<&str>,
      );
    }
  }

  let config = parse_args(args);
  let cmdline =
    KernelCmdline::parse().context("Failed to parse kernel command line")?;

  setup_console(&cmdline).ok();

  let greeting = format!("<<< {} Stage 1 >>>", config.distro_name);
  println!("{greeting}");
  log_message(&greeting, true);

  setup_environment(config.extra_utils.as_deref())
    .context("Failed to set up environment")?;
  create_directories().context("Failed to create directories")?;
  create_essential_devices().context("Failed to create essential devices")?;
  mount_essential_filesystems()
    .context("Failed to mount essential filesystems")?;
  create_essential_files().context("Failed to create essential files")?;

  run_hook_script(config.pre_device_commands.as_deref(), "pre-device commands")
    .context("Pre-device commands failed")?;

  load_kernel_modules(&config.kernel_modules, cmdline.no_modprobe)
    .context("Failed to load kernel modules")?;

  if let Some(link_units) = config.link_units.as_deref() {
    setup_link_units(link_units)
      .context("Failed to set up systemd link units")?;
  }

  start_udev(config.udev_rules.as_deref(), config.extra_utils.as_deref())
    .context("Failed to start udev")?;
  trigger_udev().context("Failed to trigger udev events")?;
  settle_udev().context("Failed to settle udev")?;
  activate_lvm().context("Failed to activate LVM")?;

  run_hook_script(
    config.post_device_commands.as_deref(),
    "post-device commands",
  )
  .context("Post-device commands failed")?;

  handle_resume(
    cmdline
      .resume
      .as_deref()
      .or(config.resume_device.as_deref()),
    &config.resume_devices,
  )
  .context("Failed to handle resume")?;

  run_hook_script(
    config.post_resume_commands.as_deref(),
    "post-resume commands",
  )
  .context("Post-resume commands failed")?;

  // Parse filesystem info early so mount_root can fall back to it when root= is
  // absent.
  let fs_infos: Vec<FsInfo> = if let Some(fs_info_path) = &config.fs_info {
    parse_fs_info(fs_info_path).context("Failed to parse filesystem info")?
  } else {
    Vec::new()
  };

  if let Err(e) = mount_root(&cmdline, &config.target_root, &fs_infos) {
    fail(
      &format!("Failed to mount root filesystem: {e}"),
      &cmdline,
      &config,
    );
  }

  for fs_info in &fs_infos {
    if needs_fsck(&fs_info.fstype, config.check_journaling_fs) {
      run_fsck(&fs_info.device, &fs_info.fstype, &fs_info.options).ok();
    }
  }

  if !fs_infos.is_empty() {
    mount_additional_filesystems(&fs_infos, &config.target_root)
      .context("Failed to mount additional filesystems")?;
  }

  if let Some(script) = &config.early_mount_script
    && script.exists()
  {
    log_message(&format!("Running early mount script: {script:?}"), true);

    let script_content = fs::read_to_string(script)
      .context("Failed to read early mount script")?;

    for line in script_content.lines() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      let Some(rest) = line.strip_prefix("specialMount ") else {
        continue;
      };
      let args = parse_shell_args(rest);
      if args.len() < 4 {
        log_message(
          &format!("Warning: malformed specialMount line: {line}"),
          true,
        );
        continue;
      }
      let device = &args[0];
      let mountpoint = &args[1];
      let options = &args[2];
      let fstype = &args[3];

      let target = config
        .target_root
        .join(mountpoint.strip_prefix('/').unwrap_or(mountpoint));
      fs::create_dir_all(&target)?;

      let (flags, data) = parse_mount_options(options.split(','));
      let opts = data.as_deref();
      if let Err(e) = mount(
        Some(device.as_str()),
        &target,
        Some(fstype.as_str()),
        flags,
        opts,
      ) {
        fail(
          &format!(
            "Early mount script: failed to mount {device} at {target:?}: {e}"
          ),
          &cmdline,
          &config,
        );
      }
    }
  }

  run_hook_script(config.post_mount_commands.as_deref(), "post-mount commands")
    .context("Post-mount commands failed")?;

  copy_iso_to_ram(&cmdline, &config.target_root)
    .context("Failed to copy ISO to RAM")?;
  handle_persistence(&cmdline, &config.target_root)
    .context("Failed to handle persistence")?;
  handle_lustrate(&config.target_root).context("Failed to handle lustrate")?;
  copy_initrd_secrets(&config.target_root)
    .context("Failed to copy initrd secrets")?;
  set_host_id(config.set_host_id.as_deref())
    .context("Failed to set host ID")?;

  log_message("Stopping udevd...", true);
  let udevadm = config
    .extra_utils
    .as_deref()
    .map(|u| u.join("bin/udevadm"))
    .unwrap_or_else(|| PathBuf::from("udevadm"));
  let _ = Command::new(&udevadm).args(["control", "--exit"]).status();

  kill_remaining_processes().context("Failed to kill remaining processes")?;

  let init = cmdline
    .init
    .as_deref()
    .unwrap_or("/nix/var/nix/profiles/system/sw/bin/init");

  if let Err(e) = switch_root(&config.target_root, init, &cmdline) {
    fail(&format!("Failed to switch root: {e}"), &cmdline, &config);
  }

  bail!("switch_root returned unexpectedly")
}
