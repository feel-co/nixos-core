{
  mkTest,
  nixosModule,
  testCommons,
}:
# mount(2) does not do mount(8)'s loop auto-setup.
mkTest {
  name = "nixos-core-loopmount";

  nodes.machine =
    { pkgs, ... }:
    let
      squashfsImage =
        pkgs.runCommand "loopmount-test.squashfs"
          {
            nativeBuildInputs = [ pkgs.squashfsTools ];
          }
          ''
            mkdir -p input
            echo "squashfs-loop-marker" > input/marker
            mksquashfs input "$out" -comp zstd -no-progress -all-root
          '';
    in
    {
      imports = [
        nixosModule
        testCommons
      ];
      system.nixos-core.enable = true;

      boot = {
        loader.grub.enable = false;
        initrd.systemd.enable = false;

        initrd.availableKernelModules = [
          "loop"
          "squashfs"
        ];
        initrd.kernelModules = [
          "loop"
          "squashfs"
        ];

        initrd.extraFiles."/loopmount-test.squashfs".source = squashfsImage;
      };

      virtualisation.fileSystems."/var/squash" = {
        device = "/loopmount-test.squashfs";
        fsType = "squashfs";
        options = [
          "loop"
          "ro"
        ];
        neededForBoot = true;
      };
    };

  # On failure, systemd waits on the missing mount. We keep that timeout short.
  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target", timeout=120)

    with subtest("squashfs mount succeeds against a regular-file source"):
      machine.succeed("mountpoint -q /var/squash")
      fstype = machine.succeed("findmnt -n -o FSTYPE /var/squash").strip()
      assert fstype == "squashfs", f"expected squashfs, got {fstype!r}"

    with subtest("squashfs contents are readable post-switch_root"):
      content = machine.succeed("cat /var/squash/marker").strip()
      assert content == "squashfs-loop-marker", f"got {content!r}"

    with subtest("no stage1 ENOTBLK or mount warning"):
      machine.fail("journalctl -b --no-pager | grep -E 'ENOTBLK|Block device required'")
      machine.fail("journalctl -b --no-pager | grep -F 'Warning: failed to mount'")
  '';
}
