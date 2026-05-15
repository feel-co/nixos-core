# Stage 2 - NixOS Stage 2 Initialization

A Rust implementation of the NixOS stage 2 initialization process, providing a
bash-compatible replacement for `stage-2-init.sh` with optional improvements
borrowed from `nixos-init`.

## Philosophy

This crate has been designed to **match the behaviour of the Bash script
exactly**. Any feature borrowed from `nixos-init` are completely **opt-in** and
must be explicitly enabled through command-line flags or compile-time features.

## Features

### Default Behavior

When run without any opt-in flags, this tool behaves identically to the original
`stage-2-init.sh`:

- Reads configuration from environment variables
- Mounts special filesystems (`/proc`, `/dev`, `/sys`, `/dev/pts`, `/dev/shm`)
- Sets up `/nix/store` permissions (1775, root:nixbld)
- Applies `/nix/store` mount options (ro, nosuid, nodev)
- Creates required directories (`/etc`, `/etc/nixos`, `/tmp`, `/run/keys`)
- Runs the activation script (`$systemConfig/activate`)
- Creates `/run/booted-system` symlink
- Runs post-boot commands if provided
- Hands off to systemd via raw `execv`

### Opt-In Improvements from nixos-init

The following features can be enabled individually to improve safety and
robustness in specific scenarios:

#### `--atomic-symlinks`

Uses retry-based atomic symlinks (`.tmp0`, `.tmp1`, ... pattern) when creating
or replacing symlinks. This prevents race conditions when multiple processes
might be manipulating the same symlink.

#### `--create-current-system`

Creates `/run/current-system` symlink in addition to `/run/booted-system`. This
matches `nixos-init` behavior and ensures proper GC roots from the start.

#### `--setup-fhs`

Sets up `/usr/bin/env` and `/bin/sh` symlinks atomically.

Normally handled by activation scripts (`usrbinenv`, `binsh`), but this flag
allows stage-2 to set them up directly if running in an environment without
activation script support.

Requires `--env-binary` and `--sh-binary` to specify the target paths.

#### `--setup-modprobe`

Configures `/proc/sys/kernel/modprobe` to point to the wrapped modprobe binary.

Normally handled by the `modprobe` activation script. This flag allows stage-2
to configure it directly.

#### `--setup-firmware`

Configures the kernel firmware search path
(`/sys/module/firmware_class/parameters/path`).

Normally handled by activation scripts or initrd setup. This flag allows stage-2
to configure it directly.

#### `--use-systemctl-handoff` (requires `systemd-integration` feature)

Uses `systemctl switch-root` instead of raw `execv` for the systemd handoff.

This is the `nixos-init` approach. It ensures a cleaner transition and lets
Systemd handle mount propagation and service state correctly.

If `systemctl switch-root` fails, falls back to raw `execv`.

#### `--use-bootspec` (requires `bootspec` feature)

Reads configuration from `boot.json` (bootspec) instead of relying solely on
environment variables.

> [!NOTE]
> Currently informational only. All actual behavior still follows the bash
> script unless additional opt-in flags are set.

## Usage

### Basic Usage

```bash
stage-2-init --system-config /nix/store/...-nixos-system
```

### With `nixos-init` Compatibiltiy

```bash
stage-2-init \
  --system-config /nix/store/...-nixos-system \
  --atomic-symlinks \
  --create-current-system \
  --setup-fhs \
  --env-binary /run/current-system/sw/bin/env \
  --sh-binary /run/current-system/sw/bin/sh \
  --setup-modprobe \
  --setup-firmware
```

### Environment Variables

All options can also be set via environment variables:

- `SYSTEM_CONFIG` - Path to system configuration
- `STAGE2_GREETING` - Greeting message (default: "<<< NixOS Stage 2 >>>")
- `NIX_STORE_MOUNT_OPTS` - Comma-separated mount options for /nix/store
- `SYSTEMD_EXECUTABLE` - Path to systemd binary
- `POST_BOOT_COMMANDS` - Path to post-boot commands script
- `USE_HOST_RESOLV_CONF` - Use host resolv.conf (set to any value)
- `STAGE2_PATH` - PATH to set (default: "/run/current-system/sw/bin")
- `MODPROBE_BINARY` - Path to modprobe binary
- `FIRMWARE_PATH` - Path to firmware directory
- `ENV_BINARY` - Path to env binary (for --setup-fhs)
- `SH_BINARY` - Path to sh binary (for --setup-fhs)

## Compile-Time Features

- `bootspec` - Enables `--use-bootspec` flag for bootspec JSON parsing
- `systemd-integration` - Enables `--use-systemctl-handoff` for systemctl
  switch-root
- `full-nixos-init-compat` - Enables all nixos-init compatibility features

To stay true to the original scripted init behaviour, no compile-time features
are enabled by default. As `nixos-core` is designed to be functional on
non-Systemd distributions, this feature set will not change even after Nixpkgs
deprecated scripted initrd support.

## Comparison with nixos-init

<!--markdownlint-disable MD013-->

| Feature            | nixos-init            | stage2 (default)  | stage2 (opt-in)     |
| ------------------ | --------------------- | ----------------- | ------------------- |
| Config source      | bootspec JSON         | Env vars          | Env vars + bootspec |
| Symlink creation   | Atomic retry          | Simple            | Atomic retry        |
| FHS setup          | Built-in              | Activation script | Built-in            |
| Modprobe           | Built-in              | Activation script | Built-in            |
| Firmware           | Built-in              | Activation script | Built-in            |
| current-system     | Yes                   | No                | Yes                 |
| Handoff            | systemctl switch-root | execv             | Both available      |
| Systemd dependency | Required (initrd)     | None              | None                |

<!--markdownlint-enable MD013-->

## Rationale

Nixpkgs, and consequently NixOS, is moving toward `nixos-init` for systemd-based
systems, but there are valid reasons to maintain bash-compatible stage 2
initialization:

1. **Non-systemd initrd support** - Scripted initrd is still the default
2. **Gradual migration** - Systems can adopt Rust components incrementally
3. **Backward compatibility** - Existing configurations continue to work
4. **Flexibility** - Users can choose the right level of sophistication and
   prefer an option that is not constrained by Nixpkgs

This implementation provides a bridge: it maintains bash compatibility by
default while offering opt-in improvements for users who need them.
