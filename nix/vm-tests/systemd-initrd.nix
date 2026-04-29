{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest ({nodes, ...}: {
  name = "nixos-core-systemd-initrd";

  nodes = {
    machine = {
      imports = [nixosModule testCommons];
      system.nixos-core.enable = true;
      boot = {
        loader.grub.enable = false;

        # Force systemd initrd; nixos-core's stage1 is a no-op here.
        # Systemd owns stage1, but stage2/activation/etc still apply.
        initrd.systemd.enable = true;

        # Marker written by stage2's postBootCommands hook.
        postBootCommands = "touch /etc/post-boot-ran";
      };
    };

    readOnlySysroot = {
      imports = [nixosModule testCommons];
      system.nixos-core.enable = true;
      boot = {
        loader.grub.enable = false;
        initrd.systemd.enable = true;

        postBootCommands = ''
          findmnt --direction backward --first-only --noheadings --output OPTIONS / \
            > /etc/stage2-root-options
          touch /etc/post-boot-ran
        '';

        initrd.systemd.services.nixos-core-remount-sysroot-ro = {
          after = ["sysroot.mount" "systemd-tmpfiles-setup-sysroot.service"];
          before = ["initrd-nixos-activation.service"];
          requiredBy = ["initrd-nixos-activation.service"];
          unitConfig.DefaultDependencies = false;
          serviceConfig.Type = "oneshot";
          script = ''
            /bin/mount -o remount,ro /sysroot
          '';
        };
      };
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

    with subtest("current-system points to exact toplevel"):
      machine.succeed(
        "test \"$(readlink /run/current-system)\" = \"${nodes.machine.system.build.toplevel}\""
      )

    with subtest("firmware search path written"):
      machine.succeed("test -f /sys/module/firmware_class/parameters/path")
      machine.succeed(
        "grep -qF /lib/firmware /sys/module/firmware_class/parameters/path"
      )

    with subtest("modprobe binary written to /proc/sys/kernel/modprobe"):
      machine.succeed(
        "grep -qF modprobe /proc/sys/kernel/modprobe"
      )

    with subtest("FHS compatibility symlinks present"):
      machine.succeed("test -L /usr/bin/env")
      machine.succeed("test -L /bin/sh")

    with subtest("systemd state passing from initrd"):
      machine.succeed("systemd-analyze | grep -q '(initrd)'")

    readOnlySysroot.start()
    readOnlySysroot.wait_for_unit("multi-user.target")

    with subtest("read-only sysroot: postBootCommands ran"):
      readOnlySysroot.succeed("test -f /etc/post-boot-ran")

    with subtest("read-only sysroot: stage2 remounted root rw"):
      readOnlySysroot.succeed('grep -Eq "(^|,)rw(,|$)" /etc/stage2-root-options')
      readOnlySysroot.fail('grep -Eq "(^|,)ro(,|$)" /etc/stage2-root-options')
  '';
})
