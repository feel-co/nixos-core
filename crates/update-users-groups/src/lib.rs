use std::{
  collections::{HashMap, HashSet},
  fs::{self, File, Permissions},
  io::{BufRead, BufReader, Write},
  os::unix::fs::{PermissionsExt, chown},
  path::Path,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};

// These are the only safe-to-call libc functions we need: getgrgid/getpwuid for
// checking whether a candidate ID is already claimed by the OS name service.
// They are inherently unsafe (static storage, not thread-safe) but there is no
// pure-Rust alternative that queries NSS rather than just /etc/passwd.
unsafe extern "C" {
  fn getgrgid(gid: u32) -> *mut libc::group;
  fn getpwuid(uid: u32) -> *mut libc::passwd;
}

const UID_MAP_FILE: &str = "/var/lib/nixos/uid-map";
const GID_MAP_FILE: &str = "/var/lib/nixos/gid-map";
const DECL_USERS_FILE: &str = "/var/lib/nixos/declarative-users";
const DECL_GROUPS_FILE: &str = "/var/lib/nixos/declarative-groups";
const SUBUID_MAP_FILE: &str = "/var/lib/nixos/auto-subuid-map";

const SYSTEM_UID_MIN: u32 = 400;
const SYSTEM_UID_MAX: u32 = 999;
const NORMAL_UID_MIN: u32 = 1000;
const NORMAL_UID_MAX: u32 = 29999;

const SYSTEM_GID_MIN: u32 = 400;
const SYSTEM_GID_MAX: u32 = 999;

const SUBUID_MIN: u32 = 100000;
const SUBUID_MAX: u32 = 100000 + 29000 * 65536 - 1;
const SUBUID_DELTA: u32 = 65536;

/// Manage /etc/passwd, /etc/group, and /etc/shadow
#[derive(Parser, Debug)]
#[command(name = "update-users-groups")]
#[command(about = "Update system user and group databases")]
struct Args {
  /// Path to JSON spec file
  spec_file: String,

