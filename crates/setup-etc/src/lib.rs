use std::{
  collections::HashSet,
  fs::{self, File, OpenOptions, Permissions},
  io::Write,
  os::unix::fs::{PermissionsExt, chown, symlink},
  path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;

/// Update /etc from the current NixOS configuration
#[derive(Parser, Debug)]
#[command(name = "setup-etc")]
#[command(about = "Atomically apply /etc files from /etc/static")]
struct Args {
  /// Path to the /nix/store/..-etc tree
  etc_dir: String,
}

const ETC_STATIC: &str = "/etc/static";

/// Apply /etc files from the given nix store path, updating /etc/static and all
/// derived symlinks.
pub fn run(args: &[String]) -> Result<()> {
  let args = Args::parse_from(args);
  let etc = PathBuf::from(&args.etc_dir);

  // Step 1: Atomically update the /etc/static symlink.
  atomic_symlink(&etc, Path::new(ETC_STATIC))
    .context("Failed to update /etc/static symlink")?;

  // Step 2: Remove dangling /etc symlinks that pointed into the old
  // /etc/static.
  remove_dangling_etc_symlinks(Path::new("/etc"))?;

  // Step 3: Load the set of files that were copied (not symlinked) last time.
  let old_copied = load_clean_list(Path::new("/etc/.clean"))?;

  // Step 4: Walk the $etc tree and apply each file to /etc.
  let mut copied: Vec<String> = Vec::new();
  let mut created: HashSet<String> = HashSet::new();

  // Note: the upstream Perl script appends to /etc/.clean as it walks, then
  // overwrites at the end. If activation crashes mid-walk the file is left
  // with both the old entries and a partial set of new entries. We skip the
  // appending step and only rewrite at the end via atomic_write, so the
  // file is either the old copy or the new one - never a mix.
  apply_etc_tree(
    &etc,
    Path::new("/etc"),
    Path::new(ETC_STATIC),
    &mut copied,
    &mut created,
  )?;

  // Step 5: Remove old copies that no longer exist in the new etc tree.
  for relative_fn in &old_copied {
    if created.contains(relative_fn) {
      continue;
    }
    let target = Path::new("/etc").join(relative_fn);
    eprintln!("removing obsolete /etc/{relative_fn}");
    if let Err(e) = fs::remove_file(&target) {
      // Not fatal: file may have already been removed.
      eprintln!("warning: failed to remove {}: {}", target.display(), e);
    }
  }

  // Step 6: Write the definitive /etc/.clean with all current copies, sorted.
  copied.sort();
  copied.dedup();
  {
    let mut content = String::new();
    for entry in &copied {
      content.push_str(entry);
      content.push('\n');
    }
    atomic_write(Path::new("/etc/.clean"), content.as_bytes(), 0o644)
      .context("Failed to write /etc/.clean")?;
  }

  // Step 7: Ensure the /etc/NIXOS tag file exists.
  create_nixos_tag()?;

  Ok(())
}

/// Walk `etc_store` (a nix store path) and apply entries to `etc_dir` (/etc).
fn apply_etc_tree(
  etc_store: &Path,
  etc_dir: &Path,
  etc_static: &Path,
  copied: &mut Vec<String>,
  created: &mut HashSet<String>,
) -> Result<()> {
  // Use a manual stack to avoid recursion limits on deeply nested trees.
  let mut stack: Vec<PathBuf> = vec![etc_store.to_path_buf()];

  while let Some(current) = stack.pop() {
    // Compute the path relative to the store root.
    let relative = current
      .strip_prefix(etc_store)
      .expect("current is always under etc_store");

    // The root directory itself has no target to create.
    if relative == Path::new("") {
      // Push children in sorted order so we process them deterministically.
      let mut children = read_dir_sorted(&current)?;
      children.reverse(); // stack is LIFO, so reverse to process in order
      for child in children {
        stack.push(child);
      }
      continue;
    }

    // Construct the target path in /etc.
    let target = etc_dir.join(relative);
    let relative_str = relative.to_string_lossy().into_owned();

    // Skip resolv.conf when running inside `nixos-enter`.
    if relative_str == "resolv.conf"
      && std::env::var("IN_NIXOS_ENTER").unwrap_or_default() == "1"
    {
      continue;
    }

    // Ensure the parent directory exists. Matches Perl's
    // `File::Path::make_path(dirname $target)`; if this fails we skip the
    // entry and keep processing the rest of the tree rather than abort the
    // whole activation.
    if let Some(parent) = target.parent()
      && let Err(e) = fs::create_dir_all(parent)
    {
      eprintln!(
        "warning: failed to create parent dir for {}: {e}",
        target.display()
      );
      continue;
    }

    created.insert(relative_str.clone());

    // .mode sidecar file on the store entry indicates a copied file with
    // explicit ownership/permissions (or a direct symlink).
    let mode_file = PathBuf::from(format!("{}.mode", current.display()));

    let current_is_symlink = current
      .symlink_metadata()
      .map(|m| m.file_type().is_symlink())
      .unwrap_or(false);
    let target_is_dir = target.is_dir()
      && !target
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    // If the store entry is a symlink but /etc already has a plain directory:
    // remove the directory if all its contents are themselves static, otherwise
    // warn. Perl uses `rmtree $target or warn;` - non-fatal.
    if current_is_symlink && target_is_dir {
      if is_fully_static(&target, etc_static) {
        if let Err(e) = fs::remove_dir_all(&target) {
          eprintln!(
            "warning: failed to remove static dir {}: {e}",
            target.display()
          );
          continue;
        }
      } else {
        eprintln!(
          "warning: not replacing /etc/{relative_str} (non-static directory) \
           with a symlink"
        );
        continue;
      }
    }

    if mode_file.exists() {
      let mode_str = match fs::read_to_string(&mode_file) {
        Ok(s) => s,
        Err(e) => {
          eprintln!(
            "warning: failed to read {}: {e}",
            mode_file.display()
          );
          continue;
        },
      };
      let mode_str = mode_str.trim();

      if mode_str == "direct-symlink" {
        // The store entry is a symlink; copy the symlink's *target* directly
        // to /etc, rather than pointing into /etc/static.
        let link_target = match fs::read_link(&current) {
          Ok(t) => t,
          Err(e) => {
            eprintln!("warning: failed to read symlink {}: {e}", current.display());
            continue;
          },
        };
        if let Err(e) = atomic_symlink(&link_target, &target) {
          eprintln!(
            "warning: could not create direct symlink {}: {e}",
            target.display()
          );
          continue;
        }
        // Record in copied list; /etc/.clean gets written atomically at the
        // end of the run rather than appended to incrementally.
        copied.push(relative_str.clone());
      } else {
        // Numeric octal mode: copy the file with explicit uid/gid/mode.
        let mode = match u32::from_str_radix(mode_str, 8) {
          Ok(m) => m,
          Err(e) => {
            eprintln!(
              "warning: invalid mode {mode_str:?} in {}: {e}",
              mode_file.display()
            );
            continue;
          },
        };

        let uid_file = PathBuf::from(format!("{}.uid", current.display()));
        let gid_file = PathBuf::from(format!("{}.gid", current.display()));

        let uid_str =
          fs::read_to_string(&uid_file).unwrap_or_else(|_| "0".to_string());
        let gid_str =
          fs::read_to_string(&gid_file).unwrap_or_else(|_| "0".to_string());

        // Leading '+' means the value is already numeric; otherwise resolve
        // the name via the on-disk databases. Perl warns rather than dies
        // here via `$uid = getpwnam $uid`, which returns undef if the name
        // is unknown and subsequently int()s to 0.
        let uid: u32 = match resolve_id(uid_str.trim(), true) {
          Ok(n) => n,
          Err(e) => {
            eprintln!(
              "warning: unknown UID {:?} for /etc/{relative_str}: {e}",
              uid_str.trim()
            );
            continue;
          },
        };
        let gid: u32 = match resolve_id(gid_str.trim(), false) {
          Ok(n) => n,
          Err(e) => {
            eprintln!(
              "warning: unknown GID {:?} for /etc/{relative_str}: {e}",
              gid_str.trim()
            );
            continue;
          },
        };

        // Source is at /etc/static/<relative>.
        let source = etc_static.join(relative);
        let tmp = PathBuf::from(format!("{}.tmp", target.display()));

        if let Err(e) = fs::copy(&source, &tmp) {
          eprintln!(
            "warning: failed to copy {} to {}: {e}",
            source.display(),
            tmp.display()
          );
          continue;
        }
        if let Err(e) = chown(&tmp, Some(uid), Some(gid)) {
          eprintln!("warning: failed to chown {}: {e}", tmp.display());
        }
        if let Err(e) = fs::set_permissions(&tmp, Permissions::from_mode(mode))
        {
          eprintln!("warning: failed to chmod {}: {e}", tmp.display());
        }
        match fs::rename(&tmp, &target) {
          Ok(()) => {
            // Record as a copied file; /etc/.clean is rewritten atomically
            // once the whole walk succeeds.
            copied.push(relative_str.clone());
          },
          Err(e) => {
            eprintln!(
              "warning: failed to rename {} to {}: {e}",
              tmp.display(),
              target.display(),
            );
            let _ = fs::remove_file(&tmp);
          },
        }
      }
    } else if current_is_symlink {
      // No .mode file and the store entry is a symlink: create a /etc/static
      // passthrough symlink, which points into /etc/static/<relative>.
      let static_target = etc_static.join(relative);
      if let Err(e) = atomic_symlink(&static_target, &target) {
        eprintln!(
          "warning: could not create symlink {}: {e}",
          target.display()
        );
      }
    } else if current.is_dir() {
      // Directory: ensure it exists in /etc and descend into it.
      if let Err(e) = fs::create_dir_all(&target) {
        eprintln!(
          "warning: failed to create directory {}: {e}",
          target.display()
        );
        continue;
      }
      match read_dir_sorted(&current) {
        Ok(mut children) => {
          children.reverse();
          for child in children {
            stack.push(child);
          }
        },
        Err(e) => {
          eprintln!(
            "warning: failed to read directory {}: {e}",
            current.display()
          );
        },
      }
    }
    // Regular files without a .mode sidecar are not handled: the Perl script
    // also silently skips them.
  }

  Ok(())
}

/// Remove any symlink inside `etc_dir` whose target starts with `/etc/static/`
/// but whose corresponding `/etc/static/<relative>` path is no longer a symlink
/// (i.e. no longer present in the current configuration).
fn remove_dangling_etc_symlinks(etc_dir: &Path) -> Result<()> {
  let mut stack: Vec<PathBuf> = vec![etc_dir.to_path_buf()];

  while let Some(current) = stack.pop() {
    // Never descend into /etc/nixos.
    if current == etc_dir.join("nixos") {
      continue;
    }

    let meta = match current.symlink_metadata() {
      Ok(m) => m,
      Err(_) => continue,
    };

    if meta.file_type().is_symlink() {
      let link_target = match fs::read_link(&current) {
        Ok(t) => t,
        Err(_) => continue,
      };

      let target_str = link_target.to_string_lossy();
      if !target_str.starts_with("/etc/static/") {
        continue;
      }

      // Relative path from /etc
      let relative = match current.strip_prefix(etc_dir) {
        Ok(r) => r,
        Err(_) => continue,
      };

      // Check whether /etc/static/<relative> is still a symlink.
      // Perl: `-l "$static/$fn"` - symlink check, not existence check.
      let static_path = Path::new(ETC_STATIC).join(relative);
      let still_present = static_path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

      if !still_present {
        eprintln!("removing obsolete symlink {}", current.display());
        if let Err(e) = fs::remove_file(&current) {
          eprintln!("warning: failed to remove {}: {}", current.display(), e);
        }
      }
    } else if meta.is_dir() {
      let mut children = read_dir_sorted(&current)?;
      children.reverse();
      for child in children {
        stack.push(child);
      }
    }
  }

  Ok(())
}

/// Returns true if `path` is a symlink pointing into /etc/static, or a
/// directory whose every descendant satisfies the same condition.
fn is_fully_static(path: &Path, etc_static: &Path) -> bool {
  let meta = match path.symlink_metadata() {
    Ok(m) => m,
    Err(_) => return false,
  };

  if meta.file_type().is_symlink() {
    let target = match fs::read_link(path) {
      Ok(t) => t,
      Err(_) => return false,
    };
    return target.starts_with(etc_static);
  }

  if meta.is_dir() {
    return match fs::read_dir(path) {
      Ok(entries) => {
        entries
          .filter_map(std::result::Result::ok)
          .all(|e| is_fully_static(&e.path(), etc_static))
      },
      Err(_) => false,
    };
  }

  // Regular files are not static.
  false
}

/// Resolve a uid/gid string: if prefixed with '+' or purely numeric, parse
/// directly. Otherwise look up the name in the system password/group database.
fn resolve_id(s: &str, is_uid: bool) -> Result<u32> {
  let s = s.trim_start_matches('+');
  if let Ok(n) = s.parse::<u32>() {
    return Ok(n);
  }
  // Name lookup via NSS - matches Perl's getpwnam/getgrnam.
  if is_uid {
    get_uid_by_name(s).with_context(|| format!("Unknown user '{s}'"))
  } else {
    get_gid_by_name(s).with_context(|| format!("Unknown group '{s}'"))
  }
}

fn get_uid_by_name(name: &str) -> Result<u32> {
  let c_name = std::ffi::CString::new(name).context("Invalid user name")?;
  // SAFETY: getpwnam reads static storage and is not thread-safe, but we call
  // it in a single-threaded context and copy the result immediately.
  let pw = unsafe { libc::getpwnam(c_name.as_ptr()) };
  if pw.is_null() {
    anyhow::bail!("user '{name}' not found");
  }
  Ok(unsafe { (*pw).pw_uid })
}

fn get_gid_by_name(name: &str) -> Result<u32> {
  let c_name = std::ffi::CString::new(name).context("Invalid group name")?;
  // SAFETY: same rationale as get_uid_by_name.
  let gr = unsafe { libc::getgrnam(c_name.as_ptr()) };
  if gr.is_null() {
    anyhow::bail!("group '{name}' not found");
  }
  Ok(unsafe { (*gr).gr_gid })
}

/// Atomically create a symlink at `link` pointing to `target` by using a
/// temporary path and renaming. Removes any existing entry at `link`.
fn atomic_symlink(target: &Path, link: &Path) -> Result<()> {
  let tmp = PathBuf::from(format!("{}.tmp", link.display()));
  // Remove a stale .tmp if one exists.
  let _ = fs::remove_file(&tmp);
  symlink(target, &tmp).with_context(|| {
    format!(
      "Failed to create symlink {} -> {}",
      tmp.display(),
      target.display()
    )
  })?;
  fs::rename(&tmp, link).with_context(|| {
    format!("Failed to rename {} to {}", tmp.display(), link.display())
  })?;
  Ok(())
}

/// Atomically write `content` to `path` via a `.tmp` rename, then set `mode`.
fn atomic_write(path: &Path, content: &[u8], mode: u32) -> Result<()> {
  let tmp = PathBuf::from(format!("{}.tmp", path.display()));
  {
    let mut f = File::create(&tmp)
      .with_context(|| format!("Failed to create {}", tmp.display()))?;
    f.write_all(content)
      .with_context(|| format!("Failed to write {}", tmp.display()))?;
  }
  fs::set_permissions(&tmp, Permissions::from_mode(mode))
    .with_context(|| format!("Failed to chmod {}", tmp.display()))?;
  fs::rename(&tmp, path).with_context(|| {
    format!("Failed to rename {} to {}", tmp.display(), path.display())
  })?;
  Ok(())
}

/// Load the list of previously copied files from /etc/.clean.
/// Returns an empty set if the file does not exist.
fn load_clean_list(path: &Path) -> Result<HashSet<String>> {
  if !path.exists() {
    return Ok(HashSet::new());
  }
  let content = fs::read_to_string(path)
    .with_context(|| format!("Failed to read {}", path.display()))?;
  Ok(
    content
      .lines()
      .filter(|l| !l.is_empty())
      .map(str::to_string)
      .collect(),
  )
}

/// Read the entries of a directory, returning paths sorted by file name.
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
  let mut entries: Vec<PathBuf> = fs::read_dir(dir)
    .with_context(|| format!("Failed to read directory {}", dir.display()))?
    .filter_map(|e| e.ok().map(|e| e.path()))
    .collect();
  entries.sort();
  Ok(entries)
}

