self: {pkgs}: let
  mkTest = test:
    (pkgs.testers.runNixOSTest {
      imports = [test];

      defaults = {
        documentation.enable = pkgs.lib.mkDefault false;
        # virtio_pci must be loaded explicitly so udev can discover virtio_blk.
        boot.initrd.kernelModules = ["virtio_pci" "virtio_blk"];

        # The nixosTest default of 1 GiB / 1 core OOMs the backdoor shell
        # on CI and surfaces as "Shell disconnected".
        virtualisation.memorySize = 2048;
        virtualisation.cores = 2;
      };
    })
    .config
    .result;

  inherit (pkgs.lib.filesystem) packagesFromDirectoryRecursive;

  checks = packagesFromDirectoryRecursive {
    directory = ../vm-tests;
    callPackage = pkgs.newScope (checks
      // {
        inherit mkTest;
        nixosModule = self.nixosModules.default;
        testCommons = ./common.nix;
      });
  };
in
  checks
