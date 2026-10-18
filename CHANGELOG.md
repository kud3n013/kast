# Changelog

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
