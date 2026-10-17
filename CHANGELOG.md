# Changelog

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
