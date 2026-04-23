# `/etc` activation

The `setup-etc` crate, exposed as `nixos-core setup-etc`, or `setup-etc`
directly when symlinked from the `nixos-core` binary, replaces upstream
`setup-etc.pl` Perl script that Nixpkgs uses to pupulate `/etc` from the current
NixOS generation.

The two implementations agree on the on-disk shape of `/etc`. This is the same
`/etc/static` indirection, the same pass-through symlinks, the same
`direct-symlink` mode, the same copied files with explicit `mode`/`uid`/`gid`
sidecars, however, the way they track and apply changes is different.

## How it works

[smfh]: https://github.com/feel-co/smfh

1. Atomically swap `/etc/static` to point at the new generation's etc tree.
2. Replay the legacy `/etc/.clean` file written by the Perl script: every
   relative path it lists is removed from `/etc`, then `.clean` itself is
   removed. The smfh activation in step 5 will recreate any of those entries
   that are still in the configuration; entries that have been dropped from the
   configuration stay deleted. One-shot per system.
3. Walk `/etc` and remove any symlink whose target lives under `/etc/static/`
   but whose entry no longer exists in the new generation. Mirrors the Perl
   `cleanup` pass and protects against symlinks created by an earlier activation
   that are not in our manifest.
4. Walk the generation's etc store tree and build an [smfh] manifest describing
   every entry: pass-through symlinks (`/etc/foo` -> `/etc/static/foo`), direct
   symlinks (`/etc/foo` -> `/nix/store/...`), and copied files (with explicit
   `mode`, `uid`, `gid`).
5. Write the manifest to `/var/lib/nixos/etc-manifest.json.new`, ask smfh to
   diff it against the existing `/var/lib/nixos/etc-manifest.json`, and apply
   the diff: deactivate (delete) entries that disappeared, activate entries that
   appeared, and atomically re-apply entries whose source changed.
6. Atomically rename the temp manifest into place.
7. Touch `/etc/NIXOS`.

> [!INFO]
> The manifest at `/var/lib/nixos/etc-manifest.json` is the source of truth for
> what nixos-core has written into `/etc`. It supersedes the Perl script's
> `/etc/.clean` state file, which only tracked copied files.

## Differences from the Perl script

As of the instroduction of `smfh` into `nixos-core`, we do `/etc` setup a little
differently. The differences are specifically in the tracking and activation
models.

### Tracking Model

Previously the **Perl-based** setup wrote `/etc/.clean` listing the relative
paths of files it had copied. Pass-through symlinks were not tracked there; they
were cleaned up on the next activation by walking `/etc` and removing any
symlink under `/etc/static/` that no longer existed in the new generation.

**nixos-core**, however, writes `/var/lib/nixos/etc-manifest.json`, which lists
_every entry_. This includes symlinks, direct symlinks, copies and the like.
Cleanup of stale entries is driven by an explicit diff between the old and new
manifest, not by a heuristic walk of `/etc`. The dangling-symlink walk is still
done as a safety net for entries that originated from outside the manifest (e.g.
a previous Perl-based activation, or external tooling).

The first activation under nixos-core has no prior manifest, so smfh falls back
to a full activation. The dangling-symlink walk handles the cleanup of entries
left over from the previous Perl run.

### Activation atomicity

Both implementations use a temp-file + rename when replacing an existing entry,
so the common case (system updates) keeps `/etc/foo` either fully old or fully
new. Files are _never_ half-written. Though, the implementations diverge on
**first activation** of a copied file (the target does not yet exist). I.e.,
**setup-etc.pl** copies into `$target.tmp`, sets ownership and mode on the temp
file, and then renames into place. The final path never exists with wrong
permissions. **nixos-core** (via smfh), on another hand, writes directly to the
final path on first activation, then calls `chmod`/`chown`. There is a brief
window during which the file exists at its final path with default umask
permissions (typically `0644`) before the configured mode is applied.

This window only exists on the very first time a given path is activated. Once
the file is present, every subsequent activation uses the atomic temp-and-rename
path. In practice, the regression is mostly theoretical for NixOS: `/etc/shadow`
is managed by `update-users-groups`, not by `environment.etc` with a `mode`
sidecar, and most `mode`-bearing entries are not secret material. Future
versions of smfh may close this window upstream.

### Idempotent re-runs

`nixos-core` also leverages smfh's manifest and BLAKE3 hash systems to verify
each file (hash for copies, target check for symlinks) and skips entries that
already match. The net effect of this behaviour is faster idempotent activations
and no needless rewrites of `/etc` files. User edits to copied entries are still
overwritten on the next generation switch (entries are flagged `clobber: true`),
matching Perl's behavior in spirit. This is rather different as opposed to the
`setup-etc.pl` script that unconditionally re-copied every copied file on every
activation, whether the source changed or not.

## Migrating from the Perl script

There's, well, nothing for you to do. The first activation under nixos-core
rplays `/etc/.clean` to delete every copied file the Perl script tracked, then
removes the file itself; entries still in the configuration are recreated by the
manifest activation that follows. `nixos-core` uses the dangling-symlink walk to
clean up any pass-through symlinks left over from the previous Perl run that are
no longer in the configuration, and writes a fresh
`/var/lib/nixos/etc-manifest.json`.

After the first activation, all subsequent activations are diff-driven from the
manifest. The VM tests for the `setup-etc` module exercise this path end-to-end
on its `perl` node: boot under `setup-etc.pl`, switch to nixos-core, verify that
both stale symlinks and stale copied files from the Perl run are removed. If you
notice any issues, create an issue!

## Glossary/Files

- `/etc/NIXOS` - tag file marking this filesystem as a NixOS root.
- `/etc/static` - symlink to the current generation's etc store tree.
- `/var/lib/nixos/etc-manifest.json` - current manifest. Do not edit manually;
  setup-etc rewrites it on every activation.
- `/var/lib/nixos/etc-manifest.json.new` - temp file used for the atomic rename.
  Removed automatically on success or failure; if it exists outside an
  in-progress activation, the previous activation crashed mid-write and it is
  safe to delete.
