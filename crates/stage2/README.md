# Stage 2 - NixOS Stage 2 Initialization

A Rust implementation of the NixOS stage 2 initialization process, providing a
bash-compatible replacement for `stage-2-init.sh` with optional improvements
borrowed from `nixos-init`.

## Philosophy

**Default behavior matches the original bash script exactly.** Any improvements
from `nixos-init` are **opt-in only** and must be explicitly enabled via
command-line flags or compile-time features.

## Features

### Default Behavior (Bash-Compatible)

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

**When to use:** High-concurrency environments or when you observe spurious
symlink-related failures during boot.

#### `--create-current-system`

Creates `/run/current-system` symlink in addition to `/run/booted-system`.
This matches `nixos-init` behavior and ensures proper GC roots from the start.

**When to use:** When you want stronger GC guarantees early in the boot
process, before activation scripts run.

#### `--setup-fhs`

Sets up `/usr/bin/env` and `/bin/sh` symlinks atomically.

Normally handled by activation scripts (`usrbinenv`, `binsh`), but this flag
allows stage-2 to set them up directly if running in an environment without
activation script support.

Requires `--env-binary` and `--sh-binary` to specify the target paths.

**When to use:** Container environments, custom init systems, or when you want
to bypass the activation script framework.

#### `--setup-modprobe`

Configures `/proc/sys/kernel/modprobe` to point to the wrapped modprobe binary.

Normally handled by the `modprobe` activation script. This flag allows
stage-2 to configure it directly.

**When to use:** When you need modprobe functionality before activation
scripts run, or in environments without activation script support.

#### `--setup-firmware`

Configures the kernel firmware search path
(`/sys/module/firmware_class/parameters/path`).

Normally handled by activation scripts or initrd setup. This flag allows
stage-2 to configure it directly.

**When to use:** When you need firmware loading before activation scripts run.

#### `--use-systemctl-handoff` (requires `systemd-integration` feature)

Uses `systemctl switch-root` instead of raw `execv` for the systemd handoff.

This is the `nixos-init` approach. It ensures a cleaner transition and lets
systemd handle mount propagation and service state correctly.

**When to use:** When running inside a systemd initrd context where
`systemctl switch-root` is available and preferred.

**Caveat:** If `systemctl switch-root` fails, falls back to raw `execv`.

#### `--use-bootspec` (requires `bootspec` feature)

Reads configuration from `boot.json` (bootspec) instead of relying solely on
environment variables.

**When to use:** When you want to validate that the bootspec is present and
well-formed, or for future extensibility.

**Note:** Currently informational only. All actual behavior still follows the
bash script unless additional opt-in flags are set.

## Usage

### Basic Usage (Bash-Compatible)

```bash
stage-2-init --system-config /nix/store/...-nixos-system
```

### With Opt-In Improvements

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
- `STAGE2_STRICT_ACTIVATION` - Fail if `$systemConfig/activate` is missing (set to `true` or `false`)
- `MODPROBE_BINARY` - Path to modprobe binary
- `FIRMWARE_PATH` - Path to firmware directory
- `ENV_BINARY` - Path to env binary (for --setup-fhs)
- `SH_BINARY` - Path to sh binary (for --setup-fhs)

## Compile-Time Features

- `bootspec` - Enables `--use-bootspec` flag for bootspec JSON parsing
- `systemd-integration` - Enables `--use-systemctl-handoff` for systemctl switch-root
- `full-nixos-init-compat` - Enables all nixos-init compatibility features

Default: **None** (pure bash-compatible behavior)

## Testing

The implementation is tested against the NixOS test suite:

```bash
nix-build -A nixosTests.boot-stage2
nix-build -A nixosTests.boot-stage1
```

## Comparison with nixos-init

| Feature | nixos-init | stage2 (default) | stage2 (opt-in) |
|---------|-----------|------------------|-----------------|
| Config source | bootspec JSON | Env vars | Env vars + bootspec |
| Symlink creation | Atomic retry | Simple | Atomic retry |
| FHS setup | Built-in | Activation script | Built-in |
| Modprobe | Built-in | Activation script | Built-in |
| Firmware | Built-in | Activation script | Built-in |
| current-system | Yes | No | Yes |
| Handoff | systemctl switch-root | execv | Both available |
| Systemd dependency | Required (initrd) | None | None |

## Migration Path

1. **Phase 1:** Deploy stage2 with default flags (drop-in replacement)
2. **Phase 2:** Enable individual opt-in features one at a time
3. **Phase 3:** Evaluate full nixos-init compatibility if desired

## Rationale

The NixOS community is moving toward `nixos-init` for systemd-based systems,
but there are valid reasons to maintain bash-compatible stage 2 initialization:

1. **Non-systemd initrd support** - Scripted initrd is still the default
2. **Gradual migration** - Systems can adopt Rust components incrementally
3. **Backward compatibility** - Existing configurations continue to work
4. **Flexibility** - Users can choose the right level of sophistication

This implementation provides a bridge: it maintains bash compatibility by
default while offering opt-in improvements for users who need them.
