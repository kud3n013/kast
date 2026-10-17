# kast — Kud3n's Arch Setup Tool

Arch Linux / CachyOS post-install tool, ported from `arch-setup.sh`. Run without
arguments it opens a full-screen app (Ratatui) for the setup steps and the App Vault
backup/restore engine for app data, KDE Plasma, Hyprland and Howdy face models.
Every subcommand still works from scripts, with plain text output.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/kud3n013/kast/main/install.sh | sh
```

This downloads the latest static binary for x86_64 or aarch64 from the GitHub releases,
checks its SHA-256 checksum and installs it to `/usr/local/bin/kast`. It uses `sudo`
only for that last copy, and only when the directory isn't writable.

To read the script before running it:

```sh
curl -fsSLO https://raw.githubusercontent.com/kud3n013/kast/main/install.sh
less install.sh
sh install.sh
```

Pin a version or install somewhere else:

```sh
curl -fsSL https://raw.githubusercontent.com/kud3n013/kast/main/install.sh | KAST_VERSION=v0.1.0 sh
curl -fsSL https://raw.githubusercontent.com/kud3n013/kast/main/install.sh | KAST_INSTALL_DIR="$HOME/.local/bin" sh
```

Uninstall (removes only the binary; it prints where the logs and backups are):

```sh
sh install.sh --uninstall
curl -fsSL https://raw.githubusercontent.com/kud3n013/kast/main/install.sh | sh -s -- --uninstall
```

Set `KAST_INSTALL_DIR` again when uninstalling from a custom directory.

## Build from source

```sh
cargo build --release
sudo install -Dm755 target/release/kast /usr/local/bin/kast
```

Runtime tools: `pacman`, `rsync` (installed automatically when missing), plus
`yay`, `v4l2-ctl`, `howdy`, `blkid`, `findmnt` for the steps that use them.
Archives are read and written natively, so `zip`/`unzip`/`jq` are no longer needed.

## Usage

```sh
kast                  # full-screen app (re-runs itself through sudo)
kast import           # full-screen restore screen (also through sudo)
kast export           # full-screen backup screen (also through sudo)

kast auto             # upgrade, repo, packages, services (no partition, no import)
kast partition        # optional: /media/data + home folder symlinks
kast repo | packages | services
kast status           # detected apps, Howdy, KDE, Hyprland
kast backup all --no-media -o ~/all.zip
kast info ~/all.zip
kast restore ~/all.zip --app kde --dry-run
kast --help           # every command and flag
```

Run it from your own account (`sudo kast`, not from a root shell): the target user is
taken from `SUDO_USER`. The full-screen app and the setup commands need root and re-run
themselves through `sudo`, so the password is asked once, before anything starts.
Vault commands with arguments (`kast backup ...`, `kast restore FILE ...`) work as your
own user. The Howdy face model lives in root-only `/etc/howdy/models`: when it is part of
the job, sudo asks for the password once at the start; without a terminal to ask on, the
face model is skipped with a warning.

Scripted use is unchanged: subcommands with arguments never open the full-screen app,
and their output stays plain text when piped (`kast status | cat`, `kast info x.zip > out.txt`).
`kast` without a command and without a terminal prints the help instead of the app;
`kast import` / `kast export` without arguments and without a terminal keep their
line-based prompts.

Backups made by the old `arch-setup.sh` can be restored, and the other way round.

## Upgrading from kas

This tool used to be called `kas`. After installing `kast`:

```sh
sudo rm /usr/local/bin/kas                     # the old binary
sudo rm -f /var/log/kas.log                    # old logs (optional)
rm -f ~/.local/state/kas.log
```

Backups made by `kas` restore fine with `kast`; the archive format did not change.
A temporary yay sudoers rule left by an old `kas` (`/etc/sudoers.d/zz-kas-yay`) is
removed the next time `kast` cleans up.

## The full-screen app

The header shows who kast acts for (user, UID/GID, home, hostname, root or user mode);
the bottom line lists the keys of the current screen and `?` shows all of them.
Errors never close the app: the action ends, you're back in the menu with a message,
and `l` opens the log (`/var/log/kast.log`). Below 60×20 it asks for a bigger window.
`NO_COLOR` turns colours off; on the Linux console (`TERM=linux`) symbols are ASCII.

| Screen | Keys |
| --- | --- |
| Everywhere | `?` help · `Ctrl+C` cancel / back / quit · mouse wheel scrolls |
| Menu | `↑/↓` `j/k` move (wraps) · `Home/End` · `Enter` run · `1`–`9` run directly · `l` log · `q`/`Esc` quit · click selects, double-click runs |
| Progress | `PgUp/PgDn` `↑/↓` scroll (pauses following) · `g`/`G` oldest / follow · `Esc`/`q` cancel the job (asks first) · when done: `Enter` back, `l` full log |
| Questions | `y`/`n` · `←/→` `Tab` choose · `Enter` the highlighted button · `Esc` always answers No |
| Summary | `Enter` back to menu · `l` full log |
| Import / Inspect | `↑/↓` move · `Enter` choose · `/` filter · `c` type a path (`Tab` completes, paste works) · `Esc` back |
| Restore options | `↑/↓` move · `Space` select / toggle · `Enter` choose · `s` start · `Esc` back |
| Export | `↑/↓` move · `Space`/`Enter` select, toggle or edit the path · `s` start · `Esc` back |
| Status | `r` refresh · `↑/↓` scroll · `Esc` back |
| Log | `PgUp/PgDn` `↑/↓` scroll · `g`/`G` top / end · `Esc` back |

Canceling a running job stops the command it is running (SIGTERM to its process group,
then SIGKILL) and ends the job at its next step; steps that already finished are not
undone. If a stopped pacman leaves `/var/lib/pacman/db.lck` behind (and no pacman is
running), kast offers to remove it.

## Releasing

1. Bump `version` in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows), then commit.
2. Tag and push the tag:

   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

The Release workflow checks that the tag matches `Cargo.toml`, runs the tests, builds
static musl binaries for x86_64 and aarch64 and publishes them with their checksums.
Tags with a `-` (`v0.2.0-rc1`) become prereleases.
kast carries a copy of archrepo's signing key (`assets/archrepo.asc`, imported by the Repo
step). If archrepo's key ever changes, replace that file and `REPO_KEY_FPR` in
`src/setup.rs`, then cut a new release. Running the workflow by hand
(Actions → Release → Run workflow) builds everything without publishing.
