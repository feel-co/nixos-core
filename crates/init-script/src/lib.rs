use std::{
  fs::{self, File, Permissions, read_link, symlink_metadata},
  io::Write,
  os::unix::fs::{MetadataExt, PermissionsExt},
  path::Path,
};

use anyhow::{Context, Result};
use clap::Parser;

/// Create generic /sbin/init script
#[derive(Parser, Debug)]
#[command(name = "init-script-builder")]
#[command(about = "Create the generic init script and configuration list")]
struct Args {
  /// Path to the default system configuration
  default_config: String,
}

struct InitEntry {
  label: String,
  // Full path to the stage-2 init executable. Matches `$stage2=$path/init`
  // in upstream init-script-builder.sh; every generated stub exec's this.
  init:  String,
}

/// Build the /sbin/init boot menu and configuration list from installed
/// generations.
pub fn run(args: &[String]) -> Result<()> {
  let args = Args::parse_from(args);

  let boot_root = Path::new("/boot");
  let default_config = Path::new(&args.default_config);
  let system_dir = default_config
    .parent()
    .context("Failed to get parent of default config")?;

  // Create directories first so /boot exists before we stat it.
  fs::create_dir_all(boot_root)?;
  fs::create_dir_all("/sbin")?;

  // Warn if /boot is on a different filesystem. We must check AFTER
  // create_dir_all though.
  if !check_same_filesystem(boot_root, Path::new("/"))? {
    eprintln!("warning: /boot is on a different filesystem than /");
  }

  let default_init = format!("{}/init", args.default_config);
  let mut entries: Vec<InitEntry> = Vec::new();

  entries.push(InitEntry {
    label: "NixOS - Default".to_string(),
    init:  default_init.clone(),
  });

  add_specialisations(system_dir, &mut entries)?;
  add_generations(&mut entries)?;

  // Default and specialisation entries (no parseable generation number) come
  // first; then generation entries sorted newest-first. Without partitioning,
  // default/specialisation entries would get generation 0 from unwrap_or(0)
  // and sort to the end of the list.
  let gen_name_of = |init_path: &str| -> String {
    Path::new(init_path)
      .parent()
      .and_then(|p| p.file_name())
      .map(|n| n.to_string_lossy().into_owned())
      .unwrap_or_default()
  };

  let (non_gen, mut gen_entries): (Vec<InitEntry>, Vec<InitEntry>) = entries
    .into_iter()
    .partition(|e| parse_generation_number(&gen_name_of(&e.init)).is_none());

  gen_entries.sort_by(|a, b| {
    let a_num = parse_generation_number(&gen_name_of(&a.init)).unwrap_or(0);
    let b_num = parse_generation_number(&gen_name_of(&b.init)).unwrap_or(0);
    b_num.cmp(&a_num)
  });

  let mut entries = non_gen;
  entries.extend(gen_entries);

  write_init_files(&entries, &default_init)?;

  Ok(())
}

fn check_same_filesystem(path1: &Path, path2: &Path) -> Result<bool> {
  let meta1 = symlink_metadata(path1)?;
  let meta2 = symlink_metadata(path2)?;
  Ok(meta1.dev() == meta2.dev())
}

fn add_specialisations(
  system_dir: &Path,
  entries: &mut Vec<InitEntry>,
) -> Result<()> {
  let spec_dir = system_dir.join("specialisation");

  if !spec_dir.exists() {
    return Ok(());
  }

  for entry in fs::read_dir(&spec_dir)? {
    let entry = entry?;
    let name = entry.file_name();
    let name_str = name.to_string_lossy();

    let init_path = entry.path().join("init");
    if init_path.exists() {
      entries.push(InitEntry {
        label: format!("NixOS - {name_str}"),
        init:  init_path.to_string_lossy().to_string(),
      });
    }
  }

  Ok(())
}

fn add_generations(entries: &mut Vec<InitEntry>) -> Result<()> {
  let profiles_dir = Path::new("/nix/var/nix/profiles");

  if !profiles_dir.exists() {
    return Ok(());
  }

  for entry in fs::read_dir(profiles_dir)? {
    let entry = entry?;
    let name = entry.file_name();
    let name_str = name.to_string_lossy();

    if name_str.starts_with("system-")
      && name_str.ends_with("-link")
      && let Some(num) = parse_generation_number(&name_str)
    {
      let init_path = entry.path().join("init");
      if init_path.exists() {
        match build_generation_suffix(&entry.path()) {
          Ok(suffix) => {
            entries.push(InitEntry {
              label: format!("NixOS - Configuration {num}{suffix}"),
              init:  init_path.to_string_lossy().to_string(),
            });
          },
          Err(e) => {
            eprintln!(
              "warning: skipping generation {} ({}): {}",
              num,
              entry.path().display(),
              e
            );
          },
        }
      }
    }
  }

  Ok(())
}

fn parse_generation_number(name: &str) -> Option<u32> {
  if name.starts_with("system-") && name.ends_with("-link") {
    let start = name.find('-')? + 1;
    let end = name.rfind('-')?;
    name[start..end].parse::<u32>().ok()
  } else {
    None
  }
}

