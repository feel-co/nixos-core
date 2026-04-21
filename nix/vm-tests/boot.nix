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
    boot = {
      loader.grub.enable = false;
      # nixos-core's stage1 only runs with the scripted initrd; systemd initrd
      # would bypass it entirely and also forbids postMountCommands.
      initrd.systemd.enable = false;

      # Canary launched during stage1's postMountCommands. After switch_root,
      # stage1 calls kill_remaining_processes; a plain shell process (cmdline
      # does not start with '@') must be killed. /run is moved to the new root
      # via MS_MOVE so the pid-file survives into the booted system.
      initrd.postMountCommands = ''
        sh -c 'while true; do sleep 1; done' &
        echo $! > /run/canary.pid
        while [ ! -s /run/canary.pid ]; do sleep 0.1; done
      '';

      # Marker written by stage2's postBootCommands hook.
      postBootCommands = "touch /etc/post-boot-ran";
    };
    networking.hostId = "cafebabe";
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    with subtest("basic presence"):
      machine.succeed("test -f /etc/os-release")
      machine.succeed("test -e /etc/passwd")

    with subtest("stage1 kills regular initrd processes before switch_root"):
      machine.succeed("test -s /run/canary.pid")
      machine.fail("kill -0 $(cat /run/canary.pid) 2>/dev/null")

    with subtest("stage2 system symlinks point into the store"):
      machine.succeed("readlink /run/current-system | grep -q '^/nix/store/'")
      machine.succeed("readlink /run/booted-system  | grep -q '^/nix/store/'")
      machine.succeed("test /run/current-system -ef /run/booted-system")

    with subtest("nix store mount options"):
      for opt in ["ro", "nosuid", "nodev"]:
        machine.succeed(f'[[ "$(findmnt --direction backward --first-only --noheadings --output OPTIONS /nix/store)" =~ (^|,){opt}(,|$) ]]')
      machine.fail("touch /nix/store/should-not-work")

    with subtest("postBootCommands ran"):
      machine.succeed("test -f /etc/post-boot-ran")

    with subtest("HOST_ID written as 4 native-endian bytes"):
      machine.succeed("test -f /etc/hostid")
      machine.succeed("test $(wc -c < /etc/hostid) -eq 4")
      machine.succeed("od -An -tx1 /etc/hostid | tr -d ' \\n' | grep -qx 'bebafeca'")

    with subtest("stage1 wipes environment before exec /init"):
      # Upstream pivots with `exec env -i` so LD_LIBRARY_PATH=@extraUtils@/lib
      # doesn't leak into PID 1 and break systemd's libseccomp dlopen.
      machine.fail("tr '\\0' '\\n' < /proc/1/environ | grep -q '^LD_LIBRARY_PATH='")
      # Without seccomp, systemd drops the service PATH that resolves
      # relative ExecStart names, so tmpfiles-setup and friends 203/EXEC.
      machine.succeed("test -z \"$(systemctl --failed --no-legend)\"")
  '';
}
