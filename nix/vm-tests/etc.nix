{
  mkTest,
  nixosModule,
  testCommons,
  writeText,
}:
mkTest {
  name = "nixos-core-etc";

  nodes.machine = {
    imports = [nixosModule testCommons];
    system.nixos-core.enable = true;
    boot.loader.grub.enable = false;

    environment.etc = {
      "nixos-core-marker".text = "nixos-core-works";

      "nixos-core-secret" = {
        text = "sensitive";
        mode = "0600";
      };

      "nixos-core-source".source = writeText "etc-source" "from-source";

      "nixos-core-direct" = {
        source = writeText "etc-direct" "direct-content";
        mode = "direct-symlink";
      };
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("text entry symlinked through /etc/static"):
      machine.succeed("grep -qx nixos-core-works /etc/nixos-core-marker")
      machine.succeed("test -L /etc/nixos-core-marker")
      machine.succeed("readlink /etc/nixos-core-marker | grep -qx /etc/static/nixos-core-marker")

    with subtest("mode 0600 produces a copied file with correct permissions"):
      machine.succeed("grep -qx sensitive /etc/nixos-core-secret")
      machine.succeed("test ! -L /etc/nixos-core-secret")
      machine.succeed("stat -c '%a' /etc/nixos-core-secret | grep -qx 600")

    with subtest("source entry symlinked through /etc/static"):
      machine.succeed("grep -qx from-source /etc/nixos-core-source")
      machine.succeed("test -L /etc/nixos-core-source")
      machine.succeed("readlink /etc/nixos-core-source | grep -qx /etc/static/nixos-core-source")

    with subtest("direct-symlink bypasses /etc/static"):
      machine.succeed("grep -qx direct-content /etc/nixos-core-direct")
      machine.succeed("test -L /etc/nixos-core-direct")
      machine.succeed("readlink /etc/nixos-core-direct | grep -q '^/nix/store/'")
      machine.succeed("readlink /etc/nixos-core-direct | grep -qv /etc/static")

    with subtest("infrastructure files written"):
      machine.succeed("test -f /etc/.clean")
      machine.succeed("test -f /etc/NIXOS")

    with subtest("idempotent re-activation"):
      machine.execute("/run/current-system/activate")
      machine.succeed("grep -qx nixos-core-works /etc/nixos-core-marker")
      machine.succeed("grep -qx sensitive /etc/nixos-core-secret")
      machine.succeed("stat -c '%a' /etc/nixos-core-secret | grep -qx 600")
  '';
}
