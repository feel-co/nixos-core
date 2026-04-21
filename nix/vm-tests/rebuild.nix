{
  mkTest,
  nixosModule,
  testCommons,
}:
mkTest ({nodes, ...}: {
  name = "nixos-core-rebuild";

  nodes.machine = {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;
    boot.loader.grub.enable = false;

    specialisation.gen2.configuration = {
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
  '';
})