  /// Dry run - don't make any changes
  #[arg(long = "dry-activate")]
  dry_activate: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spec {
  #[serde(default)]
  mutable_users: bool,
  users:         Vec<UserSpec>,
  groups:        Vec<GroupSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserSpec {
  name:                    String,
  uid:                     Option<u32>,
  group:                   String,
  #[serde(default)]
  is_system_user:          bool,
  description:             Option<String>,
  home:                    Option<String>,
  shell:                   Option<String>,
  #[serde(default)]
  create_home:             bool,
  home_mode:               Option<String>,
  hashed_password:         Option<String>,
  initial_password:        Option<String>,
  initial_hashed_password: Option<String>,
  password:                Option<String>,
  hashed_password_file:    Option<String>,
  expires:                 Option<String>,
  #[serde(default)]
  sub_uid_ranges:          Vec<SubUidRange>,
  #[serde(default)]
  sub_gid_ranges:          Vec<SubGidRange>,
  #[serde(default)]
  auto_sub_uid_gid_range:  bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupSpec {
  name:    String,
  gid:     Option<u32>,
  #[serde(default)]
  members: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubUidRange {
  start_uid: u32,
  count:     u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubGidRange {
  start_gid: u32,
  count:     u32,
}

#[derive(Debug, Clone)]
struct GroupEntry {
  name:     String,
  password: String,
  gid:      u32,
  members:  String,
}

#[derive(Debug, Clone)]
struct UserEntry {
  name:          String,
  fake_password: String,
  uid:           u32,
  gid:           u32,
  description:   String,
  home:          String,
  shell:         String,
}

/// Update /etc/passwd, /etc/group, /etc/shadow, and related state files from a
/// JSON spec.
pub fn run(args: &[String]) -> Result<()> {
  let args = Args::parse_from(args);

  let is_dry = args.dry_activate
    || std::env::var("NIXOS_ACTION").unwrap_or_default() == "dry-activate";

  if !is_dry {
    fs::create_dir_all("/var/lib/nixos")?;
  }

  let mut uid_map: HashMap<String, u32> = load_json_file(UID_MAP_FILE)?;
  let mut gid_map: HashMap<String, u32> = load_json_file(GID_MAP_FILE)?;
  let mut sub_uid_map: HashMap<String, u32> = load_json_file(SUBUID_MAP_FILE)?;

  let decl_users = load_declarative_list(DECL_USERS_FILE)?;
  let decl_groups = load_declarative_list(DECL_GROUPS_FILE)?;

  let spec_content = fs::read_to_string(&args.spec_file)
    .with_context(|| format!("Failed to read spec file: {}", args.spec_file))?;
  let spec: Spec =
    serde_json::from_str(&spec_content).context("Failed to parse spec JSON")?;

  // Pre-mark all explicitly assigned IDs so allocation never reuses them.
  let mut gids_used: HashSet<u32> = HashSet::new();
  let mut uids_used: HashSet<u32> = HashSet::new();
  let gids_prev_used: HashSet<u32> = gid_map.values().copied().collect();
  let uids_prev_used: HashSet<u32> = uid_map.values().copied().collect();

  for g in &spec.groups {
    if let Some(gid) = g.gid {
      gids_used.insert(gid);
    }
  }
  for u in &spec.users {
    if let Some(uid) = u.uid {
      uids_used.insert(uid);
    }
  }

  let mut groups_cur: HashMap<String, GroupEntry> = HashMap::new();
  if Path::new("/etc/group").exists() {
    let file = File::open("/etc/group").context("Failed to open /etc/group")?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
      let line = line.context("Failed to read /etc/group")?;
      if let Some(entry) = parse_group_line(&line, &mut gids_used)? {
        groups_cur.insert(entry.name.clone(), entry);
      }
    }
  }

  let mut groups_out: HashMap<String, GroupEntry> = HashMap::new();

  for g in &spec.groups {
    let name = &g.name;
    let existing = groups_cur.get(name);

    let (password, gid, merged) = if let Some(existing) = existing {
      let existing_gid = existing.gid;
      // Only warn when the spec explicitly requests a *different* GID.
      // Matches Perl: `if (defined $gId && $gId != $gid)`.
      if let Some(spec_gid) = g.gid
        && spec_gid != existing_gid
      {
        dry_print(
          is_dry,
          "warning: not applying",
          "warning: would not apply",
          &format!(
            "GID change of group '{name}' ({existing_gid} -> {spec_gid}) in \
             /etc/group"
          ),
        );
      }

      // When mutableUsers is set, keep non-declarative members from the
      // existing entry; spec-declared members always win.
      let mut merged: HashSet<String> = g.members.iter().cloned().collect();
      if spec.mutable_users {
        for m in existing.members.split(',').filter(|m| !m.is_empty()) {
          if !decl_users.contains(m) {
            merged.insert(m.to_string());
          }
        }
      }

      (existing.password.clone(), existing_gid, merged)
    } else {
      let gid = match g.gid {
        Some(gid) => gid,
        None => {
          alloc_gid(name, &mut gids_used, &gids_prev_used, &gid_map, is_dry)?
        },
      };
      let members: HashSet<String> = g.members.iter().cloned().collect();
      ("x".to_string(), gid, members)
    };

    // Members are sorted for deterministic output (Perl: `sort keys
    // %$members`).
    let mut members_vec: Vec<String> = merged.into_iter().collect();
    members_vec.sort();
    let members_str = members_vec.join(",");

    groups_out.insert(name.clone(), GroupEntry {
      name: name.clone(),
      password,
      gid,
      members: members_str,
    });

    gid_map.insert(name.clone(), gid);
  }

  // Write declarative-groups list, sorted.
  {
    let mut names: Vec<&String> = groups_out.keys().collect();
    names.sort();
    update_file(
      DECL_GROUPS_FILE,
      &names
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" "),
      0o644,
      is_dry,
    )?;
  }

  // Merge existing groups not in the spec.
  for (name, g) in &groups_cur {
    if groups_out.contains_key(name) {
      continue;
    }
    if !spec.mutable_users || decl_groups.contains(name) {
      dry_print(
        is_dry,
        "removing group",
        "would remove group",
        &format!("'{name}'"),
      );
    } else {
      groups_out.insert(name.clone(), g.clone());
    }
  }

  // Write /etc/group sorted by GID.
  {
    let mut lines: Vec<String> = groups_out
      .values()
      .map(|g| format!("{}:{}:{}:{}", g.name, g.password, g.gid, g.members))
      .collect();
    lines.sort_by_key(|l| {
      l.split(':')
        .nth(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
    });
    update_file_lines("/etc/group", &lines, 0o644, is_dry)?;
  }

  update_file_json(GID_MAP_FILE, &gid_map, is_dry)?;

  if !is_dry {
    let _ = std::process::Command::new("nscd")
      .args(["--invalidate", "group"])
      .spawn();
  }

  let mut users_cur: HashMap<String, UserEntry> = HashMap::new();
  if Path::new("/etc/passwd").exists() {
    let file =
      File::open("/etc/passwd").context("Failed to open /etc/passwd")?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
      let line = line.context("Failed to read /etc/passwd")?;
      if let Some(entry) = parse_passwd_line(&line, &mut uids_used)? {
        users_cur.insert(entry.name.clone(), entry);
      }
    }
  }

  let mut users_out: HashMap<String, UserEntry> = HashMap::new();
  // Track the effective hashed password per user for shadow writing.
  let mut computed_passwords: HashMap<String, Option<String>> = HashMap::new();

  for u in &spec.users {
    let name = &u.name;

    // Resolve primary GID. The Perl regex `$u->{group} =~ /^[0-9]$/` only
    // matched a *single* digit - an obvious bug. We parse any numeric string,
    // which is the clearly intended behaviour.
    let gid = if let Ok(numeric_gid) = u.group.parse::<u32>() {
      numeric_gid
    } else if let Some(g) = groups_out.get(&u.group) {
      g.gid
    } else {
      eprintln!("warning: user '{}' has unknown group '{}'", name, u.group);
      65534
    };

    let existing = users_cur.get(name);

    // Compute effective hashed password from all sources, in priority order.
    // This value is carried forward to shadow writing; it must not be lost.
    let mut hashed_password: Option<String> = u.hashed_password.clone();

    let uid = if let Some(existing) = existing {
      let existing_uid = existing.uid;
      // Only warn when spec explicitly requests a *different* UID.
      if let Some(spec_uid) = u.uid
        && spec_uid != existing_uid
      {
        dry_print(
          is_dry,
          "warning: not applying",
          "warning: would not apply",
          &format!(
            "UID change of user '{name}' ({existing_uid} -> {spec_uid}) in \
             /etc/passwd"
          ),
        );
      }
      existing_uid
    } else {
      let uid = match u.uid {
        Some(uid) => uid,
        None => {
          alloc_uid(
            name,
            u.is_system_user,
            &mut uids_used,
            &uids_prev_used,
            &uid_map,
            is_dry,
          )?
        },
      };

      // Initial password only applies to newly created accounts.
      if hashed_password.is_none() {
        if let Some(ref initial) = u.initial_password {
          hashed_password = Some(hash_password(initial)?);
        } else if let Some(ref initial_hashed) = u.initial_hashed_password {
          hashed_password = Some(initial_hashed.clone());
        }
      }

      uid
    };

    // hashedPasswordFile overrides everything; password field hashes on the
    // fly. These override both hashed_password and initial_* for all
    // accounts.
    if let Some(ref pw_file) = u.hashed_password_file {
      match fs::read_to_string(pw_file) {
        Ok(pw) => hashed_password = Some(pw.trim().to_string()),
        Err(_) => {
          eprintln!("warning: password file '{pw_file}' does not exist");
        },
      }
    } else if let Some(ref pw) = u.password {
      hashed_password = Some(hash_password(pw)?);
    }

    // Create home directory if requested.
    if u.create_home
      && !is_dry
      && let Some(ref home) = u.home
    {
      let home_path = Path::new(home);
      // Refuse to chown if home is a symlink; would change ownership of the
      // link target.
      if let Ok(meta) = home_path.symlink_metadata()
        && meta.file_type().is_symlink()
      {
        bail!("Home directory path '{home}' is a symlink - refusing to chown");
      }
      if !home_path.exists() {
        // Only chown freshly created home dirs.
        fs::create_dir_all(home).with_context(|| {
          format!("Failed to create home directory: {home}")
        })?;
        chown(home, Some(uid), Some(gid))
          .with_context(|| format!("Failed to chown home directory: {home}"))?;
      }
      if let Some(ref mode_str) = u.home_mode {
        let mode = u32::from_str_radix(mode_str, 8).with_context(|| {
          format!("Invalid home mode '{mode_str}' for user '{name}'")
        })?;
        fs::set_permissions(home, Permissions::from_mode(mode))
          .with_context(|| format!("Failed to chmod home directory: {home}"))?;
      }
    }

    // Shell: spec > existing > nologin fallback.
    let shell = u
      .shell
      .clone()
      .or_else(|| existing.map(|e| e.shell.clone()))
      .unwrap_or_else(|| {
        eprintln!(
          "warning: no declarative or previous shell for '{name}', setting \
           shell to nologin"
        );
        "/run/current-system/sw/bin/nologin".to_string()
      });

    let fake_password =
      existing.map_or_else(|| "x".to_string(), |e| e.fake_password.clone());

    computed_passwords.insert(name.clone(), hashed_password);

    users_out.insert(name.clone(), UserEntry {
      name: name.clone(),
      fake_password,
      uid,
      gid,
      description: u.description.clone().unwrap_or_default(),
      home: u.home.clone().unwrap_or_default(),
      shell,
    });

    uid_map.insert(name.clone(), uid);
  }

  // Write declarative-users list, sorted.
  {
    let mut names: Vec<&String> = users_out.keys().collect();
    names.sort();
    update_file(
      DECL_USERS_FILE,
      &names
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" "),
      0o644,
      is_dry,
    )?;
  }

  // Merge existing users not in the spec.
  for (name, u) in &users_cur {
    if users_out.contains_key(name) {
      continue;
    }
    if !spec.mutable_users || decl_users.contains(name) {
      dry_print(
        is_dry,
        "removing user",
        "would remove user",
        &format!("'{name}'"),
      );
    } else {
      users_out.insert(name.clone(), u.clone());
    }
  }

  // Write /etc/passwd sorted by UID.
  {
    let mut lines: Vec<String> = users_out
      .values()
      .map(|u| {
        format!(
          "{}:{}:{}:{}:{}:{}:{}",
          u.name, u.fake_password, u.uid, u.gid, u.description, u.home, u.shell
        )
      })
      .collect();
    lines.sort_by_key(|l| {
      l.split(':')
        .nth(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
    });
    update_file_lines("/etc/passwd", &lines, 0o644, is_dry)?;
  }

  update_file_json(UID_MAP_FILE, &uid_map, is_dry)?;

  if !is_dry {
    let _ = std::process::Command::new("nscd")
      .args(["--invalidate", "passwd"])
      .spawn();
  }

  let mut shadow_seen: HashSet<String> = HashSet::new();
  let mut shadow_lines: Vec<String> = Vec::new();

  if Path::new("/etc/shadow").exists() {
    let file =
      File::open("/etc/shadow").context("Failed to open /etc/shadow")?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
      let line = line.context("Failed to read /etc/shadow")?;
      let parts: Vec<&str> = line.split(':').collect();
      if parts.len() < 9 {
        continue;
      }

      let sp_namp = parts[0];

      // Drop entries for users no longer in users_out.
      if !users_out.contains_key(sp_namp) {
        continue;
      }

      let mut sp_pwdp = parts[1].to_string();
      let sp_lstchg = parts[2];
      let sp_min = parts[3];
      let sp_max = parts[4];
      let sp_warn = parts[5];
      let sp_inact = parts[6];
      let mut sp_expire = parts[7].to_string();
      let sp_flag = parts[8];

      // In immutable mode, lock the account unless the spec supplies a hash.
      if !spec.mutable_users {
        sp_pwdp = "!".to_string();
      }

      // If the spec has an explicit hash, apply it regardless of mutability.
      // In immutable mode the account was locked above (!), but an explicit
      // hashedPassword always overrides that lock - for both mutable and
      // immutable users. (This matches the Perl TODO comment.)
      if let Some(Some(hp)) = computed_passwords.get(sp_namp) {
        sp_pwdp = hp.clone();
      }

      // Apply expires field if present in the spec.
      if let Some(spec_user) = spec.users.iter().find(|su| su.name == sp_namp)
        && let Some(ref exp) = spec_user.expires
      {
        sp_expire = date_to_days(exp)
          .with_context(|| {
            format!("Invalid expires date '{exp}' for user '{sp_namp}'")
          })?
          .to_string();
      }

      shadow_lines.push(format!(
        "{sp_namp}:{sp_pwdp}:{sp_lstchg}:{sp_min}:{sp_max}:{sp_warn}:\
         {sp_inact}:{sp_expire}:{sp_flag}"
      ));
      shadow_seen.insert(sp_namp.to_string());
    }
  }

  // Add shadow entries for users not yet present.
  // Shadow format: namp:pwdp:lstchg:min:max:warn:inact:expire:flag
  // We set lstchg=1 (Perl FIXME), and leave min/max/warn/inact empty.
  for name in users_out.keys() {
    if shadow_seen.contains(name) {
      continue;
    }

    let hashed_password = computed_passwords
      .get(name)
      .and_then(|hp| hp.as_deref())
      .unwrap_or("!");

    let expires =
      if let Some(spec_user) = spec.users.iter().find(|su| su.name == *name) {
        match spec_user.expires {
          Some(ref exp) => {
            date_to_days(exp)
              .with_context(|| {
                format!("Invalid expires date '{exp}' for user '{name}'")
              })?
              .to_string()
          },
          None => String::new(),
        }
      } else {
        String::new()
      };

    // Fields: namp:pwdp:lstchg:min:max:warn:inact:expire:flag
    // The format has 9 colon-separated fields; expires must be in field 8
    // (expire), NOT field 7 (inact). Count:
    // name:hash:1:min:max:warn:inact:expire:flag
    shadow_lines.push(format!("{name}:{hashed_password}:1:::::{expires}:"));
  }

  update_file_lines("/etc/shadow", &shadow_lines, 0o640, is_dry)?;

  // chown /etc/shadow to root:shadow using the GID from groups_out (already in
  // memory - no need to re-read /etc/group from disk).
  if !is_dry {
    let shadow_gid = groups_out.get("shadow").map_or(0, |g| g.gid);
    chown("/etc/shadow", Some(0u32), Some(shadow_gid))
      .context("Failed to change ownership of /etc/shadow")?;
  }

  let mut sub_uids_used: HashSet<u32> = HashSet::new();
  let sub_uids_prev_used: HashSet<u32> =
    sub_uid_map.values().copied().collect();

  let mut sub_uids: Vec<String> = Vec::new();
  let mut sub_gids: Vec<String> = Vec::new();

  for spec_user in &spec.users {
    let name = &spec_user.name;
    if !users_out.contains_key(name) {
      continue;
    }

    for range in &spec_user.sub_uid_ranges {
      sub_uids.push(format!("{}:{}:{}", name, range.start_uid, range.count));
    }
    for range in &spec_user.sub_gid_ranges {
      sub_gids.push(format!("{}:{}:{}", name, range.start_gid, range.count));
    }

    if spec_user.auto_sub_uid_gid_range {
      let subordinate = alloc_sub_uid(
        name,
        &mut sub_uids_used,
        &sub_uids_prev_used,
        &sub_uid_map,
      )?;

      // Warn if the auto-allocated range shifted (collision with another user).
      if let Some(&prev) = sub_uid_map.get(name)
        && prev != subordinate
      {
        eprintln!(
          "warning: The subuids for '{name}' changed, as they coincided with \
           the subuids of a different user (see /etc/subuid). The range now \
           starts with {subordinate} instead of {prev}. If the subuids were \
           used (e.g. with rootless container managers like podman), please \
           change the ownership of affected files accordingly. Alternatively, \
           to keep the old overlapping ranges, add this to the system \
           configuration:\n  users.users.{name}.subUidRanges = [{{startUid = \
           {prev}; count = 65536;}}];\n  users.users.{name}.subGidRanges = \
           [{{startGid = {prev}; count = 65536;}}];"
        );
      }

      sub_uid_map.insert(name.clone(), subordinate);

      sub_uids.push(format!("{name}:{subordinate}:65536"));
      sub_gids.push(format!("{name}:{subordinate}:65536"));
    }
  }

  update_file_lines("/etc/subuid", &sub_uids, 0o644, is_dry)?;
  update_file_lines("/etc/subgid", &sub_gids, 0o644, is_dry)?;
  update_file_json(SUBUID_MAP_FILE, &sub_uid_map, is_dry)?;

  Ok(())
}

fn load_json_file<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
  let p = Path::new(path);
  if !p.exists() {
    return Ok(serde_json::from_str("{}")?);
  }
  let content =
    fs::read_to_string(p).with_context(|| format!("Failed to read {path}"))?;
  if content.trim().is_empty() {
    return Ok(serde_json::from_str("{}")?);
  }
  serde_json::from_str(&content)
    .with_context(|| format!("Failed to parse JSON from {path}"))
}

fn load_declarative_list(path: &str) -> Result<HashSet<String>> {
  if !Path::new(path).exists() {
    return Ok(HashSet::new());
  }
  let content = fs::read_to_string(path)
    .with_context(|| format!("Failed to read declarative list: {path}"))?;
  Ok(content.split_whitespace().map(str::to_string).collect())
}

fn parse_group_line(
  line: &str,
  gids_used: &mut HashSet<u32>,
) -> Result<Option<GroupEntry>> {
  // Skip comment lines and empty lines.
  let line = line.trim();
  if line.is_empty() || line.starts_with('#') {
    return Ok(None);
  }
  let parts: Vec<&str> = line.split(':').collect();
  if parts.len() != 4 {
    return Ok(None);
  }
  let gid = parts[2].parse::<u32>().with_context(|| {
    format!("Invalid GID '{}' in /etc/group line: {}", parts[2], line)
  })?;
  gids_used.insert(gid);
  Ok(Some(GroupEntry {
    name: parts[0].to_string(),
    password: parts[1].to_string(),
    gid,
    members: parts[3].to_string(),
  }))
}

fn parse_passwd_line(
  line: &str,
  uids_used: &mut HashSet<u32>,
) -> Result<Option<UserEntry>> {
  let line = line.trim();
  if line.is_empty() || line.starts_with('#') {
    return Ok(None);
  }
  let parts: Vec<&str> = line.split(':').collect();
  if parts.len() != 7 {
    return Ok(None);
  }
  let uid = parts[2].parse::<u32>().with_context(|| {
    format!("Invalid UID '{}' in /etc/passwd line: {}", parts[2], line)
  })?;
  let gid = parts[3].parse::<u32>().with_context(|| {
    format!("Invalid GID '{}' in /etc/passwd line: {}", parts[3], line)
  })?;
  uids_used.insert(uid);
  Ok(Some(UserEntry {
    name: parts[0].to_string(),
    fake_password: parts[1].to_string(),
    uid,
    gid,
    description: parts[4].to_string(),
    home: parts[5].to_string(),
    shell: parts[6].to_string(),
  }))
}

// Allocate an unused GID from the system range (400-999), scanning downward.
fn alloc_gid(
  name: &str,
  gids_used: &mut HashSet<u32>,
  gids_prev_used: &HashSet<u32>,
  gid_map: &HashMap<String, u32>,
  is_dry: bool,
) -> Result<u32> {
  // Revival: if this group previously existed and its old GID is still free,
  // reuse it. We only check gids_used (currently allocated this run), NOT
  // gids_prev_used: gids_prev_used is derived from gid_map itself, so any
  // prev_gid from gid_map would always appear in gids_prev_used, making
  // revival impossible.
  if let Some(&prev_gid) = gid_map.get(name)
    && !gids_used.contains(&prev_gid)
  {
    dry_print(
      is_dry,
      "reviving",
      "would revive",
      &format!("group '{name}' with GID {prev_gid}"),
    );
    gids_used.insert(prev_gid);
    return Ok(prev_gid);
  }

  // Scan downward from 999.
  let mut gid = SYSTEM_GID_MAX;
  loop {
    if !gids_used.contains(&gid)
      && !gids_prev_used.contains(&gid)
      && unsafe { getgrgid(gid).is_null() }
    {
      gids_used.insert(gid);
      return Ok(gid);
    }
    if gid == SYSTEM_GID_MIN {
      break;
    }
    gid -= 1;
  }

  bail!("out of free GIDs in range {SYSTEM_GID_MIN}-{SYSTEM_GID_MAX}");
}

// Allocate an unused UID. System users scan 400-999 downward; regular users
// scan 1000-29999 upward.
fn alloc_uid(
  name: &str,
  is_system: bool,
  uids_used: &mut HashSet<u32>,
  uids_prev_used: &HashSet<u32>,
  uid_map: &HashMap<String, u32>,
  is_dry: bool,
) -> Result<u32> {
  let (min, max, downward) = if is_system {
    (SYSTEM_UID_MIN, SYSTEM_UID_MAX, true)
  } else {
    (NORMAL_UID_MIN, NORMAL_UID_MAX, false)
  };

  // Revival: reuse the previous UID if it is still in the correct range and
  // free. We only check uids_used (currently allocated this run), NOT
  // uids_prev_used: uids_prev_used is derived from uid_map itself, so any
  // prev_uid from uid_map would always appear in uids_prev_used, making
  // revival impossible.
  if let Some(&prev_uid) = uid_map.get(name)
    && prev_uid >= min
    && prev_uid <= max
    && !uids_used.contains(&prev_uid)
  {
    dry_print(
      is_dry,
      "reviving",
      "would revive",
      &format!("user '{name}' with UID {prev_uid}"),
    );
    uids_used.insert(prev_uid);
    return Ok(prev_uid);
  }

  if downward {
    let mut uid = max;
    loop {
      if !uids_used.contains(&uid)
        && !uids_prev_used.contains(&uid)
        && unsafe { getpwuid(uid).is_null() }
      {
        uids_used.insert(uid);
        return Ok(uid);
      }
      if uid == min {
        break;
      }
      uid -= 1;
    }
  } else {
    let mut uid = min;
    while uid <= max {
      if !uids_used.contains(&uid)
        && !uids_prev_used.contains(&uid)
        && unsafe { getpwuid(uid).is_null() }
      {
        uids_used.insert(uid);
        return Ok(uid);
      }
      uid += 1;
    }
  }

  bail!("out of free UIDs in range {min}-{max}");
}

// Allocate a subordinate UID starting point (65536-aligned, range 100000+).
fn alloc_sub_uid(
  name: &str,
  sub_uids_used: &mut HashSet<u32>,
  sub_uids_prev_used: &HashSet<u32>,
  sub_uid_map: &HashMap<String, u32>,
) -> Result<u32> {
  // Revival: reuse the previously allocated start if it is still free.
  if let Some(&prev_id) = sub_uid_map.get(name)
    && !sub_uids_used.contains(&prev_id)
  {
    sub_uids_used.insert(prev_id);
    return Ok(prev_id);
  }

  let mut id = SUBUID_MIN;
  while id <= SUBUID_MAX {
    if !sub_uids_used.contains(&id) && !sub_uids_prev_used.contains(&id) {
      sub_uids_used.insert(id);
      return Ok(id);
    }
    id = id
      .checked_add(SUBUID_DELTA)
      .ok_or_else(|| anyhow::anyhow!("subordinate UID range overflow"))?;
  }

  bail!("out of free subordinate UIDs");
}

// Convert an ISO-8601 date string (YYYY-MM-DD) to days since the Unix epoch,
// matching Perl's `int(timelocal(0,0,0,$mday,$mon-1,$year-1900)/86400)`.
fn date_to_days(date: &str) -> Result<u64> {
  use chrono::NaiveDate;
  let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").with_context(|| {
    format!("Invalid date format '{date}', expected YYYY-MM-DD")
  })?;
  let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch is valid");
  let days = d.signed_duration_since(epoch).num_days();
  if days < 0 {
    bail!("expires date '{date}' is before the Unix epoch");
  }
  Ok(days as u64)
}

