# kas — Kud3n's Arch Setup

Arch Linux / CachyOS post-install tool, ported from `arch-setup.sh`. Run without
arguments it opens a full-screen app (Ratatui) for the setup steps and the App Vault
backup/restore engine for app data, KDE Plasma, Hyprland and Howdy face models.
Every subcommand still works from scripts, with plain text output.

## Build & install

```sh
cargo build --release
sudo install -Dm755 target/release/kas /usr/local/bin/kas
```

Runtime tools: `pacman`, `rsync` (installed automatically when missing), plus
`yay`, `v4l2-ctl`, `howdy`, `blkid`, `findmnt` for the steps that use them.
Archives are read and written natively, so `zip`/`unzip`/`jq` are no longer needed.

## Usage

```sh
kas                  # full-screen app (re-runs itself through sudo)
kas import           # full-screen restore screen (also through sudo)
kas export           # full-screen backup screen (also through sudo)

kas auto             # upgrade, repo, packages, services (no partition, no import)
kas partition        # optional: /media/data + home folder symlinks
kas repo | packages | services
kas status           # detected apps, Howdy, KDE, Hyprland
kas backup all --no-media -o ~/all.zip
kas info ~/all.zip
kas restore ~/all.zip --app kde --dry-run
kas --help           # every command and flag
```

Run it from your own account (`sudo kas`, not from a root shell): the target user is
taken from `SUDO_USER`. The full-screen app and the setup commands need root and re-run
themselves through `sudo`, so the password is asked once, before anything starts.
Vault commands with arguments (`kas backup ...`, `kas restore FILE ...`) work as your
own user. The Howdy face model lives in root-only `/etc/howdy/models`: when it is part of
the job, sudo asks for the password once at the start; without a terminal to ask on, the
face model is skipped with a warning.

Scripted use is unchanged: subcommands with arguments never open the full-screen app,
and their output stays plain text when piped (`kas status | cat`, `kas info x.zip > out.txt`).
`kas` without a command and without a terminal prints the help instead of the app;
`kas import` / `kas export` without arguments and without a terminal keep their
line-based prompts.

Backups made by the old `arch-setup.sh` can be restored, and the other way round.

## The full-screen app

The header shows who kas acts for (user, UID/GID, home, hostname, root or user mode);
the bottom line lists the keys of the current screen and `?` shows all of them.
Errors never close the app: the action ends, you're back in the menu with a message,
and `l` opens the log (`/var/log/kas.log`). Below 60×20 it asks for a bigger window.
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
running), kas offers to remove it.
