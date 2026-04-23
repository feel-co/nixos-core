{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest {
  name = "nixos-core-specialfs";

  nodes.machine = {
    imports = [
      nixosModule
      testCommons
    ];
    system.nixos-core.enable = true;

    boot = {
      loader.grub.enable = false;
      initrd.systemd.enable = false;
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

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
  '';
}
