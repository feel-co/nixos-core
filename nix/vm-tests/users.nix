{
  mkTest,
  nixosModule,
  testCommons,
  bash,
}: let
  # sha-512 hashes used by the mutableUsers/hashedPassword contract subtests.
  # hashA is the spec hash; hashB simulates a passwd(1) edit done out-of-band.
  hashA = "$6$tBd0y.v0jtG7SpqS$3YJGM9Hk.oMsGH6.v6MdW8kzFJg/zphs8S/o6PpTfc8j2QsF7LIJjLbxdP4cxc3aJlG7U8zghdrQzFZRbpwGS0";
  hashB = "$6$mU7oGq7HZFH.u41v$W.qZfFFHVhTxwh8U7vRs5ToqPGJgO.i06y6cEw6T/GNMj.NcChZrEomTM8DODE3C5x2Atl0WqIOg/LU4Nll4n0";
in
  mkTest {
    name = "nixos-core-users";

    nodes = {
      # Primary node for the full user/group contract suite. mutableUsers is
      # false here so it also serves as the immutable side of the
      # hashedPassword contract test below.
      machine = {
        imports = [nixosModule testCommons];
        system.nixos-core.enable = true;
        boot.loader.grub.enable = false;

        users = {
          mutableUsers = false;

          groups = {
            testgroup.gid = 1500;
            extragroup.gid = 1501;
            sysuser = {};
          };

          users = {
            testuser = {
              isNormalUser = true;
              uid = 1500;
              group = "testgroup";
              description = "Test User";
            };

            lockeduser = {
              isNormalUser = true;
              hashedPassword = "!";
            };

            hashuser = {
              isNormalUser = true;
              password = "testpassword";
            };

            homeuser = {
              isNormalUser = true;
              createHome = true;
              home = "/home/homeuser";
              homeMode = "0750";
            };

            shelluser = {
              isNormalUser = true;
              shell = bash;
            };

            groupmember = {
              isNormalUser = true;
              extraGroups = ["extragroup"];
            };

            sysuser = {
              isSystemUser = true;
              group = "sysuser";
            };

            subuser = {
              isNormalUser = true;
              autoSubUidGidRange = true;
            };

            immutableuser = {
              isNormalUser = true;
            };

            # Shared with the hashedPassword contract subtests below.
            alice = {
              isNormalUser = true;
              hashedPassword = hashA;
            };
          };
        };
      };

      # Minimal node used exclusively for the mutableUsers=true side of the
      # hashedPassword contract: activation must NOT clobber a hash the user
      # set interactively via passwd(1) or chpasswd.
      mutable = {
        imports = [nixosModule testCommons];
        system.nixos-core.enable = true;
        boot.loader.grub.enable = false;
        users.mutableUsers = true;
        users.users.alice = {
          isNormalUser = true;
          hashedPassword = hashA;
        };
      };
    };

    testScript = ''
      def shadow(m, user):
          return m.succeed(f"getent shadow {user}").strip().split(":")[1]

      start_all()
      machine.wait_for_unit("multi-user.target")
      mutable.wait_for_unit("multi-user.target")

      with subtest("normal user and group"):
        machine.succeed("id testuser")
        machine.succeed("getent passwd testuser | grep -q 'Test User'")
        machine.succeed("[ $(id -u testuser) -eq 1500 ]")
        machine.succeed("getent group testgroup")
        machine.succeed("getent group testgroup | cut -d: -f3 | grep -qx 1500")

      with subtest("shadow permissions"):
        machine.succeed("stat -c '%a' /etc/shadow | grep -qx 640")

      with subtest("locked account"):
        machine.succeed("grep '^lockeduser:' /etc/shadow | cut -d: -f2 | grep -qx '!'")

      with subtest("password hashed at activation"):
        machine.succeed("grep '^hashuser:' /etc/shadow | cut -d: -f2 | grep -qv '^!'")
        machine.succeed("grep '^hashuser:' /etc/shadow | cut -d: -f2 | grep -qv '^$'")

      with subtest("home directory created with correct mode and ownership"):
        machine.succeed("test -d /home/homeuser")
        machine.succeed("stat -c '%a' /home/homeuser | grep -qx 750")
        machine.succeed("stat -c '%U' /home/homeuser | grep -qx homeuser")

      with subtest("shell written to passwd"):
        machine.succeed("getent passwd shelluser | cut -d: -f7 | grep -q bash")

      with subtest("extraGroups membership"):
        machine.succeed("getent group extragroup | cut -d: -f4 | grep -q groupmember")

      with subtest("system user uid range"):
        machine.succeed("id sysuser")
        machine.succeed("[ $(id -u sysuser) -ge 400 ] && [ $(id -u sysuser) -le 999 ]")

      with subtest("sub-UID and sub-GID allocation"):
        machine.succeed("grep -qE '^subuser:[0-9]+:65536$' /etc/subuid")
        machine.succeed("grep -qE '^subuser:[0-9]+:65536$' /etc/subgid")

      with subtest("mutableUsers=false reverts external password changes"):
        machine.succeed("sed -i 's/^immutableuser:!:/immutableuser:changed:/' /etc/shadow")
        machine.succeed("grep '^immutableuser:' /etc/shadow | cut -d: -f2 | grep -qx changed")
        machine.execute("/run/current-system/activate")
        machine.succeed("grep '^immutableuser:' /etc/shadow | cut -d: -f2 | grep -qx '!'")

      # hashedPassword contract: in mutableUsers=true mode, activation must
      # not clobber a hash set interactively; in mutableUsers=false mode it
      # must restore the spec hash over any local edit.
      with subtest("hashedPassword: initial spec hash applied on both nodes"):
        assert shadow(machine, "alice") == "${hashA}", \
            "machine: initial activation did not apply spec hash"
        assert shadow(mutable, "alice") == "${hashA}", \
            "mutable: initial activation did not apply spec hash"

      with subtest("hashedPassword: simulate passwd(1) edit on both nodes"):
        for m in (machine, mutable):
            m.succeed(
                "sed -i 's|^alice:[^:]*:|alice:${hashB}:|' /etc/shadow"
            )
            assert shadow(m, "alice") == "${hashB}", \
                f"{m.name}: shadow edit failed"

      with subtest("hashedPassword: re-activation respects mutableUsers contract"):
        for m in (machine, mutable):
            m.succeed("/run/current-system/activate")

        mutable_hash = shadow(mutable, "alice")
        assert mutable_hash == "${hashB}", (
            "mutableUsers=true: activation clobbered the interactively-set "
            f"password; expected {'${hashB}'!r}, got {mutable_hash!r}"
        )

        immutable_hash = shadow(machine, "alice")
        assert immutable_hash == "${hashA}", (
            "mutableUsers=false: activation failed to restore spec hash; "
            f"expected {'${hashA}'!r}, got {immutable_hash!r}"
        )
    '';
  }
