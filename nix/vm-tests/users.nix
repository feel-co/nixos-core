{
  mkTest,
  nixosModule,
  testCommons,
  bash,
}:
mkTest {
  name = "nixos-core-users";

  nodes.machine = {
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
      };
    };
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

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
  '';
}
