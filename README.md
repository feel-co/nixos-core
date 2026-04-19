# NixOS Core

This is a multi-call binary implementing core NixOS system utilities in safe,
portable Rust, replacing some of the Perl and Bash scripts that are generally
load-bearing. While those scripts _can_ be replaced through various means such
as the Perl-less and Bash-less profiles, those usually change how your system
behaves and may yield general instability or breakage, as is the case with the
`/etc` overlay.

This monorepository provides a multi-call [^1] Rust binary to replace some of
these fragile scripts, and in some cases utilities _already_ written in Rust.
See the [motivation section](#motivation) for why we also try to replace Rust
code with more Rust code.

[here]: https://github.com/orangecms/multicall/blob/main/README.md

[^1]: Multicall in Unix refers to a type of binary that allows multiple commands
    to be executed through a single executable, depending on the name used to
    invoke it. See [here] for a more comprehensive explanation. Historically
    multicall binaries have also been called "BusyBox-like", which should
    _probably_ be what you're thinking of for the purposes of this project.

The package replaces legacy Bash and Perl scripts that execute on every NixOS
system during boot and activation. The multi-call binary design, like BusyBox,
reduces binary size and simplifies deployment.

## Compatibility Matrix/Overview

The scope of this program is currently limited by what Nixpkgs permits in terms
of replacing legacy solutions.

<!--markdownlint-disable MD013-->

| Command               | Original Script          | Purpose                                           | Status                |
| --------------------- | ------------------------ | ------------------------------------------------- | --------------------- |
| `update-users-groups` | `update-users-groups.pl` | Manage `/etc/passwd`, `/etc/group`, `/etc/shadow` | Awaiting Verification |
| `setup-etc`           | `setup-etc.pl`           | Atomically update `/etc/static`                   | Awaiting Verification |
| `init-script-builder` | `init-script-builder.sh` | Create generic `/sbin/init`                       | Awaiting Verification |
| `stage-1-init`        | `stage-1-init.sh`        | Initrd bootstrap                                  | Awaiting Verification |
| `stage-2-init`        | `stage-2-init.sh`        | System activation                                 | Awaiting Verification |

<!--markdownlint-enable MD013-->

This project is currently in a "research preview" state. Base rewrites are
complete but not yet verified on a real system. Until components are verified
through VM tests, do not use them on a production system. Items marked "Awaiting
Verification" have not yet been tested on real hardware. Some VM tests will
naturally exist, but we cannot know with certainty that everything works as
expected until it boots on bare metal.

## Usage

`nixos-core` is a multi-call binary invocable either as a symlink:

```bash
# Invoke the update-users-groups command
$ update-users-groups /nix/store/...-users-groups.json
```

Or explicitly as a subcommand:

```bash
# Subcommand of update-users-groups
$ nixos-core update-users-groups /nix/store/...-users-groups.json
```

This usage pattern, like the mono-repo design, is a deliberate choice that
allows reusing code and shared patterns without the cognitive overhead of
cross-referencing separate projects.

### Feature Flags/Overrides

All commands provided by `nixos-core` are feature-gated, letting you disable
commands you don't need at build time and choose which binaries to install and
replace. Feature flags work both with `cargo` and as package arguments when
building with Nix:

```nix
[(prev: final: {
  nixos-core = prev.nixos-core.override {
    withStage1 = false; # e.g., skip initrd bootstrap
  }
})];
```

[`stage2` crate's README]: ./crates/stage2/README.md

Additional feature flags, also exposed as overridable package attributes,
control the behavior of **stage 2**. These are `bootspec`,
`systemd-integration`, and `full-nixos-init-compat`, providing complete
compatibility with the pre-existing behavior of Nixpkgs' stage 2. If you are
developing a systemd-less NixOS variant but still want to manage stage 2, you
can disable `systemd-integration` while retaining `bootspec` support. Most of
these features are described in detail in the [`stage2` crate's README].

## Motivation

[MicrOS]: https://github.com/snugnug/micros

This project aims to be a safe, independent utility for NixOS and NixOS
derivatives such as [MicrOS], with the core goals of being fast, portable, and
consistent. It is an out-of-tree module to meet those goals without the
behavioral changes imposed by Nixpkgs alternatives like Userborn or nixos-init.
That is not to say those are fundamentally incompatible with this project, but
they are _different_. There may be room for collaboration in the future.

## Hacking

`nixos-core` is built with the latest stable Rust available in Nixpkgs, which is
1.94.0 at the time of writing. The Minimum Supported Rust Version (MSRV) is
therefore set at 1.94.0, and may change as the language evolves.

### Safety

- No `unsafe` code except for unavoidable syscalls (`crypt`, `geteuid`)
- Explicit error handling with the `?` operator
- Dry-run mode available for all destructive operations

### Testing

This repository provides a few VM test to verify correct behaviour. Those are
rather limited at the time but should provide some amount of guidance for you.

```bash
# Run all VM tests that are exposed under `checks`
$ nix flake check

# Run a specific VM tests
$ nix build .#checks.x86_64-linux.boot
```

You can find the available NixOS VM tests in the [nix/vm-tests](./nix/vm-tests/)
directory. If you're adding new features, you should add a new test component or
some subtests that verify that your code works exactly as expected.

## License

This project is made available under the MIT license, following NixOS/Nixpkgs.
