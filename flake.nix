{
  inputs.nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    inherit (nixpkgs.lib) genAttrs systems;
    forEachSystem = genAttrs systems.doubles.linux;
    pkgsForEach = system: import nixpkgs { inherit system; };
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

    devShells = forEachSystem (system: {
      default = (pkgsForEach system).callPackage ./nix/shell.nix {};
    });
  };
}
