{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest {
  name = "nixos-core-systemd-initrd";

  nodes.machine = {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;
    boot = {
      loader.grub.enable = false;

      # Force systemd initrd; nixos-core's stage1 is a no-op here. Systemd basically
      # owns all of stage1, but stage2/activation/etc components still apply.
      initrd.systemd.enable = true;

      # Marker written by stage2's postBootCommands hook.
      postBootCommands = "touch /etc/post-boot-ran";
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("basic presence"):
      machine.succeed("test -f /etc/os-release")
      machine.succeed("test -e /etc/passwd")

    with subtest("stage2 system symlinks point into the store"):
      machine.succeed("readlink /run/current-system | grep -q '^/nix/store/'")
      machine.succeed("readlink /run/booted-system  | grep -q '^/nix/store/'")
      machine.succeed("test /run/current-system -ef /run/booted-system")

    with subtest("nix store mount options"):
      for opt in ["ro", "nosuid", "nodev"]:
        machine.succeed(f'[[ "$(findmnt --direction backward --first-only --noheadings --output OPTIONS /nix/store)" =~ (^|,){opt}(,|$) ]]')
      machine.fail("touch /nix/store/should-not-work")

    with subtest("postBootCommands ran"):
      machine.succeed("test -f /etc/post-boot-ran")

    with subtest("nixos-core /etc activation ran"):
      # setup-etc atomically creates /etc/static -> /nix/store/..-etc first;
      # all other /etc symlinks are passthrough links into /etc/static.
      machine.succeed("test -L /etc/static")
      machine.succeed("readlink /etc/static | grep -q '^/nix/store/'")
      machine.succeed("readlink -f /etc/os-release | grep -q '^/nix/store/'")

    with subtest("nixos-core user/group activation ran"):
      machine.succeed("test -f /etc/passwd")
      machine.succeed("test -f /etc/shadow")
      machine.succeed("stat -c '%a' /etc/shadow | grep -qx 640")
  '';
}
