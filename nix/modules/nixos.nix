self: {
  config,
  pkgs,
  lib,
  ...
}: let
  udev = config.systemd.package;
  extra-utils = config.system.build.extraUtils;
  useHostResolvConf = config.networking.resolvconf.enable && config.networking.useHostResolvConf;

  cfg = config.system.nixos-core;

  # Inlined from nixpkgs nixos/lib/utils.nix since those are not available in out-of-tree modules.
  pathsNeededForBoot = [
    "/"
    "/nix"
    "/nix/store"
    "/var"
    "/var/log"
    "/var/lib"
    "/var/lib/nixos"
    "/etc"
    "/usr"
  ];
  fsNeededForBoot = fs: fs.neededForBoot || builtins.elem fs.mountPoint pathsNeededForBoot;
  toShellPath = shell:
    if lib.types.shellPackage.check shell
    then "/run/current-system/sw${shell.shellPath}"
    else if lib.types.package.check shell
    then throw "${shell} is not a shell package"
    else shell;

  # linkUnits and udevRules are private to nixpkgs' stage-1.nix; reproduced here.
  linkUnits =
    pkgs.runCommand "link-units" {
      allowedReferences = [extra-utils];
      preferLocalBuild = true;
    } (''
        mkdir -p $out
        cp -v ${udev}/lib/systemd/network/*.link $out/
      ''
      + (
        let
          links = lib.filterAttrs (n: _: lib.hasSuffix ".link" n) config.systemd.network.units;
          files = lib.mapAttrsToList (n: v: "${v.unit}/${n}") links;
        in
          lib.concatMapStringsSep "\n" (f: "cp -v ${f} $out/") files
      ));

  udevRules =
    pkgs.runCommand "udev-rules" {
      allowedReferences = [extra-utils];
      preferLocalBuild = true;
    } ''
      mkdir -p $out
      cp -v ${udev}/lib/udev/rules.d/60-cdrom_id.rules $out/
      cp -v ${udev}/lib/udev/rules.d/60-persistent-storage.rules $out/
      cp -v ${udev}/lib/udev/rules.d/75-net-description.rules $out/
      cp -v ${udev}/lib/udev/rules.d/80-drivers.rules $out/
      cp -v ${udev}/lib/udev/rules.d/80-net-setup-link.rules $out/
      cp -v ${pkgs.lvm2}/lib/udev/rules.d/*.rules $out/
      ${config.boot.initrd.extraUdevRulesCommands}

      for i in $out/*.rules; do
          substituteInPlace $i \
            --replace ata_id   ${extra-utils}/bin/ata_id \
            --replace scsi_id  ${extra-utils}/bin/scsi_id \
            --replace cdrom_id ${extra-utils}/bin/cdrom_id \
            --replace ${pkgs.coreutils}/bin/basename ${extra-utils}/bin/basename \
            --replace ${pkgs.util-linux}/bin/blkid   ${extra-utils}/bin/blkid \
            --replace ${lib.getBin pkgs.lvm2}/bin     ${extra-utils}/bin \
            --replace ${pkgs.mdadm}/sbin              ${extra-utils}/sbin \
            --replace ${pkgs.bash}/bin/sh             ${extra-utils}/bin/sh \
            --replace ${udev}                         ${extra-utils}
      done
      substituteInPlace $out/60-persistent-storage.rules \
        --replace ID_CDROM_MEDIA_TRACK_COUNT_DATA ID_CDROM_MEDIA
    '';

  # Use the topologically-sorted list from nixpkgs, not raw attrValues.
  fileSystems = lib.filter fsNeededForBoot config.system.build.fileSystems;

  fsInfo = pkgs.writeText "initrd-fsinfo" (lib.concatStringsSep "\n" (lib.concatMap (fs: [
      fs.mountPoint
      (
        if fs.device != null then fs.device
        else if fs.label != null && fs.label != "" then "/dev/disk/by-label/${fs.label}"
        else fs.fsType  # virtual filesystems (tmpfs, proc, etc.) use fsType as device
      )
      fs.fsType
      (builtins.concatStringsSep "," fs.options)
    ])
    fileSystems));

  resumeDevices =
    lib.filter (
      sd:
        lib.hasPrefix "/dev/" sd.device
        && !sd.randomEncryption.enable
        && !(lib.hasPrefix "/dev/zram" sd.device)
    )
    config.swapDevices;

  resumeDevicesList = lib.concatStringsSep " " (map
    (sd:
      if sd ? device
      then sd.device
      else "/dev/disk/by-label/${sd.label}")
    resumeDevices);

  # Hook scripts: stage1 expects file paths, not inline text.
  preFailCommandsFile = pkgs.writeText "pre-fail-commands" config.boot.initrd.preFailCommands;
  preDeviceCommandsFile = pkgs.writeText "pre-device-commands" config.boot.initrd.preDeviceCommands;
  postDeviceCommandsFile = pkgs.writeText "post-device-commands" config.boot.initrd.postDeviceCommands;
  postResumeCommandsFile = pkgs.writeText "post-resume-commands" config.boot.initrd.postResumeCommands;
  postMountCommandsFile = pkgs.writeText "post-mount-commands" config.boot.initrd.postMountCommands;

  postBootCommandsFile = pkgs.writeText "post-boot-commands" ''
    ${config.boot.postBootCommands}
    ${config.powerManagement.powerUpCommands}
  '';

  bootStage1 = pkgs.writeTextFile {
    name = "stage-1-init";
    executable = true;
    text = ''
      #!${extra-utils}/bin/ash
      export extraUtils=${extra-utils}
      export kernelModules=${lib.escapeShellArg (lib.concatStringsSep " " config.boot.initrd.kernelModules)}
      export resumeDevice=${lib.escapeShellArg config.boot.resumeDevice}
      export resumeDevices=${lib.escapeShellArg resumeDevicesList}
      export fsInfo=${fsInfo}
      export earlyMountScript=${config.system.build.earlyMountScript}
      export udevRules=${udevRules}
      export linkUnits=${linkUnits}
      export checkJournalingFS=${
        if config.boot.initrd.checkJournalingFS
        then "1"
        else "0"
      }
      export distroName=${lib.escapeShellArg config.system.nixos.distroName}
      export preFailCommands=${preFailCommandsFile}
      export preDeviceCommands=${preDeviceCommandsFile}
      export postDeviceCommands=${postDeviceCommandsFile}
      export postResumeCommands=${postResumeCommandsFile}
      export postMountCommands=${postMountCommandsFile}
      ${lib.optionalString (config.networking.hostId != null) ''
        export HOST_ID=${lib.escapeShellArg config.networking.hostId}
      ''}
      exec ${extra-utils}/bin/nixos-core stage-1-init
    '';
  };

  # top-level.nix does `substituteInPlace $out/init --subst-var-by systemConfig $out`
  # after copying bootStage2, so @systemConfig@ must be a literal string here.
  bootStage2 = pkgs.writeTextFile {
    name = "stage-2-init";
    executable = true;
    text = ''
      #!${pkgs.bash}/bin/bash
      export SYSTEM_CONFIG=@systemConfig@
      export NIX_STORE_MOUNT_OPTS=${lib.escapeShellArg (lib.concatStringsSep "," config.boot.nixStoreMountOpts)}
      export SYSTEMD_EXECUTABLE=${lib.escapeShellArg config.boot.systemdExecutable}
      export STAGE2_PATH=${lib.escapeShellArg (lib.makeBinPath ([pkgs.coreutils pkgs.util-linux] ++ lib.optional useHostResolvConf pkgs.openresolv))}
      export POST_BOOT_COMMANDS=${postBootCommandsFile}
      export USE_HOST_RESOLV_CONF=${
        if useHostResolvConf
        then "true"
        else "false"
      }
      export STAGE2_GREETING=${lib.escapeShellArg "<<< ${config.system.nixos.distroName} Stage 2 >>>"}
      exec ${cfg.package}/bin/stage-2-init
    '';
  };

  usersSpec = pkgs.writeText "users-groups.json" (builtins.toJSON {
    inherit (config.users) mutableUsers;
    users = lib.mapAttrsToList (_: u: {
      inherit
        (u)
        name
        uid
        group
        description
        home
        homeMode
        createHome
        isSystemUser
        password
        hashedPasswordFile
        hashedPassword
        autoSubUidGidRange
        subUidRanges
        subGidRanges
        initialPassword
        initialHashedPassword
        expires
        ;
      shell = toShellPath u.shell;
    }) (lib.filterAttrs (_: u: u.enable) config.users.users);
    groups = lib.attrValues config.users.groups;
  });
in {
  options.system.nixos-core = {
    enable = lib.mkEnableOption "nixos-core multi-call binary";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "self.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The nixos-core package.";
    };
  };

  config = lib.mkIf cfg.enable {
    # Legacy initrd only: add nixos-core to extraUtils so stage-1 wrapper can exec it,
    # and override bootStage1/initialRamdisk with our wrapper.
    # With the systemd initrd stage-1 is handled by systemd; these don't apply.
    boot.initrd.extraUtilsCommands = lib.mkIf (!config.boot.initrd.systemd.enable) ''
      copy_bin_and_libs ${cfg.package}/bin/nixos-core
    '';

    system.build.bootStage1 = lib.mkIf (!config.boot.initrd.systemd.enable) (lib.mkForce bootStage1);

    # Rebuild the initrd with our bootStage1 as /init.
    # Mirrors the contents list from nixpkgs' stage-1.nix.
    system.build.initialRamdisk = lib.mkIf (!config.boot.initrd.systemd.enable) (lib.mkForce (
      pkgs.makeInitrd {
        name = "initrd-${config.boot.kernelPackages.kernel.name or "kernel"}";
        inherit (config.boot.initrd) compressor compressorArgs prepend;
        contents =
          [
            {
              object = bootStage1;
              symlink = "/init";
            }
            {
              object = "${config.system.build.modulesClosure}/lib";
              symlink = "/lib";
            }
            {
              object = "${pkgs.kmod-blacklist-ubuntu}/modprobe.conf";
              symlink = "/etc/modprobe.d/ubuntu.conf";
            }
            {
              object = config.environment.etc."modprobe.d/nixos.conf".source;
              symlink = "/etc/modprobe.d/nixos.conf";
            }
            {
              object = pkgs.kmod-debian-aliases;
              symlink = "/etc/modprobe.d/debian.conf";
            }
          ]
          ++ lib.optionals config.services.multipath.enable [
            {
              object =
                pkgs.runCommand "multipath.conf" {
                  src = config.environment.etc."multipath.conf".text;
                  preferLocalBuild = true;
                } ''
                  target=$out
                  printf "$src" > $out
                  substituteInPlace $out \
                    --replace ${config.services.multipath.package}/lib ${extra-utils}/lib
                '';
              symlink = "/etc/multipath.conf";
            }
          ]
          ++ lib.mapAttrsToList (symlink: options: {
            inherit symlink;
            object = options.source;
          })
          config.boot.initrd.extraFiles;
      }
    ));

    system.build.bootStage2 = lib.mkIf (!config.boot.initrd.systemd.enable) (lib.mkForce bootStage2);

    system.build.installBootLoader = lib.mkIf config.boot.loader.initScript.enable (
      lib.mkForce "${cfg.package}/bin/init-script-builder"
    );

    system.activationScripts.users = lib.mkIf (!config.systemd.sysusers.enable) (lib.mkForce {
      supportsDryActivation = true;
      text = ''
        install -m 0700 -d /root
        install -m 0755 -d /home
        ${cfg.package}/bin/update-users-groups ${usersSpec}
      '';
    });

    system.build.etcActivationCommands = lib.mkIf (!config.system.etc.overlay.enable) (lib.mkForce ''
      echo "setting up /etc..."
      ${cfg.package}/bin/setup-etc ${config.system.build.etc}/etc
    '');
  };
}
