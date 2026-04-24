{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest {
  name = "nixos-core-fs";

  # For bcachefs, stage1 splits the colon-separated device string and waits for
  # each member individually
  nodes.machine = {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;

    boot = {
      loader.grub.enable = false;
      initrd.systemd.enable = false;
      supportedFilesystems = ["bcachefs"];

      initrd.postDeviceCommands = ''
        if ! bcachefs show-super /dev/vdb >/dev/null 2>&1; then
          bcachefs format --force \
            --metadata_replicas 3 \
            --label=disk0 /dev/vdb \
            --label=disk1 /dev/vdc \
            --label=disk2 /dev/vdd
        fi
      '';
    };

    virtualisation.emptyDiskImages = [
      512
      512
      512
    ];

    # qemu-vm.nix replaces fileSystems via mkVMOverride.
    virtualisation.fileSystems."/mnt/bcache3" = {
      device = "/dev/vdb:/dev/vdc:/dev/vdd";
      fsType = "bcachefs";
      neededForBoot = true;
      options = ["noatime"];
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("three-device bcachefs mounts"):
      machine.succeed("mountpoint -q /mnt/bcache3")
      fstype = machine.succeed("findmnt -n -o FSTYPE /mnt/bcache3").strip()
      assert fstype == "bcachefs", f"expected bcachefs, got {fstype!r}"

    with subtest("all three members attached"):
      usage = machine.succeed("bcachefs fs usage /mnt/bcache3")
      for dev in ["vdb", "vdc", "vdd"]:
        assert dev in usage, f"{dev} missing from usage:\n{usage}"

    with subtest("noatime forwarded"):
      opts = machine.succeed("findmnt -n -o OPTIONS /mnt/bcache3").strip()
      assert "noatime" in opts, f"options: {opts!r}"

    with subtest("filesystem is writable and survives reboot"):
      machine.succeed("echo hello > /mnt/bcache3/marker")
      machine.shutdown()
      machine.start()
      machine.wait_for_unit("multi-user.target")
      machine.succeed("mountpoint -q /mnt/bcache3")
      machine.succeed("grep -qx hello /mnt/bcache3/marker")
  '';
}
