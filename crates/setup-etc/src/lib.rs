use std::{
  collections::HashSet,
  fs::{self, OpenOptions},
  os::unix::fs::symlink,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use smfh_core::manifest::{File as ManifestFile, FileKind, Manifest};

/// Update /etc from the current NixOS configuration
#[derive(Parser, Debug)]
#[command(name = "setup-etc")]
#[command(about = "Atomically apply /etc files from /etc/static")]
struct Args {
  /// Path to the /nix/store/..-etc tree
  etc_dir: String,
}

const ETC_STATIC: &str = "/etc/static";
const ETC_MANIFEST_DEFAULT: &str = "/var/lib/nixos";
const ETC_MANIFEST_ENV: &str = "NIXOS_CORE_STATE_DIR";

fn get_etc_manifest() -> PathBuf {
  let state_dir = std::env::var(ETC_MANIFEST_ENV)
    .unwrap_or_else(|_| ETC_MANIFEST_DEFAULT.to_string());
  PathBuf::from(state_dir).join("etc-manifest.json")
}

/// Apply /etc files from the given nix store path, updating /etc/static and all
/// derived symlinks.
pub fn run(args: &[String]) -> Result<()> {
  let args = Args::parse_from(args);
  let etc = PathBuf::from(&args.etc_dir);

  // Step 1: Atomically update the /etc/static symlink.
  atomic_symlink(&etc, Path::new(ETC_STATIC))
    .context("Failed to update /etc/static symlink")?;

  // Step 2: Remove dangling /etc symlinks that pointed into the old
  // /etc/static. Kept as a safety net during transition and for symlinks
  // created outside of the manifest.
  remove_dangling_etc_symlinks(Path::new("/etc"))?;

  // Step 3: Walk the $etc tree and build a smfh manifest.
  let files =
    build_etc_manifest(&etc, Path::new("/etc"), Path::new(ETC_STATIC))?;

  // Step 4: Serialise to JSON. permissions must be octal strings because
  // smfh-core's deserialize_octal only accepts Option<String>.
  let files_json: Vec<serde_json::Value> =
    files.iter().map(file_to_json).collect::<Result<Vec<_>>>()?;
  let manifest_content = serde_json::to_string_pretty(
    &serde_json::json!({"version": 1, "files": files_json}),
  )
  .context("Failed to serialise manifest")?;

  // Step 5: Write to a sibling temp path so the rename in step 7 is atomic.
  let manifest_path = get_etc_manifest();
  let manifest_tmp = PathBuf::from(format!("{}.new", manifest_path.display()));
  fs::create_dir_all(
    manifest_path
      .parent()
      .expect("manifest path always has a parent"),
  )?;
  fs::write(&manifest_tmp, &manifest_content)
    .context("Failed to write manifest")?;

  // Steps 6–7: diff and commit; clean up the temp file on any failure so we
  // do not leave a stale .new file behind.
  let diff_result = (|| {
    // Step 6: Load the new manifest and transition from the old one via diff.
    // fallback=true means a missing or corrupt old manifest triggers a clean
    // activate rather than an error.
    let new_manifest = Manifest::read(&manifest_tmp, false)
      .map_err(|e| anyhow::anyhow!("Failed to parse manifest: {e:?}"))?;

    new_manifest
      .diff(&manifest_path, "", true)
      .map_err(|e| anyhow::anyhow!("Failed to apply manifest diff: {e:?}"))?;

    // Step 7: Commit the new manifest atomically.
    fs::rename(&manifest_tmp, manifest_path)
      .context("Failed to commit manifest")
  })();

  if let Err(ref e) = diff_result {
    eprintln!(
      "warning: manifest activation failed, cleaning up temp file: {e}"
    );
    let _ = fs::remove_file(&manifest_tmp);
  }
  diff_result?;

  // Step 8: Migrate from the legacy Perl tracking file: delete every entry it
  // lists that is no longer in the current configuration (stale copies), then
  // remove the file itself. Entries still in the configuration were already
  // activated by smfh above and must not be deleted. Running after the manifest
  // commit means a failed activation leaves Perl-tracked files intact; the
  // migration retries cleanly on the next run.
  let kept: HashSet<PathBuf> = files.iter().map(|f| f.target.clone()).collect();
  migrate_perl_clean_file(Path::new("/etc/.clean"), Path::new("/etc"), &kept);

  // Step 9: Ensure the /etc/NIXOS tag file exists.
  create_nixos_tag()?;

  Ok(())
}

/// Walk `etc_store` and build a smfh manifest describing every file that
/// belongs under `etc_dir` (/etc). Directories are created eagerly so that
/// smfh has valid parent paths when activating file entries.
fn build_etc_manifest(
  etc_store: &Path,
  etc_dir: &Path,
  etc_static: &Path,
) -> Result<Vec<ManifestFile>> {
  let mut files: Vec<ManifestFile> = Vec::new();
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
    let relative_str = relative.to_string_lossy();

    // Skip resolv.conf when running inside `nixos-enter` (bind-mounted by the
    // host). Perl checks the variable as truthy: non-empty and not "0".
    if relative_str == "resolv.conf"
      && std::env::var("IN_NIXOS_ENTER")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
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

    // .mode sidecar file on the store entry indicates a copied file with
    // explicit ownership/permissions (or a direct symlink).
    let mode_file = PathBuf::from(format!("{}.mode", current.display()));

    if mode_file.exists() {
      let mode_str = match fs::read_to_string(&mode_file) {
        Ok(s) => s,
        Err(e) => {
          eprintln!("warning: failed to read {}: {e}", mode_file.display());
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
            eprintln!(
              "warning: failed to read symlink {}: {e}",
              current.display()
            );
            continue;
          },
        };
        files.push(ManifestFile {
          source: Some(link_target),
          target,
          kind: FileKind::Symlink,
          clobber: Some(true),
          permissions: None,
          uid: None,
          gid: None,
          deactivate: None,
          follow_symlinks: None,
          ignore_modification: None,
        });
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

        // Source is at /etc/static/<relative>. Canonicalize to the real nix
        // store path: smfh's check_source uses symlink_metadata, which does
        // not follow symlinks, so a symlink source fails its is_file() guard
        // and gets silently skipped.
        let real_source = match fs::canonicalize(etc_static.join(relative)) {
          Ok(p) => p,
          Err(e) => {
            eprintln!(
              "warning: failed to canonicalize source for \
               /etc/{relative_str}: {e}"
            );
            continue;
          },
        };
        files.push(ManifestFile {
          source: Some(real_source),
          target,
          kind: FileKind::Copy,
          clobber: Some(true),
          permissions: Some(mode),
          uid: Some(uid),
          gid: Some(gid),
          deactivate: None,
          follow_symlinks: None,
          ignore_modification: None,
        });
      }
    } else if current_is_symlink {
      // No .mode file and the store entry is a symlink: create a /etc/static
      // passthrough symlink, which points into /etc/static/<relative>.
      // follow_symlinks must be false so smfh uses path::absolute() instead of
      // fs::canonicalize(); canonicalize() would resolve /etc/static/<relative>
      // all the way to the nix store, breaking the indirection that lets
      // generation switches work without touching individual /etc symlinks.
      files.push(ManifestFile {
        source: Some(etc_static.join(relative)),
        target,
        kind: FileKind::Symlink,
        clobber: Some(true),
        permissions: None,
        uid: None,
        gid: None,
        deactivate: None,
        follow_symlinks: Some(false),
        ignore_modification: None,
      });
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

  Ok(files)
}

/// Serialise a `ManifestFile` to a JSON value, writing `permissions` as an
/// octal string so that smfh-core's `deserialize_octal` round-trips correctly.
fn file_to_json(file: &ManifestFile) -> Result<serde_json::Value> {
  let mut v = serde_json::to_value(file)
    .context("ManifestFile is always serialisable")?;
  if let Some(perm) = file.permissions {
    v["permissions"] = serde_json::Value::String(format!("{perm:o}"));
  }
  Ok(v)
}

/// Read the legacy Perl `/etc/.clean` state file (one relative path per line),
/// delete every listed file under `etc_dir` that is not in `kept`, and remove
/// the state file. No-op if the file does not exist. `kept` is the set of
/// target paths that the new manifest owns; those entries were already
/// activated by smfh and must not be removed.
fn migrate_perl_clean_file(
  clean_file: &Path,
  etc_dir: &Path,
  kept: &HashSet<PathBuf>,
) {
  let contents = match fs::read_to_string(clean_file) {
    Ok(s) => s,
    Err(_) => return,
  };

  for line in contents.lines() {
    let entry = line.trim();
    if entry.is_empty() || entry.starts_with('/') || entry.contains("..") {
      // Skip blank lines and anything that tries to escape /etc.
      continue;
    }
    let target = etc_dir.join(entry);
    if kept.contains(&target) {
      continue;
    }
    if let Err(e) = fs::remove_file(&target)
      && e.kind() != std::io::ErrorKind::NotFound
    {
      eprintln!(
        "warning: failed to remove legacy Perl-era file {}: {e}",
        target.display()
      );
    }
  }

  if let Err(e) = fs::remove_file(clean_file)
    && e.kind() != std::io::ErrorKind::NotFound
  {
    eprintln!("warning: failed to remove {}: {e}", clean_file.display());
  }
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

  /// Build a minimal fake nix-store etc tree under `store_dir` and call
  /// `build_etc_manifest`, returning the entries.
  fn manifest_for(
    store_dir: &Path,
    etc_dir: &Path,
    static_dir: &Path,
  ) -> Vec<ManifestFile> {
    build_etc_manifest(store_dir, etc_dir, static_dir).unwrap()
  }

  #[test]
  fn test_build_manifest_passthrough_symlink() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let etc = dir.path().join("etc");
    let stat = dir.path().join("static");
    fs::create_dir_all(&store).unwrap();
    fs::create_dir_all(&etc).unwrap();
    fs::create_dir_all(&stat).unwrap();

    // A symlink in the store without a .mode sidecar → pass-through symlink
    symlink("/nix/store/irrelevant", store.join("hostname")).unwrap();

    let files = manifest_for(&store, &etc, &stat);
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(f.kind, FileKind::Symlink);
    assert_eq!(f.target, etc.join("hostname"));
    assert_eq!(f.source, Some(stat.join("hostname")));
    // Must not canonicalize so the symlink points at /etc/static, not the
    // store.
    assert_eq!(f.follow_symlinks, Some(false));
  }

  #[test]
  fn test_build_manifest_direct_symlink() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let etc = dir.path().join("etc");
    let stat = dir.path().join("static");
    fs::create_dir_all(&store).unwrap();
    fs::create_dir_all(&etc).unwrap();
    fs::create_dir_all(&stat).unwrap();

    let nix_target = PathBuf::from("/nix/store/aaaa-foo/bin/sh");
    symlink(&nix_target, store.join("shells")).unwrap();
    fs::write(store.join("shells.mode"), "direct-symlink").unwrap();

    let files = manifest_for(&store, &etc, &stat);
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(f.kind, FileKind::Symlink);
    assert_eq!(f.source, Some(nix_target));
    assert_eq!(f.target, etc.join("shells"));
  }

  #[test]
  fn test_build_manifest_copied_file() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let etc = dir.path().join("etc");
    let stat = dir.path().join("static");
    fs::create_dir_all(&store).unwrap();
    fs::create_dir_all(&etc).unwrap();
    fs::create_dir_all(&stat).unwrap();

    // The store entry drives mode detection; the static entry is the file
    // that gets canonicalized into the manifest source.
    fs::write(store.join("secret"), "sensitive").unwrap();
    fs::write(store.join("secret.mode"), "0600").unwrap();
    fs::write(stat.join("secret"), "sensitive").unwrap();
    // No .uid/.gid files → both default to 0

    let files = manifest_for(&store, &etc, &stat);
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(f.kind, FileKind::Copy);
    assert_eq!(f.permissions, Some(0o600));
    assert_eq!(f.uid, Some(0));
    assert_eq!(f.gid, Some(0));
    // Canonicalized, so the source is the real file, not a symlink.
    assert_eq!(
      f.source,
      Some(fs::canonicalize(stat.join("secret")).unwrap())
    );
    assert_eq!(f.target, etc.join("secret"));
  }

  #[test]
  fn test_resolve_id_numeric() {
    assert_eq!(resolve_id("1000", true).unwrap(), 1000);
    assert_eq!(resolve_id("+1000", true).unwrap(), 1000);
    assert_eq!(resolve_id("0", false).unwrap(), 0);
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

  #[test]
  fn test_file_to_json_permissions_are_octal_string() {
    let file = ManifestFile {
      source:              Some(PathBuf::from("/src/foo")),
      target:              PathBuf::from("/etc/foo"),
      kind:                FileKind::Copy,
      clobber:             Some(true),
      permissions:         Some(0o644),
      uid:                 Some(0),
      gid:                 Some(0),
      deactivate:          None,
      follow_symlinks:     None,
      ignore_modification: None,
    };
    let v = file_to_json(&file).unwrap();
    assert_eq!(
      v["permissions"],
      serde_json::Value::String("644".to_string())
    );
  }
}
