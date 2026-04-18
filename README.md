# NixOS Core

This is a multi-call binary implementing core NixOS system utilities in safe,
portable Rust to replace some of the Perl and Bash scripts that are generally
load-bearing. While those scripts _can_ be replaced through various means such
as the Perl-less and Bash-less profiles, those usually change how your system
behaves while enabled, and may yield general instability or breakage. Such is
the case for the `/etc` overlay.

Thus, this is a monorepository providing a multi-call [^1] Rust binary to
replace some of the fragile scripts, and in some cases utilities _already_
written in Rust. See the [motivation section](#motivation) if you want to know
why we also try to replace Rust code with more Rust code.

[here]: https://github.com/orangecms/multicall/blob/main/README.md

[^1]: Multicall in Unix refers to a type of binary that allows multiple commands
    to be executed through a single executable, depending on the name used to
    invoke it. See [here] for a more comprehensive explanation. Historically
    multicall binaries have also been called "BusyBox-like", which should
    _probably_ be what you're thinking of for the purposes of this project.

In short, this package replaces legacy bash/Perl scripts that execute on every
NixOS system during boot and activation. The multi-call binary design (like
BusyBox) reduces binary size and simplifies deployment.

## Compatibility Matrix/Overview

The scope of this program is rather limited at the moment. It is limited in the
sense that Nixpkgs does not allow replacing most legacy solutions

<!--markdownlint-disable MD013-->

| Command               | Original Script          | Purpose                                           | Status                |
| --------------------- | ------------------------ | ------------------------------------------------- | --------------------- |
| `update-users-groups` | `update-users-groups.pl` | Manage `/etc/passwd`, `/etc/group`, `/etc/shadow` | Awaiting Verification |
| `setup-etc`           | `setup-etc.pl`           | Atomically update `/etc/static`                   | Awaiting Verification |
| `init-script-builder` | `init-script-builder.sh` | Create generic `/sbin/init`                       | Awaiting Verification |
| `stage-1-init`        | `stage-1-init.sh`        | Initrd bootstrap                                  | Awaiting Verification |
| `stage-2-init`        | `stage-2-init.sh`        | System activation                                 | Awaiting Verification |

<!--markdownlint-enable MD013-->

This project is currently in a "research preview" state, where _base rewrites_
are done but not yet verified in a real system. Until components are verified
through VM tests, it is not recommended to use those components on a production
system. The items marked as "awaits verification" above are those I am **yet to
test on real hardware**. Some VM tests will, naturally, exist but we cannot know
for certain that everything works as expected until it boots on baremetal.

## Usage

`nixos-core` is a multi-call binary that can be called either as a symlink:

```bash
# Invoke the update-users-groups command
$ update-users-groups /nix/store/...-users-groups.json
```

Or explicitly as a subcommand:

```bash
# Subcommand of update-users-groups
$ nixos-core update-users-groups /nix/store/...-users-groups.json
```

This usage pattern, like the mono-repo design, is a deliberate choice to allow
re-using code and shared patterns without the cognitive load of having to
cross-reference projects.

### Feature Flags/Overrides

All of the commands provided by nixos-core are **feature-gated**. This is so
that you can disable commands you don't need at build-time, and to let you pick
_which_ binaries to install and replace. You can use the feature flags while
building with `cargo`, or the package arguments while building with Nix:

```nix
[(prev: final: {
  nixos-core = prev.nixos-core.override {
    withStage1 = false; # e.g., skip initrd bootstrap
  }
})];
```

[`stage2` crate's README]: ./crates/stage2/README.md

There exists some _additional_ feature flags, also exposed by the overridable
attributes of the package, that you can use to control the **stage 2** behaviour
of nixos-core. Namely, `bootspec`, `systemd-integration` and
`full-nixos-init-compat` to provide complete compatibility with the pre-existing
behaviour of Nixpkgs' stage 2 behaviour. In the case you are developing a
Systemd-less NixOS variant but still would like to manage stage 2, you can
disable `systemd-integration` and still keep bootspec support! Most of those
features are described in detail at the [`stage2` crate's README].

## Motivation

[MicrOS]: https://github.com/snugnug/micros

This project aims to be a safe, independent utility for NixOS **and** NixOS
derivatives such as [MicrOS] with the core goals of being fast, portable, and
consistent. It is an out-of-tree module exactly to meet those goals, but without
the behavioural changes imposed by the Nixpkgs alternatives such as Userborn or
nixos-init. That is not to say that those are fundamentally incompatible, but
they are _different_. Perhaps there will be some collaboration in the future.

## Hacking

nixos-core is built with latest Rust stable available in Nixpkgs, which is
1.94.0 at the time of writing. Thus, the Minimum Supported Rust Version (MSRV)
for this project has been set as 1.94.0. This may change in the future as the
Rust language evolves.

### Safety

- No `unsafe` code except unavoidable syscalls (crypt, geteuid)
- Explicit error handling with `?` operator
- Dry-run mode available for all destructive operations

### Testing

Run NixOS tests after changes:

```bash
nix-build -A nixosTests.nixos-rebuild-specialisations
```

## License

This project is made available under the MIT license, following NixOS/Nixpkgs.
