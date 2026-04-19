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

    checks = forEachSystem (system: let
      pkgs = pkgsForEach system;
    in
      import ./nix/checks self {inherit pkgs;});

    packages = forEachSystem (system: {
      nixos-core = (pkgsForEach system).callPackage ./nix/package.nix {};
      default = self.packages.${system}.nixos-core;
    });

    checks = forEachSystem (system: {
      mutable-users = import ./nix/tests/mutable-users.nix self {
        pkgs = pkgsForEach system;
      };
    });

    devShells = forEachSystem (system: {
      default = (pkgsForEach system).callPackage ./nix/shell.nix {};
    });
  };
}
