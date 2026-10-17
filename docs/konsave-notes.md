# konsave study notes (basis for kast's `plasma` module)

Studied from clones kept outside this repo (never committed):

| Repo | Commit | Date |
|---|---|---|
| [Prayag2/konsave](https://github.com/Prayag2/konsave) (original, PyPI `konsave` 2.2.x) | `abdb6d38af6be570689e147eb7697bf571a17f9e` | 2026-01-30 |
| [urban-1/konsave](https://github.com/urban-1/konsave) (refactored fork) | `f07715ee1dadcaaa824fa084ecf0bc28f65106c9` | 2023-08-10 |

Both are GPL-3.0 (`LICENSE` is the unmodified FSF text; `setup.py` classifier says "GPLv3 or later"). The fork has no
`CHANGELOG.md`, no tests; the original has no tests either. File names below are from the Python package `konsave/`.

## 1. Config (`conf.yaml`)

Schema (header comment of `conf_kde.yaml`), both repos:

```yaml
save:                       # backed up into the profile
  <section>:
    location: "<dir with placeholders>"
    entries: [name, name, ...]      # paths relative to location
export:                     # only used by export/import, never stored in the profile
  <section>:
    location: ...
    entries: [...]
```

Placeholders are plain string replaces on `location` only (never on entries): `$HOME`, `$CONFIG_DIR` (`$HOME/.config`),
`$SHARE_DIR` (`$HOME/.local/share`), `$BIN_DIR` (`$HOME/.local/bin`) (`consts.py`, `parse.parse_keywords` /
`config._parse_keywords`). Note `$HOME` replaces first-come, so `$HOME` inside `$HOMEDIR` would also match.

`${ENDS_WITH='x'}` / `${BEGINS_WITH='x'}` (`parse.ends_with`/`begins_with`, `config.ends_with`/`begins_with`): list the
directory in front of the token and substitute the **first** entry from `os.listdir` (unsorted, so filesystem order)
whose name ends/begins with `x`. The argument regex is `\S+` so it cannot contain spaces.

- **No match**: original returns the bare token text (`${ENDS_WITH=...}` literally), so the location becomes a
  nonexistent path and the section is silently skipped by the `os.path.exists` checks. The fork does
  `os.listdir(path[0:token])`, slicing a `str` with a `str`, which raises `TypeError`: **the fork's placeholder
  functions crash on every use** (the shipped config uses none, so it is never hit).
- **Several matches**: the first one listed wins, silently and non-deterministically.
- **Globs**: none. Entries are literal names; there is no wildcard support in either repo.
- Empty `entries:` parse as YAML `None`: the original converts them to `[]` (`read_konsave_config`, #47/#57), the fork
  guards each loop with `entries or ()`.

## 2. Profile storage

`PROFILES_DIR = ~/.config/konsave/profiles` (`consts.py`); global config `~/.config/konsave/conf.yaml`, copied from the
package template on first run (`__main__.main` / `funcs.install_config`). Profile layout:

```
profiles/<name>/
  conf.yaml            # copy of the *global* conf.yaml at save time
  <section>/<entry>    # one directory per `save` section
```

No `save/`/`export/` split inside a profile: `export/` only exists inside a `.knsv` archive. No metadata, id or version
file. Profiles are the directory listing (`os.listdir`, sorted for display only); the original used numeric IDs until
2.1.0, now names only.

## 3. save (`funcs.save_profile`)

- Refuses if the name exists unless `-f`; with `-f` it writes **into** the old directory (merge), so entries removed from
  the config or deleted since linger in the profile forever.
- For each `save` section, `mkdir <profile>/<section>`, then per entry: skip silently if the source does not exist; a
  directory goes through the recursive `copy()`, a file through `shutil.copy`.
- `copy()` (`funcs.copy`): `os.listdir` recursion, `os.remove(dest)` then `shutil.copy`. Symlinks are **followed**
  (`isdir`, `exists`, `shutil.copy` all dereference), dangling symlinks are skipped (`exists` false), symlink loops
  recurse until the OS errors. `shutil.copy` keeps the permission bits but **not** timestamps (`copy2` would). In the
  original the whole function is wrapped in `exception_handler`: any error is written to `~/.cache/konsave_log.txt`
  and **swallowed**, the command prints "saved successfully" regardless. Fork: no wrapper, `AssertionError` surfaces.
- Copies the global `conf.yaml` into the profile.

## 4. apply (`funcs.apply_profile`)

Reads the **profile's** `conf.yaml`, and for each `save` section calls `copy(<profile>/<section>, location)`:
overlay copy. Files present in the profile overwrite the live ones (`os.remove` + copy, no backup, not atomic, a crash
between the two loses the file); files on disk but absent from the profile are left alone, and an entry that existed
at save time but not now is *not* removed. `location` comes from the archive/profile's own `conf.yaml`
(**an imported profile therefore chooses where apply writes**, absolute paths and `..` included).
If `location` does not exist, `copy()` does `os.mkdir` (not `makedirs`), which fails when the parent is missing (#89).
Prints "log out and in". Fork adds `-r/--reload-kde`, which runs `KDE_RELOAD_CMD = "killall plasmashell; kstart plasmashell"`
via `os.system` **after** copying. That is the wrong order for Plasma: `plasmashell` rewrites
`plasma-org.kde.plasma.desktop-appletsrc`/`plasmashellrc` from memory on exit, undoing the apply (#120, #64 are
plausibly this). It also hardcodes Plasma 5-era `kstart` (Plasma 6 ships `kstart`, `kquitapp6`).

## 5. export / import

`export` (`funcs.export`, both repos): builds a directory tree then zips it with `shutil.make_archive(..., "zip")`
and renames to `.knsv`, so **`.knsv` is a ZIP (deflate), not tar.gz**:

```
conf.yaml                    # the GLOBAL config file (not the profile's own copy: they can disagree)
save/<section>/<entry>       # copied from the profile
export/<section>/<entry>     # copied *live* from `location` (the machine's current files) at export time
```

Name collision on the output path (`<name>`, `.knsv`, `.zip`): without `-f` it appends a timestamp (original
`f%d-%m-%Y:%H-%M-%S`, with colons; fork `_<isoformat>`), with `-f` it overwrites. Original: `-d dir`, `-n name`; fork:
`-o path` (extension stripped, `/dev/stdout` allowed).

`import_profile`: requires `is_zipfile` and a `.knsv` name; profile name = file basename (fork: `-n/--import-name`);
refuses if that profile exists. Extracts with `ZipFile.extractall` to a temp dir (original `~/.config/konsave/temp/<name>`
not cleaned up on failure; fork `TemporaryDirectory`), then:
1. copies `save/` to `profiles/<name>/` and `conf.yaml` beside it;
2. for each `export` section in the archive's `conf.yaml`, copies `export/<section>/<entry>` **straight over the live
   files** at the archive-declared `location` (overwrite, no backup, no confirmation, and apply is separate but
   import has already changed the live system).

`extractall` sanitises `..`/absolute member names (Python's zipfile strips them) so classic zip-slip is mitigated, but
the `location` fields come from the untrusted archive, which is the larger hole. Zip symlink members are written as
plain files by zipfile (no symlink creation), a safe accident.

The `.knsv` layout above is a zip; kast writes `.tar.gz` (a task constraint), so kast archives **cannot** share the
`.knsv` extension. kast uses its own **`.kast-plasma.tar.gz`** (see design).

## 6. `config-check` / `ls-archive` (fork only)

- `config_check` (`funcs.config_check`): for every `save` section whose `location == CONFIG_DIR` (exact string compare:
  `$CONFIG_DIR/autostart`, a trailing slash, or `$HOME/.config` all silently skip), prints a table of
  `sorted(entries | os.listdir(~/.config))` with two booleans: `Backed Up?` and `In ~/.config`. Needs `tabulate`.
- `ls_archive`: opens the zip, accumulates per-directory sums of `file_size`/`compress_size` over `infolist()` and prints
  name, human size, compressed size.

## 7. Default KDE list (`conf_kde.yaml`, original)

`save.configs` (`$CONFIG_DIR`): gtk-2.0, gtk-3.0, gtk-4.0, kate, Kvantum, latte, dolphinrc, katerc, konsolerc,
kcminputrc, kdeglobals, kglobalshortcutsrc, klipperrc, krunnerrc, kscreenlockerrc, ksmserverrc, kwinrc, kwinrulesrc,
plasma-org.kde.plasma.desktop-appletsrc, plasmarc, plasmashellrc, gtkrc, gtkrc-2.0, lattedockrc, breezerc, oxygenrc,
lightlyrc, ksplashrc, khotkeysrc, systemsettingsrc, kded5rc, kconf_updaterc, kscreenrc, plasma-systemmonitorrc,
spectaclerc, plasmanotifyrc, plasma-localerc, plasma_calendar_holiday_regions, xdg-desktop-portal-kderc,
kwinoutputconfig.json.
`save.app_layouts` (`$HOME/.local/share/kxmlgui5`): dolphin, konsole, yakuakerc, systemsettings, spectacle,
plasma-systemmonitor.
`save.autostart` (`$CONFIG_DIR/autostart`): *empty*.
`export.share_folder` (`$SHARE_DIR`): plasma, kwin, konsole, fonts, color-schemes, aurorae, icons, wallpapers, yakuake.
`export.home_folder` (`$HOME/`): .fonts, .themes, .icons.
The fork's list is the same set in a different order, without the original's "Plasma 6.x compatibility" additions
(systemsettingsrc ... kwinoutputconfig.json). Still missing in both for Plasma 6 (#106): `kdedefaults/`, `kded6rc`,
`kxmlgui6`, `plasmashell` window state, and the plasmoid/theme folders under `~/.local/share/plasma` are only exported,
not saved.

## 8. Known bugs and sharp edges

From the code and the issue tracker (`gh issue list -R Prayag2/konsave`, issues #3 to #134; many remain open):

- **Untrusted `conf.yaml` controls write destinations** on import and apply (section 4/5). Most serious.
- **No safety net**: apply/import overwrite live files without backup; `os.remove` before copy; swallowed errors (orig).
- **Symlinks followed** on save: a symlinked config dir is dereferenced into the profile; loops possible.
- **`save -f` merges** rather than replaces. **Export uses the global config**, not the profile's. **Export of `export/`
  sections reads live files**, so an export is not a faithful copy of the saved profile.
- **Plasma 6**: missing files (#106), Plasma rewrites state on exit so apply-while-running is overwritten (#120, #64,
  #41), `kxmlgui5` -> `kxmlgui6`. #132 not portable across distributions (absolute paths in themes, fonts,
  icon themes that were never installed there: package names would help).
- **#79 credentials in `plasma-org.kde.plasma.desktop-appletsrc`** (calendar widgets) end up in shared archives.
  kast warns on export when it spots obvious secret keys.
- **#89** import fails when the destination's parent directories do not exist (`mkdir` not `makedirs`).
- **#10** (closed) absolute paths in archives; the current relative `save/`/`export/` layout is the result.
- **#68** apply removes Konsole session state (`session/`, `konsolerc`): known Plasma-restores-from-session effect.
- **#96/#102/#91** huge exports (4.5 GB) and near-empty (4 KB) ones: `export` copies `icons`, `wallpapers`, `fonts`
  wholesale, and the original swallows copy errors (section 3), which fits the empty-archive reports (not verified).
- Fork-only: `ends_with`/`begins_with` TypeError; `config_check` exact-match location.

## 9. Intentionally not ported

- Python CLI plumbing (`argparse`, logging setup, `exception_handler` log file, `wipe`, `reset-config`): kast has its
  own clap CLI, logging and journal. `wipe` is dropped (destructive, `remove` per profile exists).
- `pyyaml` / `conf.yaml`: kast uses TOML (serde + toml are already dependencies).
- First-run copy of the template by `$XDG_CURRENT_DESKTOP` and `conf_other.yaml`: kast embeds the default list and only
  writes a user override on request. This module is KDE Plasma only.
- Zip `.knsv` import/export for interoperability: out of scope here; the layouts differ (kast archives are tar.gz with
  `profile.toml` instead of `conf.yaml`), a conversion could be a follow-up.
- Export of live `export` files that are not part of the saved profile: kast takes **assets into the profile at
  save time** (see design), so an archive always equals the profile.
- `tabulate` tables, `-d` debug flag.

## Design (kast `src/plasma/`)

**Naming.** Top-level `kast export`/`import` already mean the App Vault, so the feature is `kast plasma <list|save|
apply|remove|export|import|config-check>` and a TUI screen `screens/plasma.rs`. The vault's `kde` app is untouched.

**Config.** TOML (kast's format) with serde: `[[section]]` with `id`, `category`, `location` (placeholders) and `entries`.
Entries may use globs. Embedded via `include_str!("default.toml")`; a user file `~/.config/kast/plasma.toml`
replaces it when present. `kind = "save"|"export"` mirrors konsave's two sections: `save` entries are always stored in
the profile, `export` entries (large assets) only by `save --assets`, or live at `export` time. Placeholders resolve against
an injected `Roots { home, config, share, bin, profiles }` and **only** from kast's table: locations must stay under
`home` (absolute paths and `..` rejected), so a hostile archive can never choose where files go; destination roots
come from the local config, never from the archive. `ENDS_WITH`/`BEGINS_WITH` are replaced with sorted candidates;
several matches warn and use the first sorted, none skips the section with a warning.

**Profile layout.** `~/.local/share/kast/plasma/profiles/<name>/{meta.toml, profile.toml, files/<section>/<entry>}`
(not under `~/.config`, because profiles can be large). `profile.toml` is the config *as saved* (so apply uses what was
saved, and the entry list can differ from the current default). `meta.toml`: kast version, Plasma version, hostname,
date, source commit of the default list, required packages.

**Operations.** All state-changing operations first build a `Plan` (create / overwrite / unchanged / skip(reason)),
then execute exactly that plan. `--dry-run` prints it. Before apply: snapshot of the overwrite set into
`pre-apply-<timestamp>`. Writes are atomic (temp file in the same directory, then rename). Symlinks are stored and
restored as symlinks (never followed), special files skipped. `save --force` builds the new profile in a temp dir and
swaps it in (no merge).

**Archive.** `.kast-plasma.tar.gz` (gzip, `tar` + `flate2`): `profile.toml`, `meta.toml`, `files/...` only; absolute
paths, `..`, hardlinks, devices and symlinks leaving the tree are rejected; unpacking goes through a temp dir under
the profiles dir, then rename (also the collision check).

**Plasma reload.** quit plasmashell, apply, `kstart plasmashell` (the order that survives Plasma's write-on-exit);
binaries found with `which` (`kquitapp6`/`kquitapp5`, `kstart`/`kstart5`), run as the target user via `run_as_user`.

## As built (deviations from konsave, and from this design)

- `import` only creates a profile; it never writes live files (konsave overwrote the live
  `export` locations on import). Applying writes both kinds, from the profile.
- A profile stores no locations: `apply` takes the location of each section from the *local*
  list by section id and only restores entries that list covers (konsave trusted the archive's
  own `conf.yaml`).
- Dry run is the plan that `apply` and `import` run (one code path), plus a snapshot
  `pre-apply-<time>` of the files about to be overwritten. Files an apply creates are not removed
  when the snapshot is restored (nothing is ever deleted).
- Wildcards are `*` and `?` in the last component, matched by a small built-in matcher instead
  of the `glob` crate (no new dependency for ten lines of logic).
- Not done: the `find -mmin` discovery of changed files (needs a person changing settings in
  System Settings). The list was checked with read-only `test -e` on a Plasma 6.8.0 machine
  instead; names missing here are kept, because many (`plasmarc`, `kscreenlockerrc`, `khotkeysrc`...)
  only appear once a setting is changed.
- Reading konsave's `.knsv` zip files is not implemented (a possible follow-up).
