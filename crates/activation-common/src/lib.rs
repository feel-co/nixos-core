use std::{
  fs::File,
  io::{BufRead, BufReader},
  path::Path,
};

use anyhow::{Context, Result};

// Unescape octal sequences in /proc/mounts paths (e.g., \040 -> ' ').
fn unescape_mount_path(s: &str) -> String {
  let mut result = String::with_capacity(s.len());
  let mut chars = s.chars().peekable();
  while let Some(c) = chars.next() {
    if c == '\\' {
      let octal: String = chars.by_ref().take(3).collect();
      if let Ok(code) = u8::from_str_radix(&octal, 8) {
        result.push(code as char);
      } else {
        result.push('\\');
        result.push_str(&octal);
      }
    } else {
      result.push(c);
    }
  }
  result
}

/// Check if a filesystem is already mounted at the given path.
#[must_use]
pub fn is_mounted(path: &Path) -> bool {
  let Ok(file) = File::open("/proc/mounts") else {
    return false;
  };
  let reader = BufReader::new(file);
  let path_str = path.to_string_lossy();
  for line_result in reader.lines() {
    let Ok(line) = line_result else {
      continue;
    };
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 && unescape_mount_path(parts[1]) == path_str {
      return true;
    }
  }
  false
}

/// Get current mount options for a path using /proc/mounts.
pub fn get_mount_options(path: &Path) -> Result<Vec<String>> {
  let file =
    File::open("/proc/mounts").context("Failed to open /proc/mounts")?;
  let reader = BufReader::new(file);
  let path_str = path.to_string_lossy();

  for line_result in reader.lines() {
    let line = line_result.context("Failed to read /proc/mounts")?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 4 && unescape_mount_path(parts[1]) == path_str {
      return Ok(parts[3].split(',').map(str::to_owned).collect());
    }
  }

  anyhow::bail!("Mount point not found: {}", path.display())
}
