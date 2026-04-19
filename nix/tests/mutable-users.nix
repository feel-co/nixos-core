# VM test reinforcing the mutableUsers hashedPassword contract implemented by
# update-users-groups (crates/update-users-groups/src/lib.rs).
#
# Two halves mirroring the Perl script:
#
#   mutableUsers = true  -> activation must NOT clobber shadow entries the
#   user set interactively (passwd(1) / chpasswd). This is the behavioural
#   change called out in NotAShelf's review of ut (update-users-groups:
#   preserve passwd(1)-set hashes in mutable mode).
#
#   mutableUsers = false -> activation MUST re-apply the spec's
#   hashedPassword, clobbering any local change.
self: {pkgs, ...}: let
  # Two pre-computed SHA-512 crypt hashes; the values are irrelevant beyond
  # being valid and distinct.
  hashA = "$6$tBd0y.v0jtG7SpqS$3YJGM9Hk.oMsGH6.v6MdW8kzFJg/zphs8S/o6PpTfc8j2QsF7LIJjLbxdP4cxc3aJlG7U8zghdrQzFZRbpwGS0";
  hashB = "$6$mU7oGq7HZFH.u41v$W.qZfFFHVhTxwh8U7vRs5ToqPGJgO.i06y6cEw6T/GNMj.NcChZrEomTM8DODE3C5x2Atl0WqIOg/LU4Nll4n0";

  mkCommon = mutable: {
    system.nixos-core.enable = true;
    users.mutableUsers = mutable;
    users.users.alice = {
      isNormalUser = true;
      hashedPassword = hashA;
    };
  };
in
  pkgs.testers.nixosTest {
    name = "nixos-core-mutable-users-hashedPassword";

    nodes = {
      mutable = {
        imports = [self.nixosModules.nixos-core (mkCommon true)];
      };
      immutable = {
        imports = [self.nixosModules.nixos-core (mkCommon false)];
      };
    };

    testScript = ''
      def shadow(m, user):
          return m.succeed(f"getent shadow {user}").strip().split(":")[1]

      start_all()

      for m in (mutable, immutable):
          m.wait_for_unit("multi-user.target")
          assert shadow(m, "alice") == "${hashA}", \
              f"initial activation did not apply spec hash on {m.name}"

      # Simulate passwd(1): rewrite the shadow hash out-of-band.
      for m in (mutable, immutable):
          m.succeed(
              "sed -i 's|^alice:[^:]*:|alice:${hashB}:|' /etc/shadow"
          )
          assert shadow(m, "alice") == "${hashB}", "shadow edit failed"

      # Re-run update-users-groups; system config is unchanged so the only
      # variable is mutableUsers. /run/current-system/activate runs the whole
      # activation script, which invokes update-users-groups.
      for m in (mutable, immutable):
          m.succeed("/run/current-system/activate")

      mutable_hash = shadow(mutable, "alice")
      assert mutable_hash == "${hashB}", (
          "mutableUsers=true: activation clobbered the interactively-set "
          f"password; expected {'${hashB}'!r}, got {mutable_hash!r}"
      )

      immutable_hash = shadow(immutable, "alice")
      assert immutable_hash == "${hashA}", (
          "mutableUsers=false: activation failed to restore spec hash; "
          f"expected {'${hashA}'!r}, got {immutable_hash!r}"
      )
    '';
  }