fn hash_password(password: &str) -> Result<String> {
  use rand::RngExt;
  const CHARSET: &[u8] =
    b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  let mut rng = rand::rng();
  let salt: String = (0..8)
    .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
    .collect();
  let settings = format!("$6${salt}$");
  pwhash::sha512_crypt::hash_with(&*settings, password.as_bytes())
    .map_err(|e| anyhow::anyhow!("Failed to hash password: {e:?}"))
}

fn dry_print(is_dry: bool, action: &str, dry_action: &str, target: &str) {
  if is_dry {
    eprintln!("{dry_action} {target}");
  } else {
    eprintln!("{action} {target}");
  }
}

fn update_file(
  path: &str,
  content: &str,
  mode: u32,
  is_dry: bool,
) -> Result<()> {
  if is_dry {
    return Ok(());
  }
  let temp = format!("{path}.tmp");
  {
    let mut f = File::create(&temp)
      .with_context(|| format!("Failed to create {temp}"))?;
    f.write_all(content.as_bytes())
      .with_context(|| format!("Failed to write {temp}"))?;
  }
  fs::set_permissions(&temp, Permissions::from_mode(mode))
    .with_context(|| format!("Failed to set permissions on {temp}"))?;
  fs::rename(&temp, path)
    .with_context(|| format!("Failed to rename {temp} to {path}"))?;
  Ok(())
}

