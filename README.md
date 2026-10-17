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
kast packages --list  # what would be installed, and from which list (no root needed)
kast status           # detected apps, Howdy, KDE, Hyprland
kast log              # where the log is; --tail 50, --open, --copy
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

## The package list

What `kast packages`, the automatic run and menu options 1 and 4 install is
[`packages.toml`](packages.toml): groups (official, CachyOS, kud3n, add-ons) of packages,
each with optional rules (`fallback`, `replaces`, `requires`, `description`).

- **Online first.** Every run downloads the file from this repo's `main` branch (curl,
  HTTPS only, 15 s at most). So an edit to `packages.toml` on `main` reaches every user on
  their next run, with no release needed.
- **Built-in fallback.** Each kast binary carries the file from the commit it was built
  from. kast uses that copy, and says why, when curl is missing, the download fails or
  times out, or the downloaded file doesn't parse or fails validation. The downloaded
  file is treated as untrusted: names must be plain package names (no leading `-`, no
  spaces or shell characters) and the only repo prefix allowed is `archrepo/`.
- **Always shown first.** The app shows the source (online, built-in and why, or a local
  file), the full list and what differs from the built-in list before anything is
  installed. The CLI prints the source line before installing.
- **AUR builds stay pinned.** When pacman can't install a package, kast retries it with
  yay, but only for packages in the built-in list. A package that only the downloaded list
  names is never built from the AUR.

```sh
kast packages --list                         # source line, groups, rules, what's installed
kast packages --list --json                  # the same, for scripts
kast packages --offline                      # use the built-in list (also: kast auto, kast)
kast packages --list --packages-file ./packages.toml   # try an edit before pushing it
```

A fallback is a valid result: `kast packages --list` exits 0 and prints the reason.

**Changing the format.** `schema = 1` at the top of the file is the format version. Add
new optional fields freely: older kast versions ignore fields they don't know. Bump
`schema` only for a breaking change, and only after a kast release that understands the
new version is out. Older versions then use their built-in list and tell the user to
update. `cargo test` validates the file, and CI runs that check on every push that
touches it.

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
| Log file (menu, log, summary, progress when done) | `e` pager · `E` editor · `o` open with the default app · `f` show in the file manager · `y` copy the log · `Y` copy its path (`y`/`Y` also while a job runs) |

### Getting the log out

`y` copies this job's log on the progress and summary screens, and everything since kast
started on the menu and in the log viewer (ANSI colours removed; over 256 KiB only the end
is kept). It uses `wl-copy` (Wayland) or `xclip`/`xsel` (X11) as your user, and otherwise
the terminal's clipboard escape (OSC 52). That one works over SSH in kitty, foot, WezTerm,
Alacritty, Ghostty, recent Konsole and Windows Terminal, but not in GNOME Terminal; inside
tmux add `set -g set-clipboard on` to `~/.tmux.conf`. The Linux console has no clipboard: use `e` there.

`e` opens the log in `$KAST_PAGER`, `$PAGER` or `less` (`E`: `$VISUAL`, `$EDITOR`, `nano`)
on the terminal and returns to the app when you quit it. `o` and `f` need a desktop
session; over SSH or on the Linux console they say so. `kast log --open` / `--copy` do the
same from a script; without root `kast log` reads `~/.local/state/kast.log`, or
`/var/log/kast.log` when yours is empty.

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
