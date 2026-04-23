{
  lib,
  mkTest,
  nixosModule,
  testCommons,
  writeText,
}:
mkTest ({nodes, ...}: {
  name = "nixos-core-etc";

  # Two nodes:
  #
  # - `machine` boots straight under nixos-core's setup-etc and exercises
  #   pass-through symlinks, copied files, source entries, direct symlinks,
  #   and idempotent re-activation.
  #
  # - `perl` boots under upstream's setup-etc.pl (nixos-core's etc activation
  #   disabled), then switches into a specialisation that re-enables
  #   nixos-core's setup-etc. Verifies the Perl-to-nixos-core migration:
  #   /etc/.clean is replayed, stale entries are dropped, surviving entries
  #   are preserved.
  #
  # TODO: see if we can do reboot subtests. As far as I understand vm tests run
  # with `boot.loader.grub.enable = false`, so a `shutdown` + `start` cycle
  # reboots the disk image into the same oplevel the test was built with. So
  # i the end there is no bootloader to redirect into the post-switch generation.
  # The boot test covers a fresh nixos-core boot end-to-end, which is the best
  # I can do :/
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

  nodes.perl = {
    imports = [nixosModule testCommons];

    # Bare `false` wins over the option's default (priority 1500) without
    # using mkForce, so the specialisation can mkForce true cleanly.
    system.nixos-core.enable = true;
    system.nixos-core.components.etcActivation.enable = false;
    boot.loader.grub.enable = false;

    environment.etc = {
      "perl-migration-marker".text = "from-perl";

      "perl-migration-secret" = {
        text = "secret-content";
        mode = "0600";
      };

      "perl-migration-direct" = {
        source = writeText "perl-migration-direct-src" "direct-from-perl";
        mode = "direct-symlink";
      };

      # Stale entries: present in the Perl-managed gen, dropped in the
      # nixos-core specialisation. The migration must remove both.
      "perl-migration-stale-symlink".text = "stale-symlink";

      "perl-migration-stale-copy" = {
        text = "stale-copy";
        mode = "0640";
      };
    };

    specialisation.nixos-core-etc.configuration = {
      system.nixos-core.components.etcActivation.enable = lib.mkForce true;
      environment.etc."perl-migration-stale-symlink".enable = lib.mkForce false;
      environment.etc."perl-migration-stale-copy".enable = lib.mkForce false;
    };
  };

  testScript = ''
    start_all()
    machine.wait_for_unit("multi-user.target")
    perl.wait_for_unit("multi-user.target")

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
      machine.succeed("test -f /var/lib/nixos/etc-manifest.json")
      machine.succeed("test -f /etc/NIXOS")

    with subtest("manifest contains expected entries"):
      machine.succeed("grep -q nixos-core-marker /var/lib/nixos/etc-manifest.json")
      machine.succeed("grep -q nixos-core-secret /var/lib/nixos/etc-manifest.json")
      machine.succeed("grep -q nixos-core-source /var/lib/nixos/etc-manifest.json")
      machine.succeed("grep -q nixos-core-direct /var/lib/nixos/etc-manifest.json")

    with subtest("idempotent re-activation"):
      machine.execute("/run/current-system/activate")
      machine.succeed("grep -qx nixos-core-works /etc/nixos-core-marker")
      machine.succeed("grep -qx sensitive /etc/nixos-core-secret")
      machine.succeed("stat -c '%a' /etc/nixos-core-secret | grep -qx 600")
      machine.succeed("test -f /var/lib/nixos/etc-manifest.json")

    ## Perl-to-nixos-core migration
    with subtest("Perl-based activation populated /etc"):
      perl.succeed("test -f /etc/.clean")
      perl.fail("test -e /var/lib/nixos/etc-manifest.json")
      perl.succeed("grep -q perl-migration-secret /etc/.clean")
      perl.succeed("grep -q perl-migration-stale-copy /etc/.clean")

    with subtest("pass-through symlink under Perl"):
      perl.succeed("grep -qx from-perl /etc/perl-migration-marker")
      perl.succeed("test -L /etc/perl-migration-marker")
      perl.succeed(
        "readlink /etc/perl-migration-marker | grep -qx /etc/static/perl-migration-marker"
      )

    with subtest("copied file under Perl"):
      perl.succeed("grep -qx secret-content /etc/perl-migration-secret")
      perl.succeed("test ! -L /etc/perl-migration-secret")
      perl.succeed("stat -c '%a' /etc/perl-migration-secret | grep -qx 600")

    with subtest("direct symlink under Perl"):
      perl.succeed("grep -qx direct-from-perl /etc/perl-migration-direct")
      perl.succeed("test -L /etc/perl-migration-direct")
      perl.succeed("readlink /etc/perl-migration-direct | grep -q '^/nix/store/'")

    with subtest("stale entries present under Perl"):
      perl.succeed("test -e /etc/perl-migration-stale-symlink")
      perl.succeed("test -e /etc/perl-migration-stale-copy")

    nixos_core = "${nodes.perl.system.build.toplevel}/specialisation/nixos-core-etc"

    with subtest("switch to nixos-core's setup-etc"):
      perl.succeed(f"{nixos_core}/bin/switch-to-configuration switch")

    with subtest("Perl state file removed, manifest written"):
      perl.fail("test -e /etc/.clean")
      perl.succeed("test -f /var/lib/nixos/etc-manifest.json")

    with subtest("pass-through symlink survives migration"):
      perl.succeed("grep -qx from-perl /etc/perl-migration-marker")
      perl.succeed("test -L /etc/perl-migration-marker")
      perl.succeed(
        "readlink /etc/perl-migration-marker | grep -qx /etc/static/perl-migration-marker"
      )

    with subtest("copied file survives migration with mode preserved"):
      perl.succeed("grep -qx secret-content /etc/perl-migration-secret")
      perl.succeed("test ! -L /etc/perl-migration-secret")
      perl.succeed("stat -c '%a' /etc/perl-migration-secret | grep -qx 600")

    with subtest("direct symlink survives migration"):
      perl.succeed("grep -qx direct-from-perl /etc/perl-migration-direct")
      perl.succeed("test -L /etc/perl-migration-direct")
      perl.succeed("readlink /etc/perl-migration-direct | grep -q '^/nix/store/'")

    with subtest("stale Perl-era entries removed after migration"):
      perl.fail("test -e /etc/perl-migration-stale-symlink")
      perl.fail("test -e /etc/perl-migration-stale-copy")

    with subtest("manifest mentions migrated entries"):
      perl.succeed("grep -q perl-migration-marker /var/lib/nixos/etc-manifest.json")
      perl.succeed("grep -q perl-migration-secret /var/lib/nixos/etc-manifest.json")
      perl.succeed("grep -q perl-migration-direct /var/lib/nixos/etc-manifest.json")

    with subtest("idempotent re-activation under nixos-core"):
      perl.execute("/run/current-system/activate")
      perl.fail("test -e /etc/.clean")
      perl.succeed("test -f /var/lib/nixos/etc-manifest.json")
      perl.succeed("grep -qx from-perl /etc/perl-migration-marker")
      perl.succeed("grep -qx secret-content /etc/perl-migration-secret")
      perl.succeed("stat -c '%a' /etc/perl-migration-secret | grep -qx 600")
  '';
})
