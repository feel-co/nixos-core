{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest {
  name = "nixos-core-boot";

  nodes.machine = {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;
    boot.loader.grub.enable = false;
    networking.hostId = "cafebabe";
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

    with subtest("HOST_ID written as 4 native-endian bytes"):
      machine.succeed("test -f /etc/hostid")
      machine.succeed("test $(wc -c < /etc/hostid) -eq 4")
      machine.succeed("od -An -tx1 /etc/hostid | tr -d ' \\n' | grep -qx 'bebafeca'")
  '';
}
