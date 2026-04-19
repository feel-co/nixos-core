{
  inputs.nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forEachSystem = nixpkgs.lib.genAttrs systems;
    pkgsForEach = system: nixpkgs.legacyPackages.${system};
  in {
    nixosModules = {
      nixos-core = import ./nix/modules/nixos.nix self;
      default = self.nixosModules.nixos-core;
    };

    packages = forEachSystem (system: {
      nixos-core = (pkgsForEach system).callPackage ./nix/package.nix {};
      default = self.packages.${system}.nixos-core;
    });

    devShells = forEachSystem (system: {
      default = (pkgsForEach system).callPackage ./nix/shell.nix {};
    });
  };
}
