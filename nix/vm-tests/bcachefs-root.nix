{
  mkTest,
  nixosModule,
  ...
}:
let
  # This is pinned so `bcachefs format --uuid=...` and fileSystems."/".device agree at eval time.
  bcachefsUuid = "fef71188-998b-4a00-a263-6b525fe9832b";
in
mkTest {
  name = "nixos-core-bcachefs-root";

  nodes.machine = {
    imports = [ nixosModule ];
    system.nixos-core.enable = true;
    system.stateVersion = "26.05";

    boot = {
      loader.grub.enable = false;
      initrd.systemd.enable = false;
      supportedFilesystems = [ "bcachefs" ];

      initrd.postDeviceCommands = ''
        if ! bcachefs show-super /dev/vdb >/dev/null 2>&1; then
          bcachefs format --force \
            --uuid=${bcachefsUuid} \
            --metadata_replicas 2 \
            --label=ssd /dev/vdb \
            --label=hdd /dev/vdc
        fi
      '';
    };

    virtualisation = {
      # Otherwise qemu-vm.nix synthesises an ext4 "/" on /dev/vda that
      # collides with our bcachefs root below.
      useDefaultFilesystems = false;
      emptyDiskImages = [
        1024
        1024
      ];
      fileSystems."/" = {
        device = "UUID=${bcachefsUuid}";
        fsType = "bcachefs";
        neededForBoot = true;
        options = [ "noatime" ];
      };
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("/ is bcachefs"):
      fstype = machine.succeed("findmnt -n -o FSTYPE /").strip()
      assert fstype == "bcachefs", f"got {fstype!r}"

    with subtest("root UUID matches config"):
      uuid = machine.succeed(
        "bcachefs show-super /dev/vdb | awk '/External UUID/ {print $3}'"
      ).strip()
      assert uuid == "${bcachefsUuid}", f"got {uuid!r}"

    with subtest("both members attached"):
      usage = machine.succeed("bcachefs fs usage /")
      for dev in ["vdb", "vdc"]:
        assert dev in usage, f"{dev} missing:\n{usage}"

    with subtest("noatime applied"):
      opts = machine.succeed("findmnt -n -o OPTIONS /").strip()
      assert "noatime" in opts, f"options: {opts!r}"

    with subtest("root is writable"):
      machine.succeed("touch /root-marker && test -f /root-marker")

    with subtest("/etc activation ran"):
      machine.succeed("test -L /etc/static")
      machine.succeed("readlink /etc/static | grep -q '^/nix/store/'")

    with subtest("existing bcachefs root remounts across reboot"):
      machine.shutdown()
      machine.start()
      machine.wait_for_unit("multi-user.target")
      fstype = machine.succeed("findmnt -n -o FSTYPE /").strip()
      assert fstype == "bcachefs"
      machine.succeed("test -f /root-marker")
  '';
}