/// Touch /etc/NIXOS to mark this as a NixOS system.
pub fn create_nixos_tag() -> Result<()> {
  OpenOptions::new()
    .create(true)
    .append(true)
    .open("/etc/NIXOS")
    .context("Failed to create /etc/NIXOS tag")?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::symlink;

  use tempfile::TempDir;

  use super::*;

  #[test]
  fn test_resolve_id_numeric() {
    assert_eq!(resolve_id("1000", true).unwrap(), 1000);
    assert_eq!(resolve_id("+1000", true).unwrap(), 1000);
    assert_eq!(resolve_id("0", false).unwrap(), 0);
  }

  #[test]
  fn test_load_clean_list_nonexistent() {
    let result = load_clean_list(Path::new("/nonexistent/etc/.clean")).unwrap();
    assert!(result.is_empty());
  }

  #[test]
  fn test_load_clean_list_reads_lines() {
    let dir = TempDir::new().unwrap();
    let clean = dir.path().join(".clean");
    fs::write(&clean, "foo\nbar\nbaz\n").unwrap();
    let result = load_clean_list(&clean).unwrap();
    assert!(result.contains("foo"));
    assert!(result.contains("bar"));
    assert!(result.contains("baz"));
    assert_eq!(result.len(), 3);
  }

  #[test]
  fn test_atomic_symlink_creates_link() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    fs::write(&target, "content").unwrap();
    atomic_symlink(&target, &link).unwrap();
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), target);
  }

  #[test]
  fn test_atomic_symlink_replaces_existing() {
    let dir = TempDir::new().unwrap();
    let target1 = dir.path().join("target1");
    let target2 = dir.path().join("target2");
    let link = dir.path().join("link");
    fs::write(&target1, "a").unwrap();
    fs::write(&target2, "b").unwrap();
    atomic_symlink(&target1, &link).unwrap();
    atomic_symlink(&target2, &link).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), target2);
  }

  #[test]
  fn test_atomic_write_creates_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("out.txt");
    atomic_write(&path, b"hello\n", 0o644).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
  }

  #[test]
  fn test_read_dir_sorted() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("c"), "").unwrap();
    fs::write(dir.path().join("a"), "").unwrap();
    fs::write(dir.path().join("b"), "").unwrap();
    let entries = read_dir_sorted(dir.path()).unwrap();
    let names: Vec<_> = entries
      .iter()
      .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
      .collect();
    assert_eq!(names, vec!["a", "b", "c"]);
  }

  #[test]
  fn test_is_fully_static_symlink_pointing_to_static() {
    let dir = TempDir::new().unwrap();
    let static_dir = dir.path().join("static");
    let etc_dir = dir.path().join("etc");
    fs::create_dir_all(&static_dir).unwrap();
    fs::create_dir_all(&etc_dir).unwrap();
    let link = etc_dir.join("foo");
    symlink(static_dir.join("foo"), &link).unwrap();
    assert!(is_fully_static(&link, &static_dir));
  }

  #[test]
  fn test_is_fully_static_regular_file_is_not_static() {
    let dir = TempDir::new().unwrap();
    let static_dir = dir.path().join("static");
    let file = dir.path().join("regular");
    fs::write(&file, "content").unwrap();
    assert!(!is_fully_static(&file, &static_dir));
  }
}