fn update_file_lines(
  path: &str,
  lines: &[String],
  mode: u32,
  is_dry: bool,
) -> Result<()> {
  let mut content = lines.join("\n");
  if !content.is_empty() {
    content.push('\n');
  }
  update_file(path, &content, mode, is_dry)
}

fn update_file_json<T: Serialize>(
  path: &str,
  data: &T,
  is_dry: bool,
) -> Result<()> {
  if is_dry {
    return Ok(());
  }
  let content =
    serde_json::to_string_pretty(data).context("Failed to serialize JSON")?;
  update_file(path, &content, 0o644, is_dry)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_group_line() {
    let mut gids_used = HashSet::new();
    let entry = parse_group_line("users:x:1000:user1,user2", &mut gids_used)
      .unwrap()
      .unwrap();
    assert_eq!(entry.name, "users");
    assert_eq!(entry.password, "x");
    assert_eq!(entry.gid, 1000);
    assert_eq!(entry.members, "user1,user2");
    assert!(gids_used.contains(&1000));
  }

  #[test]
  fn test_parse_group_line_empty_members() {
    let mut gids_used = HashSet::new();
    let entry = parse_group_line("nogroup:x:65534:", &mut gids_used)
      .unwrap()
      .unwrap();
    assert_eq!(entry.members, "");
    assert_eq!(entry.gid, 65534);
  }

  #[test]
  fn test_parse_group_line_comment_skipped() {
    let mut gids_used = HashSet::new();
    assert!(
      parse_group_line("# comment", &mut gids_used)
        .unwrap()
        .is_none()
    );
    assert!(parse_group_line("", &mut gids_used).unwrap().is_none());
  }

  #[test]
  fn test_parse_passwd_line() {
    let mut uids_used = HashSet::new();
    let entry = parse_passwd_line(
      "testuser:x:1000:1000:Test User:/home/testuser:/bin/bash",
      &mut uids_used,
    )
    .unwrap()
    .unwrap();
    assert_eq!(entry.name, "testuser");
    assert_eq!(entry.uid, 1000);
    assert_eq!(entry.gid, 1000);
    assert_eq!(entry.description, "Test User");
    assert_eq!(entry.home, "/home/testuser");
    assert_eq!(entry.shell, "/bin/bash");
    assert!(uids_used.contains(&1000));
  }

  #[test]
  fn test_parse_passwd_line_comment_skipped() {
    let mut uids_used = HashSet::new();
    assert!(
      parse_passwd_line("# comment", &mut uids_used)
        .unwrap()
        .is_none()
    );
  }

  #[test]
  fn test_alloc_gid_revive() {
    let mut gids_used = HashSet::new();
    let gids_prev_used = HashSet::new();
    let mut gid_map = HashMap::new();
    gid_map.insert("testgroup".to_string(), 500);
    let gid = alloc_gid(
      "testgroup",
      &mut gids_used,
      &gids_prev_used,
      &gid_map,
      false,
    )
    .unwrap();
    assert_eq!(gid, 500);
    assert!(gids_used.contains(&500));
  }

  #[test]
  fn test_alloc_gid_no_revive_when_in_used() {
    let mut gids_used = HashSet::new();
    gids_used.insert(500);
    let gids_prev_used = HashSet::new();
    let mut gid_map = HashMap::new();
    gid_map.insert("testgroup".to_string(), 500);
    // Should allocate a different GID since 500 is taken.
    let gid = alloc_gid(
      "testgroup",
      &mut gids_used,
      &gids_prev_used,
      &gid_map,
      false,
    )
    .unwrap();
    assert_ne!(gid, 500);
    assert!((SYSTEM_GID_MIN..=SYSTEM_GID_MAX).contains(&gid));
  }

  #[test]
  fn test_alloc_uid_system() {
    let mut uids_used = HashSet::new();
    let uids_prev_used = HashSet::new();
    let uid_map = HashMap::new();
    let uid = alloc_uid(
      "sysuser",
      true,
      &mut uids_used,
      &uids_prev_used,
      &uid_map,
      false,
    )
    .unwrap();
    assert!((SYSTEM_UID_MIN..=SYSTEM_UID_MAX).contains(&uid));
    assert!(uids_used.contains(&uid));
  }

  #[test]
  fn test_alloc_uid_normal() {
    let mut uids_used = HashSet::new();
    let uids_prev_used = HashSet::new();
    let uid_map = HashMap::new();
    let uid = alloc_uid(
      "normaluser",
      false,
      &mut uids_used,
      &uids_prev_used,
      &uid_map,
      false,
    )
    .unwrap();
    assert!((NORMAL_UID_MIN..=NORMAL_UID_MAX).contains(&uid));
    assert!(uids_used.contains(&uid));
  }

  #[test]
  fn test_alloc_uid_revive() {
    let mut uids_used = HashSet::new();
    let uids_prev_used = HashSet::new();
    let mut uid_map = HashMap::new();
    uid_map.insert("user".to_string(), 1500);
    let uid = alloc_uid(
      "user",
      false,
      &mut uids_used,
      &uids_prev_used,
      &uid_map,
      false,
    )
    .unwrap();
    assert_eq!(uid, 1500);
  }

  #[test]
  fn test_alloc_sub_uid() {
    let mut used = HashSet::new();
    let prev = HashSet::new();
    let map = HashMap::new();
    let id = alloc_sub_uid("user", &mut used, &prev, &map).unwrap();
    assert_eq!(id, SUBUID_MIN);
    assert!(used.contains(&id));
  }

  #[test]
  fn test_alloc_sub_uid_revive() {
    let mut used = HashSet::new();
    let prev = HashSet::new();
    let mut map = HashMap::new();
    map.insert("user".to_string(), 200000);
    let id = alloc_sub_uid("user", &mut used, &prev, &map).unwrap();
    assert_eq!(id, 200000);
  }

  #[test]
  fn test_date_to_days() {
    // 1970-01-01 = day 0
    assert_eq!(date_to_days("1970-01-01").unwrap(), 0);
    // 1970-01-02 = day 1
    assert_eq!(date_to_days("1970-01-02").unwrap(), 1);
    // 2000-01-01 = 10957 days after epoch
    assert_eq!(date_to_days("2000-01-01").unwrap(), 10957);
  }

  #[test]
  fn test_date_to_days_invalid() {
    assert!(date_to_days("not-a-date").is_err());
    assert!(date_to_days("1969-12-31").is_err());
  }

  #[test]
  fn test_load_json_file_nonexistent() {
    let result: Result<HashMap<String, u32>> =
      load_json_file("/nonexistent/path.json");
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
  }

  #[test]
  fn test_load_declarative_list_nonexistent() {
    let result = load_declarative_list("/nonexistent/path");
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
  }

  #[test]
  fn test_update_file_dry_run() {
    let temp_path = "/tmp/test_nixos_core_update_file_dry";
    let result = update_file(temp_path, "content", 0o644, true);
    assert!(result.is_ok());
    assert!(!Path::new(temp_path).exists());
  }

  #[test]
  fn test_update_file_lines_trailing_newline() {
    // Verify the content that would be written has correct structure.
    // We test through update_file_lines by checking update_file receives
    // the correct content (indirectly, by testing on /tmp in a real write).
    use std::fs;
    let path = "/tmp/test_nixos_core_lines";
    update_file_lines(path, &["a".to_string(), "b".to_string()], 0o644, false)
      .unwrap();
    let content = fs::read_to_string(path).unwrap();
    assert_eq!(content, "a\nb\n");
    let _ = fs::remove_file(path);
  }
}
