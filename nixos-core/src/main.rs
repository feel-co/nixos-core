use std::{env, path::Path};

use anyhow::{Result, bail};

fn main() -> Result<()> {
  let all_args: Vec<String> = env::args().collect();

  let (command, handler_args): (&str, &[String]) = {
    let argv0_base = Path::new(&all_args[0])
      .file_stem()
      .and_then(|s| s.to_str())
      .unwrap_or("nixos-core");

    if argv0_base != "nixos-core" {
      (argv0_base, &all_args[..])
    } else if all_args.len() >= 2 {
      (all_args[1].as_str(), &all_args[1..])
    } else {
      print_usage();
      bail!("No command specified. Usage: nixos-core <command> [args...]");
    }
  };

  dispatch(command, handler_args)
}

fn dispatch(command: &str, args: &[String]) -> Result<()> {
  match command {
    #[cfg(feature = "update-users-groups")]
    "update-users-groups" => update_users_groups::run(args),

    #[cfg(feature = "setup-etc")]
    "setup-etc" => setup_etc::run(args),

    #[cfg(feature = "init-script")]
    "init-script" | "init-script-builder" => init_script::run(args),

    #[cfg(feature = "stage-1")]
    "stage-1-init" => stage1::run(args),

    #[cfg(feature = "stage-2")]
    "stage-2-init" => stage2::run_from_args_and_handoff(args),

    _ => {
      eprintln!("Unknown command: {command}");
      print_usage();
      bail!("Unknown command: {command}");
    },
  }
}

fn print_usage() {
  eprintln!("Available commands:");
  #[cfg(feature = "update-users-groups")]
  eprintln!(
    "  update-users-groups   Manage /etc/passwd, /etc/group, /etc/shadow"
  );
  #[cfg(feature = "setup-etc")]
  eprintln!("  setup-etc             Atomically apply /etc from /etc/static");
  #[cfg(feature = "init-script")]
  eprintln!(
    "  init-script           Create the generic /sbin/init fallback script"
  );
  #[cfg(feature = "stage-1")]
  eprintln!(
    "  stage-1-init          Initrd bootstrap (mounts, udev, LUKS, fsck)"
  );
  #[cfg(feature = "stage-2")]
  eprintln!("  stage-2-init          Activation and systemd handoff");
}
