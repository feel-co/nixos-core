{
  mkTest,
  nixosModule,
  testCommons,
}: let
  mkMachine = {withSystemd}: {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;

    boot = {
      loader.grub.enable = false;
      # For systemd test cases, we must force systemd initrd and thus not use
      # nixos-core's stage1. Systemd handles /run (tmpfs) and specialMount
      # skips /run when IN_NIXOS_SYSTEMD_STAGE1=true.
      initrd.systemd.enable = withSystemd;
    };
  };
in
  mkTest {
    name = "nixos-core-specialfs";
    nodes = {
      machine = mkMachine {withSystemd = false;};
      machineSystemd = mkMachine {withSystemd = true;};

      # Stage1 must create the bind-mount target as a file (not a directory)
      # when the source is a regular file. The source is baked into the initrd
      # CPIO via extraFiles (wired via initialRamdisk) so it is guaranteed
      # to be present when mount_additional_filesystems runs.
      fileBindMount = {pkgs, ...}: {
        imports = [nixosModule testCommons];
        system.nixos-core.enable = true;
        boot = {
          loader.grub.enable = false;
          initrd.systemd.enable = false;

          initrd.extraFiles."/bind-src".source =
            pkgs.writeText "bind-src-content" "file-bind-marker";
        };

        # qemu-vm.nix overrides fileSystems via mkVMOverride; use
        # virtualisation.fileSystems to survive that.
        virtualisation.fileSystems."/var/bound" = {
          device = "/bind-src";
          fsType = "none";
          options = ["bind"];
          neededForBoot = true;
        };
      };
    };

    testScript = ''
      machine.start()
      machineSystemd.start()
      machine.wait_for_unit("multi-user.target")
      machineSystemd.wait_for_unit("multi-user.target")

      # Scripted initrd.
      with subtest("/run/keys survives switch_root as its own ramfs mount"):
        machine.succeed("mountpoint -q /run/keys")
        fstype = machine.succeed("findmnt -n -o FSTYPE /run/keys").strip()
        assert fstype == "ramfs", f"expected /run/keys on ramfs, got {fstype!r}"

      with subtest("activation specialfs remounts did not fail"):
        machine.succeed("/run/current-system/activate")
        machine.fail(
          "journalctl -b --no-pager | grep -F 'mount point not mounted or bad option'"
        )
        machine.fail(
          "journalctl -b --no-pager | grep -F 'Activation script exited with code'"
        )

      # Systemd initrd.
      # /run is managed by systemd as tmpfs (not ramfs) and /run/keys is still a
      # separate mount (specialfs) via stage2 activation.
      with subtest("systemd-initrd: /run is a tmpfs managed by systemd"):
        machineSystemd.succeed("mountpoint -q /run")
        runFstype = machineSystemd.succeed("findmnt -n -o FSTYPE /run").strip()
        assert runFstype == "tmpfs", f"expected /run on tmpfs, got {runFstype!r}"

      with subtest("systemd-initrd: /run/keys is its own mount (specialfs)"):
        machineSystemd.succeed("test -d /run/keys")
        machineSystemd.succeed("mountpoint -q /run/keys")

      with subtest("systemd-initrd: activation specialfs remounts did not fail"):
        machineSystemd.succeed("/run/current-system/activate")
        machineSystemd.fail(
          "journalctl -b --no-pager | grep -F 'mount point not mounted or bad option'"
        )
        machineSystemd.fail(
          "journalctl -b --no-pager | grep -F 'Activation script exited with code'"
        )

      with subtest("systemd-initrd: systemd state passing from initrd"):
        machineSystemd.succeed("systemd-analyze | grep -q '(initrd)'")

      # File bind-mount. Start after the other VMs are done to avoid
      fileBindMount.start()
      fileBindMount.wait_for_unit("multi-user.target")

      with subtest("file bind-mount: target is a regular file, not a directory"):
        fileBindMount.succeed("test -f /var/bound")

      with subtest("file bind-mount: inode is reachable after switch_root"):
        content = fileBindMount.succeed("cat /var/bound").strip()
        assert content == "file-bind-marker", f"got {content!r}"

      with subtest("file bind-mount: no stage1 mount warnings"):
        fileBindMount.fail(
          "journalctl -b --no-pager | grep -F 'Warning: failed to mount'"
        )
    '';
  }