// Format a Unix timestamp as "YYYY-MM-DD HH:MM:SS" UTC without pulling in a
// date library. Howard Hinnant's civil_from_days handles negative timestamps
// and all proleptic Gregorian dates.
fn format_utc_datetime(ts: i64) -> String {
  let days = ts.div_euclid(86400);
  let tod = ts.rem_euclid(86400);
  let (h, m, s) = (tod / 3600, (tod / 60) % 60, tod % 60);
  let (y, mo, d) = civil_from_days(days);
  format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
  let z = z + 719468;
  let era = if z >= 0 { z } else { z - 146096 } / 146097;
  let doe = (z - era * 146097) as u64;
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
  let y = yoe as i64 + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
  let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
  let y = (y + i64::from(m <= 2)) as i32;
  (y, m, d)
}

fn build_generation_suffix(path: &Path) -> Result<String> {
  let mut suffix = String::new();

  let meta = symlink_metadata(path)?;
  let mtime = meta.mtime();
  suffix.push_str(&format!(" ({} - ", format_utc_datetime(mtime)));

  // Get kernel version
  let kernel_path = path.join("kernel");
  let kernel_ver = extract_kernel_version(&kernel_path)?;
  suffix.push_str(&kernel_ver);
  suffix.push(')');

  Ok(suffix)
}

fn extract_kernel_version(kernel_path: &Path) -> Result<String> {
  // Resolve symlink if needed
  let real_path = if kernel_path.is_symlink() {
    read_link(kernel_path)?
  } else {
    kernel_path.to_path_buf()
  };

  // Navigate to lib/modules to find version
  let modules_dir = real_path
    .parent()
    .and_then(|p| p.parent())
    .map(|p| p.join("lib/modules"))
    .filter(|p| p.exists())
    .ok_or_else(|| anyhow::anyhow!("Could not find modules directory"))?;

  // Collect all version directories, sort, and return the last, i.e., highest
  // version.
  let mut versions: Vec<String> = Vec::new();
  for entry in fs::read_dir(&modules_dir)? {
    let entry = entry?;
    let name = entry.file_name();
    if entry.metadata()?.is_dir() {
      versions.push(name.to_string_lossy().to_string());
    }
  }
  // Sort versions semantically by splitting on '.' and '-' and comparing
  // numeric parts. Lexicographic sort fails for kernel versions: "5.9.0" >
  // "5.10.0" lexicographically but 5.10.0 is the newer kernel. Parse numeric
  // components for correct ordering.
  versions.sort_by(|a, b| {
    let parse_parts = |s: &str| -> Vec<u64> {
      s.split(['.', '-'])
        .filter_map(|p| p.parse::<u64>().ok())
        .collect()
    };
    parse_parts(a).cmp(&parse_parts(b))
  });
  versions.into_iter().last().ok_or_else(|| {
    anyhow::anyhow!(
      "No kernel version directories found in {}",
      modules_dir.display()
    )
  })
}

const OTHER_CONFIGS_PATH: &str = "/boot/init-other-configurations-contents.txt";

fn write_init_files(entries: &[InitEntry], default_init: &str) -> Result<()> {
  // The default entry is the one whose init path matches $default_config/init.
  // Its label goes at the top of /sbin/init as a comment; every entry (default
  // included) is appended to $OTHER_CONFIGS_PATH as a runnable stub.
  let default_label = entries
    .iter()
    .find(|e| e.init == default_init)
    .map_or("NixOS - Default", |e| e.label.as_str());

  // Write /sbin/init atomically via a temp file.
  // Clean up the temp file if any step fails so we don't leave stale files.
  let sbin_init_tmp = "/sbin/init.tmp";
  let result = (|| -> Result<()> {
    let mut sbin_init = File::create(sbin_init_tmp)?;
    write_sbin_init(&mut sbin_init, default_label, default_init)?;
    drop(sbin_init);
    let perms = Permissions::from_mode(0o755);
    fs::set_permissions(sbin_init_tmp, perms)?;
    fs::rename(sbin_init_tmp, "/sbin/init")?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(sbin_init_tmp);
    return result;
  }

  // Write configs list atomically via a temp file.
  let configs_tmp = format!("{OTHER_CONFIGS_PATH}.tmp");
  let result = (|| -> Result<()> {
    let mut configs = File::create(&configs_tmp)?;
    write_configs_file(&mut configs, entries)?;
    drop(configs);
    fs::rename(&configs_tmp, OTHER_CONFIGS_PATH)?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(&configs_tmp);
    return result;
  }

  Ok(())
}

fn write_sbin_init(
  file: &mut File,
  default_label: &str,
  default_init: &str,
) -> Result<()> {
  writeln!(file, "#!/bin/sh")?;
  writeln!(file, "# {default_label}")?;
  writeln!(file, "# created by init-script-builder")?;
  writeln!(file, "exec {default_init}")?;
  writeln!(file, "# older configurations: {OTHER_CONFIGS_PATH}")?;
  Ok(())
}

/// Each entry becomes a standalone shell script body separated by a blank
/// line, matching init-script-builder.sh:44-57.
fn write_configs_file(file: &mut File, entries: &[InitEntry]) -> Result<()> {
  for entry in entries {
    writeln!(file, "#!/bin/sh")?;
    writeln!(file, "# {}", entry.label)?;
    writeln!(file, "# created by init-script-builder")?;
    writeln!(file, "exec {}", entry.init)?;
    writeln!(file)?;
  }
  Ok(())
}
