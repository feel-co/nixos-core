self: {pkgs}: let
  mkTest = test:
    (pkgs.testers.runNixOSTest {
      imports = [test];

      defaults = {
        documentation.enable = pkgs.lib.mkDefault false;
        # virtio_pci must be loaded explicitly so udev can discover virtio_blk.
        boot.initrd.kernelModules = ["virtio_pci" "virtio_blk"];
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
