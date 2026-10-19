# Changelog

## Unreleased

- **`[archrepo]` goes above every other repository in `pacman.conf`**, so pacman takes
  archrepo's packages over Arch's and CachyOS's. `kast repo` also moves a block that an
  older version appended at the end.
- **`kast plasma apply` stops `kglobalacceld` while it restores shortcuts**, then starts it
  again. The daemon owns the global shortcuts and rewrote `kglobalshortcutsrc` from memory,
  so KDE's defaults (Win+L) won over the backup.
- **`kast import` starts the apps it closed again**, like `backup` does (`--no-relaunch`
  leaves them closed). Without `-k` it no longer force-kills an app that ignores the close
  request: it stops before changing anything.

## 0.7.2

- **Every package archrepo publishes is offered automatically**, without an entry in
  `packages.toml`. kast reads archrepo's signed database (verified with `gpgv` against the
  key it ships) next to the list and adds what the file doesn't mention: as an add-on of a
  listed package it depends on, otherwise to the kud3n group. They show up marked "from
  archrepo, not in packages.toml" in the picker and in `kast packages --list` (the source line
  counts them), install as `archrepo/<name>`, and are never built from the AUR. If the
  download or signature fails, or you're offline, the list is as before; the reason is shown.
  - New optional `[archrepo]` table in `packages.toml`: `auto = false` turns it off,
    `exclude = [...]` keeps packages out. No `schema` bump.
  - Left out of the picker while `[archrepo]` isn't in `pacman.conf` and the run has no
    repository step, so kast never installs a package that wasn't shown.
  - When a retry brings a different list, the picker keeps your checks for the packages both
    lists have.
- `packages.toml`: `zed` is now `archrepo/zed-globalmenu` (it replaces `zed`) and
  `obsidian` is `archrepo/obsidian`, so pacman can't pick Arch's unpatched builds.

## 0.7.1

- **Restoring a backup made under another account name now works properly.**
  - Absolute `/home/<old>` paths in restored KDE, Hyprland and fcitx5 settings are rewritten to
    this account's home (whole path components only, text files up to 4 MiB, never app databases
    or browser profiles). Backups now record their source home in the manifest; older archives
    fall back to `/home/<user>`.
  - The Howdy restore installs only your own face model, as `<user>.dat`, replacing an existing
    one. It no longer leaves an orphan `<olduser>.dat` or brings in other users' models from old
    multi-model archives.

## 0.7.0

- **The data partition step is back, and it works for any partition**, not just the old NTFS
  `/media/data`. Menu `2`, `kast partition` and the first step of the automatic run: pick an
  internal or external partition (NTFS, exFAT, FAT, ext4, btrfs, XFS) from a list that greys out
  what can't be used and says why, choose the mount point and options, see the exact
  `/etc/fstab` line, and optionally move home folders onto it.
  - Detection with `lsblk -J -b` cross-checked by `blkid -p`, parsed defensively across
    util-linux versions (string booleans/sizes, `mountpoint` vs `mountpoints`, missing columns).
  - A correct entry per filesystem: six fields, escaped, `UUID=` as reported, `nofail`, a device
    timeout (5 s external, 10 s internal), `x-systemd.automount` for external drives (with a toggle
    for mounting at boot), correct fsck order, ownership through `uid/gid` + `dmask/fmask` presets
    on NTFS/exFAT/FAT and a `chown` of the mount folder only on ext4/btrfs/xfs. Never `defaults`,
    `exec`, `force` or `umask=000`; options are only emitted if the driver accepts them.
  - Safe apply: test mount first (NTFS Fast Startup/dirty volumes are explained), `findmnt
    --verify` on the candidate fstab (only new problems block), atomic write with a backup,
    `daemon-reload`, mount only that entry (never `mount -a`) and verify it; any failure restores
    the original bytes of `/etc/fstab`.
  - Fully undoable: the fstab line (and the line it replaced), the mount, the mount folder's
    owner, the folder moves (copied back, never deleting the copy on the partition) and
    `user-dirs.dirs` are journaled; a busy mount stops the undo and names the processes.
  - `kast partition --list [--json]`, `--device … --mountpoint … --dry-run`, `--external`,
    `--internal`, `--boot`, `--read-only`, `--options`, `--move-folders`, `--replace`,
    `--unmount-first`. `kast undo --step partition` works.
  - `~/.config/user-dirs.dirs` is now edited key by key and journaled instead of overwritten.

## 0.6.2

- **`kast fix-permissions` now checks the folders themselves**: `~`, `~/.config`, `~/.local` and
  `~/.local/share`. An old restore (`rsync -a src/ dest/`) could hand these to root, after which
  files inside stayed yours but nothing new could be created (`spectaclerc` "not writable"), and
  the repair command did not see it. The check also runs after every restore.
## 0.6.1

- **Fixed: `Configuration file "~/.config/spectaclerc" not writable`** (and the same for other KDE
  apps) after restoring KDE settings. Restored files could be left root-owned, or read-only for
  their owner: the restore kept an archived `0444`/`0400` mode (`--chmod=go-w` never added owner
  write), and when kast ran under sudo the files were written as root, with ownership only handed
  back at the very end by a `chown` whose errors were ignored.
  - The restore now writes **as your user** (rsync runs through `sudo -u`), so nothing can be
    created root-owned. The extracted archive is handed to you for that step, except the Howdy
    face models, which stay root-only (`0700`).
  - Restored files always keep owner read/write (and folders owner rwx); group and others never
    write. The same holds when extracting archives, for `undo`, and for `kast plasma apply`.
  - After every restore (and `kast plasma apply`) kast checks each restored entry (owner and owner
    write), fixes what it can and lists the rest in the summary with the repair command. A failed
    `chown`/`chmod` is never ignored.
  - New **`kast fix-permissions`** (`--dry-run`, `--yes`) repairs a system an older kast already
    broke: it lists and fixes files in the KDE and app settings kast manages that are not yours or
    not writable by you. It never follows symlinks and never leaves your home.
    Run it with `sudo kast fix-permissions` (sudo is needed only to `chown` root-owned files).
## 0.6.0

- **KDE Plasma settings** (`kast plasma ...`, menu key `p`): save, list, apply, export, import and
  remove profiles of your Plasma settings, `config-check`, and the packages a profile needs.
  A Rust port of the logic of [konsave](https://github.com/Prayag2/konsave) and its
  [urban-1 fork](https://github.com/urban-1/konsave) (see `THIRD_PARTY.md`).
  - `apply` shows a dry run, applies by category (`--only`), snapshots what it overwrites
    (`pre-apply-<time>`), writes atomically and never deletes; `--restart-plasma` quits and starts
    plasmashell around it.
  - Archives (`.kast-plasma.tar.gz`) are checked before anything is written: no absolute or `..`
    paths, no writing through symlinks, no hard links or devices; destinations come from your own list.
  - Your list: `~/.config/kast/plasma.toml` (`kast plasma default-config` prints the built-in one).
- kast is now licensed GPL-3.0-or-later (`LICENSE`), because it contains the konsave port.
