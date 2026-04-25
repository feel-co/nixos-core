{
  lib,
  rustPlatform,
  # Features to enable for nixos-core crate
  withUpdateUsersGroups ? true,
  withSetupEtc ? true,
  withInitScript ? true,
  withStage1 ? true,
  withStage2 ? true,
  # Features to enable for stage2 library
  withStage2Bootspec ? false,
  withStage2SystemdIntegration ? false,
}: let
  inherit (lib.lists) optional;
  nixosCoreFeatures =
    optional withUpdateUsersGroups "update-users-groups"
    ++ optional withSetupEtc "setup-etc"
    ++ optional withInitScript "init-script"
    ++ optional withStage1 "stage-1"
    ++ optional withStage2 "stage-2";

  stage2Features =
    optional withStage2Bootspec "stage2/bootspec"
    ++ optional withStage2SystemdIntegration "stage2/systemd-integration";

  allFeatures = nixosCoreFeatures ++ stage2Features;

  commands = lib.concatStringsSep " " (
    optional withUpdateUsersGroups "update-users-groups"
    ++ optional withSetupEtc "setup-etc"
    ++ optional withInitScript "init-script-builder"
    ++ optional withStage1 "stage-1-init"
    ++ optional withStage2 "stage-2-init"
  );
in
  rustPlatform.buildRustPackage {
    pname = "nixos-core";
    version = "26.05"; # TODO: version this

    src = let
      fs = lib.fileset;
      s = ../.;
    in
      fs.toSource {
        root = s;
        fileset = fs.unions [
          (s + /crates)
          (s + /nixos-core)
          (s + /Cargo.toml)
          (s + /Cargo.lock)
        ];
      };

    cargoLock = {
      lockFile = ../Cargo.lock;
      # FIXME: drop when smfh 1.5.0 is tagged
      outputHashes = {
        "smfh-core-1.4.0" = "sha256-8aLIC3bWQS+mb9sQnEB8utjJfwLMMIKQNYF2Lc6TfV8=";
      };
    };

    buildFeatures = allFeatures;

    installPhase = ''
      runHook preInstall

      mkdir -p $out/bin

      # Install nixos-core binary
      install -Dm755 \
        "$(find target -maxdepth 4 -path "*/release/nixos-core" -type f | head -1)" \
        $out/bin/nixos-core

      # Create symlinks for multi-call binary functionality
      for cmd in ${commands}; do
        ln -sfn nixos-core "$out/bin/$cmd"
      done

      runHook postInstall
    '';

    doCheck = false; # FIXME: make tests less flaky

    meta = {
      description = "Core NixOS system utilities";
      homepage = "https://github.com/feel-co/nixos-core";
      license = lib.licenses.mit;
      maintainers = with lib.maintainers; [NotAShelf];
      platforms = lib.platforms.linux;
      mainProgram = "nixos-core";
    };
  }
