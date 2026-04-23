{
  lib,
  mkTest,
  nixosModule,
  testCommons,
  writeText,
}:
mkTest ({nodes, ...}: {
  name = "nixos-core-rebuild";

  nodes.machine = {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;
    boot.loader.grub.enable = false;

    environment.etc = {
      # A plain text entry present in gen1 only; used to verify stale-entry
      # removal when switching forward to gen2.
      "nixos-core-gen1-marker".text = "generation-1";

      # A direct-symlink entry in gen1; must also be cleaned up on gen switch.
      "nixos-core-gen1-direct" = {
        source = writeText "gen1-direct-content" "gen1-direct";
        mode = "direct-symlink";
      };
    };

    specialisation.gen2.configuration = {
      # Specialisations merge with the base config, so we must explicitly
      # disable the gen1-only entries so they are absent from the gen2 etc
      # store derivation and therefore removed by the manifest diff.
      environment.etc."nixos-core-gen1-marker".enable = lib.mkForce false;
      environment.etc."nixos-core-gen1-direct".enable = lib.mkForce false;
      environment.etc."nixos-core-gen2-marker".text = "generation-2";
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("gen1 booted"):
      machine.succeed("readlink /run/current-system | grep -q '^/nix/store/'")
      machine.succeed("readlink /run/booted-system  | grep -q '^/nix/store/'")
      machine.succeed("test /run/current-system -ef /run/booted-system")
      machine.succeed("grep -qx generation-1 /etc/nixos-core-gen1-marker")
      machine.fail("test -f /etc/nixos-core-gen2-marker")

    gen1_toplevel = machine.succeed("readlink /run/current-system").strip()
    gen2 = "${nodes.machine.system.build.toplevel}/specialisation/gen2"

    with subtest("switch to generation 2"):
      machine.succeed(f"{gen2}/bin/switch-to-configuration switch")

    with subtest("current-system changed to gen2 toplevel"):
      gen2_toplevel = machine.succeed("readlink /run/current-system").strip()
      assert gen2_toplevel != gen1_toplevel, \
        f"current-system was not updated: still {gen2_toplevel}"
      machine.succeed("readlink /run/current-system | grep -q '^/nix/store/'")

    with subtest("nixos-core /etc activation applied gen2 file"):
      machine.succeed("test -f /etc/nixos-core-gen2-marker")
      machine.succeed("grep -qx generation-2 /etc/nixos-core-gen2-marker")

    with subtest("nixos-core /etc static symlink still valid"):
      machine.succeed("test -L /etc/static")
      machine.succeed("readlink /etc/static | grep -q '^/nix/store/'")

    with subtest("gen1-only entries removed after forward switch"):
      # The manifest diff must have deactivated files absent from gen2.
      machine.fail("test -e /etc/nixos-core-gen1-marker")
      machine.fail("test -e /etc/nixos-core-gen1-direct")

    with subtest("rollback to generation 1"):
      machine.succeed(f"{gen1_toplevel}/bin/switch-to-configuration switch")

    with subtest("current-system rolled back to gen1 toplevel"):
      rolled_back = machine.succeed("readlink /run/current-system").strip()
      assert rolled_back == gen1_toplevel, \
        f"rollback did not restore gen1: got {rolled_back}"

    with subtest("gen2-only entry removed after rollback"):
      machine.fail("test -e /etc/nixos-core-gen2-marker")

    with subtest("gen1 entries restored after rollback"):
      machine.succeed("grep -qx generation-1 /etc/nixos-core-gen1-marker")
  '';
})
