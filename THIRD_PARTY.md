# Third-party notices

## konsave

The Plasma export/import module (`src/plasma/`) is a Rust port of the logic of
**konsave**, a Python tool for saving and restoring KDE Plasma customisations:

- Original: <https://github.com/Prayag2/konsave> by Prayag2 and contributors, commit
  `abdb6d38af6be570689e147eb7697bf571a17f9e`.
- Refactored fork: <https://github.com/urban-1/konsave> by urban-1 and contributors, commit
  `f07715ee1dadcaaa824fa084ecf0bc28f65106c9`.
- License: GNU General Public License, version 3 or (at your option) any later version.

What was taken: the concepts and behaviour (profiles, the save/export sections of the config, the
`$HOME`/`$CONFIG_DIR`/`$SHARE_DIR`/`$BIN_DIR` and `${ENDS_WITH}`/`${BEGINS_WITH}` placeholders, `apply`, `export`,
`import`, and the fork's `config-check` idea), plus the default KDE file list (`src/plasma/default.toml`) which
starts from konsave's `conf_kde.yaml`. No Python source is included. `docs/konsave-notes.md` records what was
studied and where kast deliberately differs.

Because of this, kast as a whole is distributed under the GPL (see `LICENSE`; `Cargo.toml` says
`GPL-3.0-or-later`). The App Vault's KDE file list in `src/vault/mod.rs` was likewise curated from konsave's
`conf_kde.yaml`.

Rust crate dependencies keep their own licenses (see `Cargo.lock` and each crate's metadata).
