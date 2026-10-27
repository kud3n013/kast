# kast — Kud3n's Arch Setup Tool

Arch Linux / CachyOS post-install tool, ported from `arch-setup.sh`. Run without
arguments it opens a full-screen app (Ratatui) for the setup steps and the App Vault
backup/restore engine for app data, KDE Plasma, Hyprland and Howdy face models, plus
`kast plasma` profiles for your Plasma settings.
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

kast auto             # upgrade, repo, packages, services (--no-snapshot: no btrfs snapshot first)
kast partition        # pick a partition, see the fstab line, test-mount, install, mount (undoable)
kast partition --list # the partitions and why a row can't be used (no root needed; --json too)
kast repo | packages | services
kast packages --list  # what would be installed, and from which list (no root needed)
kast undo --list      # what kast changed, run by run; kast undo <run> / --last undoes it
kast uninstall steam  # remove apps of the list (keeps what something else needs)
kast status           # detected apps, Howdy and the IR emitter, KDE, Hyprland, keyrings
kast ir-emitter probe # the IR camera's face-authentication control (read-only; see below)
kast charge           # battery: charge mode, overnight charging (Lenovo; see "Battery charging" below)
kast log              # where the log is; --tail 50, --open, --copy
kast backup all --no-media -o ~/all.zip
kast backup all --upload proton   # back up to Proton Drive (see "Proton Drive" below)
kast backup keyrings  # KDE Wallet + GNOME Keyring (opt-in; see "Keyrings" below)
kast backup ollama    # Ollama's models (not in all; see "Ollama models" below)
kast drive login      # sign in to Proton Drive (link + QR code); drive status / logout
kast schedule enable --targets all --every daily   # back up to Proton Drive on a timer
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

When a backup or a restore closes running apps (`-k`, or yes to its question), kast opens them again in the
background once it is done: Vesktop goes to its tray, other apps start minimized on
Plasma (`kstart --iconify`) or on a hidden special workspace on Hyprland, and as before
anywhere else. `--no-relaunch` leaves them closed. A restore only force-kills an app that ignores the
close request when you pass `-k`; answering yes to the question closes apps gracefully and stops if one stays open.

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
- **archrepo's packages need no entry.** kast also downloads archrepo's signed package
  database (`archrepo.db` and its signature, in the same job as the list) and adds every
  package the file doesn't mention. One that depends on a listed package becomes an add-on
  of it; any other goes to the kud3n group. They are marked "from archrepo, not in
  packages.toml" in the app and in `--list`, and the source line counts them ("36 packages
  + 6 from archrepo"). The signature is checked with `gpgv` against the key kast ships, in
  a temporary keyring; if the download, the signature or the data is bad, or you're
  offline, nothing is added and the list is as the file gives it. They're installed as
  `archrepo/<name>`, are never built from the AUR, and are left out of the picker while
  `[archrepo]` isn't in `pacman.conf` and the run has no repository step. An entry in the
  file overrides the automatic one (group, `description`, `requires`, `replaces`).
  Opt out with an optional top-level table, which needs no `schema` bump:

  ```toml
  [archrepo]
  auto = true                  # merge archrepo's packages (default true)
  exclude = ["some-package"]   # never added automatically; explicit entries still count
  ```

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
| Menu | `↑/↓` `j/k` move (wraps) · `Home/End` · `Enter` run · `1`–`9` `s` `u` `x` run directly · `l` log · `q`/`Esc` quit · click selects, double-click runs |
| Progress | `PgUp/PgDn` `↑/↓` scroll (pauses following) · `g`/`G` oldest / follow · `Esc`/`q` cancel the job (asks first) · when done: `Enter` back, `l` full log |
| Questions | `y`/`n` · `←/→` `Tab` choose · `Enter` the highlighted button · `Esc` always answers No |
| Summary | `Enter` back to menu · `l` full log |
| Checklists (steps, apps) | `↑/↓` `j/k` move (skips greyed items) · `Home/End` `g/G` `PgUp/PgDn` · `Space` toggle (on a group header: the group) · `a` all · `n` none · `i` invert · click toggles · `Enter` continue · `Esc` back (choices are kept) |
| Install packages (4) | the checklist · `r` download the list again after a fallback · `Enter` asks, then installs what's checked |
| Data partition (2, or a stage of the automatic run) | list: `↑/↓` move · `Enter` pick (greyed rows say why they can't) · `r` scan again · mount point: type it, `Tab` completes, `Enter` checks it · options: `Space` change · `←/→` cycle · `c` continue · folders: `Space` · `Enter` continue · review: `s` start · `Esc` back one stage |
| Review (automatic run) | `←/→` `Tab` Start / Back · `Enter` the highlighted button · `s` start · `Esc` back |
| Import / Inspect | source: `Enter` Local folder or Proton Drive (`r` re-check, `L` sign out) · folder: type it, `Tab` completes, `Enter` lists it · Proton Drive: machines, then their backups · files: `↑/↓` move, `Enter` choose (Proton Drive: download, `Esc` stops it), `/` filter · `Esc` back one stage |
| Restore options | `↑/↓` move · `Space` select / toggle · `Enter` choose · `s` start (automatic run: use this) · `Esc` back |
| Export | source and folder as for import (a missing folder is created as you, after asking; `s` on the sources opens the scheduled backup) · then `↑/↓` move · `Space`/`Enter` select, toggle or edit the file name · `s` start (automatic run: use this) · `Esc` back |
| Sign in to Proton Drive | `y` copy the link · `o` open it in the browser · `Esc` cancel (asks first) · failed: `Enter` try again |
| Scheduled backup (`s`) | `↑/↓` move · `Space` toggle a target, the frequency or the local copy · `←/→` how many to keep · `Enter` on Turn on / Turn off / Run now · `r` refresh · `Esc` back |

### Automatic run, step by step

Menu option 1 first shows the steps of a full setup, in the order they run:

1. Data partition (off by default): pick the partition, mount point, options and home folders while configuring
2. Export data (off by default): before anything below changes your apps
3. System upgrade · 4. kud3n's repository · 5. Install packages · 6. Services (all on by default)
7. Import data (off by default): last, once the apps are installed and `/media/data` is mounted

The defaults are `kast auto`'s run (the data partition stays off until you check it). `Enter` then goes through a stage for each checked step
that needs choices: the apps to install (all checked; an add-on can only be picked while its
app is checked or already installed), the backup to restore and its options, what to back up
and where. A review lists everything; `Start` runs it unattended. The overall progress bar
counts only the chosen steps, and the summary marks the others "skipped".

Menu option 4 uses the same app picker, then asks before installing. On the command line,
`kast packages --only a,b` / `--skip a,b` do the same; `kast auto` and `kast packages` without
them install everything, as before.
| Status | `r` refresh · `↑/↓` scroll · `Esc` back |
| Undo (`u`) | runs: `↑/↓` move, `Enter` open, `p` prune · changes: the checklist · preview: `←/→` Start / Back, `s` start · `Esc` back one stage |
| Uninstall (`x`) | the checklist (nothing checked to start with) · `Enter` preview · `s` start |
| Log | `PgUp/PgDn` `↑/↓` scroll · `g`/`G` top / end · `Esc` back |
| Log file (menu, log, summary, progress when done) | `e` pager · `E` editor · `o` open with the default app · `f` show in the file manager · `y` copy the log · `Y` copy its path (`y`/`Y` also while a job runs) |

Checking the system upgrade adds a stage that says plainly that upgrades can't be undone by
kast, and offers a snapshot first when the machine can take one (see below).

## Face recognition: IR emitter

Many Windows Hello laptop cameras keep their IR emitter (the red LED) off on Linux, so Howdy
sees dark frames. Windows switches it on through the face-authentication control of
Microsoft's camera extension unit ([UVC 1.5 extensions](https://learn.microsoft.com/en-us/windows-hardware/drivers/stream/uvc-extensions-1-5))
right before the IR stream starts; the camera's firmware then strobes the emitter. kast does
the same, natively, through uvcvideo.

The **Services** step (`kast services`, `kast auto`, the app's step 6) does it as part of the
Howdy setup:

- it checks the IR camera it finds (read-only) and sets Howdy's `device_path` to the camera's
  stable `/dev/v4l/by-path/...` name, and raises `dark_threshold` to 90 (a higher value
  you set is kept), so frames between the emitter's flashes don't stop a match;
- it gives `/etc/howdy` and `config.ini` Howdy's own modes (0750, 0640). Lock screens check
  your face as you, and Howdy only hands them the root-only face model through its setuid
  helper when you can't read `config.ini`; a world-readable one (some packages ship it so)
  makes unlocking fall back to the password;
- when the camera qualifies, every PAM file kast gives Howdy (sudo, polkit-1, system-local-login,
  plasmalogin, sddm, gdm-password, hyprlock, swaylock, kde) gets this line directly above
  Howdy's. A file whose auth stack already includes one with Howdy (Arch's `kde` includes
  `system-local-login`) is left alone, so Howdy doesn't scan twice:

  ```
  auth      optional   pam_exec.so quiet /usr/local/bin/kast ir-emitter enable --device /dev/v4l/by-path/...
  ```

  so the emitter is on for every sudo, login, polkit prompt and unlock. It is `optional`:
  if it fails, Howdy just runs as before and the password still works. A hand-written
  `pam_exec` line running `ir-msxu`, or an older kast line, is replaced, never doubled;
- the summary says "IR emitter: switched on before Howdy (alternate)", "not supported by this
  camera", or why it wasn't set up.

PAM runs that command as root, so kast only adds the line when its binary and every folder
above it belong to root and no one else can write them (`/usr/local/bin` from `install.sh`
is fine, `~/.local/bin` isn't: the summary says so and how to fix it).

**Which cameras qualify:** a USB (UVC) camera whose IR function has the Microsoft extension
unit with the face-authentication control, that the camera lets kast write and that
advertises a mode (alternate frame illumination, else background subtraction). kast only
sends a mode the camera lists and never touches other bits; nothing is hardcoded per model.

```sh
kast ir-emitter probe            # what the camera says and what enable would do (--json too)
sudo kast afm --ir               # add a face model with the emitter on (kast add-face)
sudo kast ir-emitter run -- howdy -U $USER test
kast ir-emitter restore          # write the camera's default back
```

`run` is for Howdy commands that don't go through PAM; the camera switches the emitter off
by itself a few seconds after the stream stops. **Face models added before the emitter
worked were recorded in the dark**: add a fresh one with `sudo kast afm --ir` (it adds a
model next to the old ones; `howdy remove` / `howdy clear` drop old ones). Without `--ir`,
`kast afm` is plain `howdy -U <you> add`; `--user` picks another account.
`kast status` shows the emitter's state.

Undo (`sudo kast undo --last --step services`, or Undo changes in the menu) puts the PAM
files, `device_path` and the config modes back as they were, like the rest of the Howdy setup.

## Battery charging (Lenovo IdeaPad)

`kast charge` reads and switches the two charging controls of a Lenovo IdeaPad:

- **Charge mode:** standard, fast (rapid charge) or conservation (stops at about 80%), one at
  a time. kast writes the kernel's `charge_types` (`ideapad_laptop`), so it works on any
  laptop whose battery has it and needs nothing else.
- **Overnight charging** holds the battery at about 80% while it is plugged in at night, on
  top of whatever charge mode is set. The kernel driver doesn't know about it: a battery that
  stops at 80% while every Linux setting says it shouldn't ("Not charging", 0 W) is usually
  this. It lives in the embedded controller, and kast reaches it with Lenovo's ACPI method
  `VPC0.BNMC` through `acpi_call` (`acpi_call-dkms` on CachyOS kernels, `acpi_call` for the
  stock `linux` kernel). ACPI methods are model-specific, and these values were confirmed on
  the Lenovo 83D3 (IdeaPad Pro 5 14AHP9 / XiaoXin Pro 14 AHP9) only: elsewhere kast refuses
  unless you pass `--force`.

```sh
kast charge                      # status: level, power, mode; overnight charging with sudo (--json too)
kast charge mode conservation    # or -c; standard / -s, fast / -f
kast charge overnight off        # or kast charge o9 off
```

Switching runs through sudo. kast reads the state back after each write and reports what the
hardware says, trying again when the embedded controller is busy (on this generation it is
shared with the USB-C driver, so a write can time out). When a plugged-in battery below 95%
isn't charging, the status says what is holding it back.

Overnight charging is stored in the embedded controller, so it stays across reboots and
operating systems: Lenovo Vantage or PC Manager on Windows can switch it on again. After a
change the controller doesn't start charging again above 95%.

## Keyrings (KDE Wallet and GNOME Keyring)

The Chromium and Electron apps kast backs up (Helium, Beeper, Vesktop, Obsidian, Antigravity,
Zalo) don't keep their saved passwords, cookies and tokens readable in their profile: they
encrypt them with a key that lives in the desktop keyring. Restore a profile next to an empty
keyring and the app signs you out anyway. Wi-Fi passwords saved "for this user only" and other
Secret Service secrets (proton-drive's session, unless it uses `pass`) live there too. So kast
can carry the keyrings along:

```sh
kast backup keyrings                # both; or kast backup kwallet / gnome-keyring
kast backup all --with-keyrings     # all, keyrings included
kast restore ~/all.zip --app keyrings --dry-run
```

| Target | What goes in the archive |
|---|---|
| `kwallet` (`kdewallet`, `wallet`) | `~/.local/share/kwalletd`: each wallet's `<name>.kwl` with its `<name>.salt` (a PBKDF2 wallet can't be opened without it) and `<name>_attributes.json` (KWallet 6's Secret Service attributes), plus `~/.config/kwalletrc` |
| `gnome-keyring` (`gkr`, `seahorse`) | every regular file in `~/.local/share/keyrings`: `<name>.keyring`, `default`, `user.keystore` |

**Opt-in.** They're the most sensitive thing kast carries, and `all` also feeds the scheduled
Proton Drive backup, so `all` leaves them out unless you pass `--with-keyrings` or set it for
good (the schedule then follows):

```toml
# ~/.config/kast/settings.toml
[vault]
keyrings = true
```

In the backup screen both are listed, and `all` has an "Include keyrings" option that is off
unless the setting is on.

**What kast does and doesn't do.** It copies the encrypted files as they are on disk; it never
asks for, reads, stores or logs a keyring password, and never opens or unlocks a keyring. Only
the file headers are read, to tell the formats apart, and the manifest lists each keyring's name,
type and size, never its content. The files stay 0600 in a 0700 folder in the archive (itself
0600) and when restored.

**Brute force.** Encrypted keyrings stay encrypted inside the zip, but anyone with the archive
can try passwords against them offline as long as they like: KWallet derives its key with
PBKDF2-SHA512, GNOME Keyring with iterated SHA-256 (a few thousand rounds). A weak keyring password can be
cracked. Keep the archive private, and give the keyring a real password.

**Plain-text keyrings.** A GNOME keyring with an empty password is saved as a plain-text file:
every secret in it is readable. kast never puts one in a Proton Drive upload or a scheduled run.
A local backup leaves it out too unless you say so (`--allow-plaintext-keyring`, or "Back up
anyway" in the backup screen). Better: set a password in Seahorse (Passwords and Keys: right-click
the keyring, Change Password). (KWallet has no unencrypted format.)

**Which store an app uses.** It depends on the desktop it runs in. Chromium's os_crypt (and the
Electron apps built on it) talks to KWallet (`org.kde.kwalletd6`) when `XDG_CURRENT_DESKTOP` says
KDE, and to whatever owns `org.freedesktop.secrets` (usually gnome-keyring) everywhere else,
Hyprland included; `--password-store=` in an app's flags file overrides that. If you use both
Plasma and Hyprland, the same app can have keys in both stores, which is why both are backed up.
On KWallet 6, `kwalletd6` itself forwards to a Secret Service: ksecretd's wallet files by default,
or the `org.freedesktop.secrets` owner when kwalletrc has `[KSecretD] Enabled=false`. Only one
program can own `org.freedesktop.secrets`; the backup records which one did, and a restore warns
when another one does now (libsecret apps then look in the other store). `kast status` shows the
keyrings, the owner and the PAM setup.

**Restoring** puts the keyrings in before every other app (an app started first could ask for its
secret and get a freshly created empty keyring), keeps keyrings of the new system that aren't in
the archive, and snapshots the ones it replaces (a fresh install's first login may already have
made an empty `kdewallet` or `login.keyring`; `kast undo` puts them back). It stops the daemon
first, so it can't write the old keyring from memory over the restored files: ksecretd and
kwalletd6 are asked to quit (they start again on demand through D-Bus), and gnome-keyring-daemon
is stopped through its systemd user units, or with SIGTERM when PAM's `auto_start` or Hyprland's
`exec-once` started it, and started again the same way. A daemon that won't stop aborts that
keyring's restore with nothing written. kwalletrc comes back with `Enabled=true` and
`First Use=false`, so the first-run wizard can't create a new wallet over yours; when the archive
also has the `kde` target, the kwalletrc that came with the wallets is the one restored.

**Passwords after a restore.** PAM unlocks a keyring at login with your login password:
`pam_kwallet5.so` (package `kwallet-pam`) the wallet named `kdewallet` (Blowfish, PBKDF2),
`pam_gnome_keyring.so` (package `gnome-keyring`) the `login` keyring, each needing an `auth` and a
`session` line in your display manager's PAM file (`/etc/pam.d/sddm`, `plasmalogin`,
`gdm-password`, `greetd`, `ly`, `login`, ...). Log out and back in after the restore. If your new
login password matches the old keyring password it unlocks by itself; otherwise you're asked for
the old password once. Set the keyring's password to your login password (KWalletManager,
Seahorse) and the automatic unlock works from then on. kast says so after the restore, and warns
when the PAM module or its package isn't set up. A default wallet that isn't `kdewallet`, or a
default keyring that isn't `login`, is never unlocked by PAM; the backup warns about that.

**GPG wallets.** A KWallet can be encrypted with a GPG key instead of a password. It opens on the
new system only with the matching private key in `~/.gnupg`, which kast doesn't back up. The
backup warns, the manifest records the key ID, and the restore checks `gpg --list-secret-keys`
for it.

## Ollama models

`kast backup ollama` carries the models Ollama has downloaded: the store's `blobs/` (weights,
templates, parameters) and the manifests that name the models (`manifests/`, and Ollama 0.40's
`manifests-v2/`). It finds the store in kast's own `OLLAMA_MODELS`, then `~/.ollama/models` (an
`ollama serve` you run yourself), then the store of the installed `ollama.service`
(`/var/lib/ollama` with Arch's package, `/usr/share/ollama/.ollama/models` with Ollama's own
installer). The server's key (`id_ed25519`), its caches and unfinished downloads stay out.

Models are gigabytes that can be downloaded again, so they are **not in `all`** (nor in a
schedule of `all`); name `ollama` to back them up or schedule them. The weights go into the zip
uncompressed (they don't compress), so a backup takes about as long as copying them.

- `--no-media` leaves the weights out and keeps the manifests and small blobs (a few KB). The
  restore then runs `ollama pull` for each model whose weights are missing (with the server
  running), and keeps a manifest only for models that are complete, so `ollama list` never
  shows a model that can't run.
- A restore adds the archived models to the store and leaves the others alone. The system store
  belongs to the `ollama` user: kast writes it through sudo (the password is asked once, as for
  the face model) and hands the files to that user; without a terminal to ask on, the models are
  skipped with a warning. The safety snapshot holds the manifests; undo puts them back, and
  Ollama removes the weights no model uses any more the next time it starts.
- `kast info` lists the models in an archive and whether their weights are in it.

## KDE Plasma settings

`kast plasma` (menu key `p`) saves your Plasma settings as named profiles, applies them again
and moves them between machines as a `.kast-plasma.tar.gz`. It is a Rust port of the logic of
[konsave](https://github.com/Prayag2/konsave) (see [Credits](#credits-and-license) and
`docs/konsave-notes.md`), with a safer apply. It works as your own user, no sudo.

```sh
kast plasma save work                      # profile of what exists in ~/.config (add --assets for themes, icons, plasmoids)
kast plasma list
kast plasma apply work --dry-run           # exactly what would be created, overwritten or skipped
kast plasma apply work --only panels,shortcuts --restart-plasma
kast plasma export work -o ~/Downloads/    # -> work.kast-plasma.tar.gz
kast plasma import ~/Downloads/work.kast-plasma.tar.gz --name laptop --dry-run
kast plasma deps laptop --install          # themes / icon themes / plasmoids it needs, via pacman
kast plasma config-check                   # the list against ~/.config and ~/.local/share/plasma
kast plasma remove work
```

**Categories** (for `--only` and the checkboxes in the app): `appearance` (themes, colors,
icons, splash, lock screen), `shortcuts` (global and per-app shortcuts, with the launchers and
commands they run), `panels` (panels, widgets, plasmoids), `windows` (KWin, rules, screens,
your KWin scripts and effects), `input`, `apps` (Dolphin, Konsole, Kate, KRunner...), `gtk`, `autostart`.

**Safety**

- `apply` builds one plan, then runs exactly that plan; `--dry-run` only prints it. Before it
  overwrites anything it snapshots those files as a profile named `pre-apply-<date>-<time>`
  (undo with `kast plasma apply pre-apply-...`). It never deletes files, and skips (with the
  reason) a folder in the way or a symlink pointing outside.
- Writes are atomic (a temp file in the same folder, then a rename); modes and times are kept
  and symlinks stay symlinks.
- Where files go comes only from your list (below), never from a profile or archive: a hostile
  archive cannot make `apply` write to `~/.ssh`. Archives are unpacked defensively: no absolute
  or `..` paths, no writing through symlinks, no hard links or devices, no links leaving their
  section, and an existing profile is only replaced with `--force`. `import` never touches your
  live settings; only a confirmed `apply` does.
- `save --force` replaces a profile (it is rebuilt, not merged). Settings that look like
  credentials (a calendar widget's token in `plasma-org.kde.plasma.desktop-appletsrc`) are flagged
  on save and export, by key name only.
- Plasma rewrites its settings when it quits, so files restored while it runs can be written
  over. `--restart-plasma` (the app has a checkbox) quits plasmashell, applies, then starts it
  (`kquitapp6`/`kstart`, or the `5` ones on Plasma 5). A log out and in is the most reliable;
  KWin settings need that or a KWin reconfigure.
- Each profile has a `meta.toml` (kast and Plasma version, host, date, the kast commit of the
  built-in list and the packages it needs). Profiles live in `~/.local/share/kast/plasma/profiles`.

**The list** is TOML, built into kast (`kast plasma default-config` prints it). Put your own at
`~/.config/kast/plasma.toml` to replace it:

```toml
[[section]]
id = "firefox"                  # lowercase letters, digits, dashes
category = "apps"               # appearance shortcuts panels windows input apps gtk autostart
kind = "save"                   # "save": small settings; "export": large assets (--assets / export)
location = "$HOME/.mozilla/firefox/${ENDS_WITH='.default-release'}"
entries = ["chrome", "user.js", "plasma*rc"]   # * and ? in the last component
```

Placeholders: `$HOME`, `$CONFIG_DIR`, `$SHARE_DIR`, `$BIN_DIR`, `${ENDS_WITH="x"}`,
`${BEGINS_WITH="x"}` (a whole path component, sorted, several matches warn). A location must be
inside your home. Entries that do not exist on a machine are skipped.

## Proton Drive

kast backs up to and restores from Proton Drive with Proton's own command-line tool,
`proton-drive`, run as you (never as root). kast never sees your password: you sign in in
a browser, on this computer or any other device.

**Requirements.** `proton-drive` 0.9.0 or newer, on your `PATH` or in `~/.local/bin`. When
it's missing, the Proton Drive row says so and Enter installs it from kud3n's repository if
it's there, or shows how to install it by hand: download it from
<https://proton.me/download/drive/cli/index.html>, pick `linux/x64` (or `linux/x64-baseline`
on a CPU without AVX2; kast says which), `chmod +x` it and put it in `~/.local/bin`. kast
works without it; only Proton Drive needs it. Proton offers the CLI for some plans (Proton
for Business); an account that can't use it gets Proton's own error.

**Signing in.** In the app, Enter on "Proton Drive · not signed in" shows the sign-in link,
a QR code to scan with your phone (when the window is big enough), and a 10-minute countdown;
`y` copies the link, `o` opens it. On a terminal: `kast drive login`. `kast drive status`
(`--json`) says whether it's installed and signed in; `kast drive logout` (or `L` on the
row) removes the session and proton-drive's local caches. The sign-in link carries key
material: kast never writes it to the log.

**The keyring.** proton-drive keeps its session in your desktop keyring (KWallet or GNOME
Keyring). On a TTY or over SSH without an unlocked keyring the row says "keyring locked".
If `pass` is set up (`~/.password-store/.gpg-id`), kast offers to keep the session there
instead (`kast drive store pass`); switching means signing in again.

**Where backups go.** `/my-files/kast-backups/<hostname>/` on Drive, as
`<target>_backup_<date>_<time>.zip` (a face model: `<user>_face_model_<date>_<time>.dat`),
each with a `….manifest.json` sidecar listing what's inside, its size and sha256. Uploads
never replace anything on Drive. The backup is built in a private folder
(`~/.cache/kast/proton-upload`, 0700) and removed after the upload, unless you keep a local
copy (`--also-local DIR`, or the option on the export screen).

**Unchanged data isn't uploaded again.** Each backup has a content fingerprint (the files
inside and their checksums, not the time it was made). When it matches the last upload, kast
asks first (`--force-upload` uploads anyway; scheduled runs skip it).

**Retention.** kast keeps the newest 5 backups per target and machine (`--keep N`,
`proton.keep`). Older ones and their sidecars go to Proton Drive's trash, where they can
still be restored; kast never deletes them for good or empties the trash. The first time,
it asks.

**Restoring.** The import screen's Proton Drive source lists the machines (this one first),
then a machine's backups with what's inside, read from the sidecars without downloading the
archives. Enter downloads the chosen one into a private folder
(`~/.cache/kast/proton-restore`, 0700), checks it against its sidecar's sha256, then
continues as for a local backup. The downloaded copy (decrypted app data) is wiped after the
restore, when you leave the screen, if the check fails, and when kast exits. Inspect (9)
shows a backup's contents from its sidecar.

**Scheduled backups.** Menu `s` (or `kast schedule enable --targets all --every daily
[--keep N]`) sets up a systemd user timer (`~/.config/systemd/user/kast-backup.{service,timer}`)
that runs `kast backup <targets> --upload proton --scheduled`, daily or weekly. It runs as
you **while you're logged in** (that's when your keyring is unlocked); a run missed while the
computer was off happens at your next login (`Persistent=true`). A scheduled run never asks
anything: apps that are running are left out, never closed (their databases may be
mid-write, so an app you always keep open is rarely backed up this way: check "Left out" in
the status); the face model isn't included (it needs sudo); nothing is uploaded when nothing
changed; "all" uploads as `apps_backup_…` so its retention never touches your manual "all"
backups. The result goes to `~/.local/state/kast/schedule.json` and the log, and a failure
shows a desktop notification saying what to do (e.g. "Run: kast drive login").
`kast schedule status` (`--json`), `run` and `disable` do what they say; status also notices
when the timer points at a kast that moved.

**Exit status 3** from `kast backup … --upload proton`, `kast drive status` and scheduled
runs means Proton Drive isn't ready: not installed, not signed in, keyring locked or offline.

**Undo.** Uploads made from the app (or with sudo) are recorded: `kast undo` offers to move
them to Proton Drive's trash (unchecked to start with). Turning the schedule on is recorded
too; undoing it removes the timer.

**Settings** live in `~/.config/kast/settings.toml` (yours, created the first time something
is changed; unknown keys are kept):

```toml
[proton]
credentials_store = "pass"            # or "keychain" (the default)
folder = "/my-files/kast-backups"     # where backups go on Drive
keep = 5                              # backups kept per target and machine
also_local = "/media/data/backups"    # scheduled runs keep a local copy here too

[schedule]
notify_success = false                # also notify when a scheduled run worked
```

### Settings that are "not writable"

KDE says `Configuration file "~/.config/spectaclerc" not writable` when a config file is not owned
by you or lacks owner write. kast restores KDE settings and app data as your user and always
leaves owner read/write, then checks every restored entry and reports anything it cannot fix.
If an older kast (before 0.6.1) already left such files behind, repair them with:

```sh
kast fix-permissions --dry-run    # list what is wrong
sudo kast fix-permissions         # fix it (sudo only to chown root-owned files)
```

It checks the folders themselves (`~`, `~/.config`, `~/.local/share`: an old restore could leave them root-owned)
and the KDE and app settings kast manages, never follows symlinks and never leaves your home.

## Data partition

Menu `2` (or the first step of the automatic run) mounts any partition you pick. It lists the
disks with `lsblk -J -b` (cross-checked with `blkid -p`); rows that can't be used are dim and
say why: in use by the system or as swap, the EFI partition, no filesystem or UUID, encrypted
(LUKS/BitLocker), read-only media, a filesystem the kernel has no driver for. Supported:
NTFS (ntfs3, or ntfs-3g when the kernel driver is missing), exFAT, FAT, ext4, btrfs, XFS.

You pick the partition, the mount point (checked: absolute, not a system folder, not an
existing mount point, empty), the options, and optionally home folders to move onto it. The
options stage shows **the exact line** that will be added to `/etc/fstab`, live. Examples:

```
UUID=6C62ADEA62ADB96A /media/data ntfs3 uid=1000,gid=1000,dmask=0077,fmask=0177,iocharset=utf8,windows_names,nofail,x-systemd.device-timeout=10s 0 0
UUID=A40D-85E7 /media/stick exfat uid=1000,gid=1000,dmask=0077,fmask=0177,nofail,x-systemd.device-timeout=5s,x-systemd.automount,x-systemd.idle-timeout=5min 0 0
UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/backup ext4 nofail,x-systemd.device-timeout=10s 0 2
UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/pool btrfs subvol=/@data,compress=zstd:1,nofail,x-systemd.device-timeout=10s 0 0
UUID=8b1e2f3a-aaaa-bbbb-cccc-0123456789ab /media/data xfs nofail,x-systemd.device-timeout=10s 0 2
```

What the lines follow (see the comments in `src/partition/fstab.rs` for the man-page sources):

- six explicit fields; spaces, tabs and backslashes escaped as `\040`, `\011`, `\134`; `UUID=`
  exactly as `lsblk` reports it, `PARTUUID=` only when there is no UUID;
- `nofail` always (a missing drive never stops the boot) and `x-systemd.device-timeout=` 10 s
  (internal) or 5 s (external); never `defaults`, `exec`, `force` or `umask=000`;
- external drives are mounted on first access (`x-systemd.automount`, unmounted after 5 min
  idle) so an unplugged drive doesn't wait at boot; a toggle switches to mounting at boot;
- passno 0 for NTFS, exFAT, FAT and btrfs; 2 for ext4 and XFS; never 1;
- NTFS/exFAT/FAT have no Unix owners: `uid`/`gid` and the permission preset (*Only me* `0077/0177`,
  *Me, others can read* `0022/0133`, *Me, files executable* `0077/0077`). ext4, btrfs and XFS get
  no `uid`/`gid`; "Make the top folder yours" changes the owner of the mount point folder only.

Applying it is a sequence that stops and puts things back at the first failure: re-check that the
device is the one you saw; **test-mount** it read-write on a private folder (a dirty NTFS from
Windows Fast Startup is explained, not forced); write a candidate fstab and run
`findmnt --verify` on it (only *new* problems block); write `/etc/fstab` atomically, keeping a
copy; `systemctl daemon-reload`; mount **only that entry** (never `mount -a`) and check it came up
as expected and is writable as you. If any step fails the exact original bytes of `/etc/fstab`
are restored, the test mount is unmounted and the folders kast made are removed. Moving home
folders copies first and replaces a folder with a link only after a complete copy.

```sh
kast partition --list [--json]
kast partition --device LABEL=Data --mountpoint /media/data --dry-run
sudo kast partition --device /dev/sdb1 --mountpoint /media/stick --external --move-folders Documents,Videos
```

Undo (`u`, or `sudo kast undo --last --step partition`) reverses it in this order: link → real
folder copied back (only if the link is still the one kast made and there is room; the copy on the
partition is never deleted), `user-dirs.dirs` restored, the mount folder's owner given back,
unmount (a busy mount names the processes and stops the undo; never lazy or forced), the `fstab`
line removed (the lines it replaced are put back; a file you edited since only loses kast's own
lines), `systemctl daemon-reload`, and the mount point folder removed if kast made it and it is empty.

## Undo and uninstall

Everything kast changes on the system is recorded in **`/var/lib/kast/`** (root only: the
folder is 0700, files 0600): `journal.jsonl`, one JSON line per change, written *before* the
change with what was there before and again after it, and `files/<run>/<sha256>`, copies of
the files kast changed. A journal or copy that isn't root's or that others can write is
refused, never used. A change that was interrupted (a crash, a power cut) shows as
"incomplete" and is undone from what was there before it.

Menu `u` **Undo changes** lists the runs newest first → the changes of one → a preview of
exactly what will run, in order → a job. What it can undo, newest first:

| Change | Undo |
| --- | --- |
| Packages kast installed | `pacman -Rns`, first (archrepo's packages go before `[archrepo]`); packages something else needs now, or that you marked explicitly installed since, are kept and say why |
| Legacy packages a swap removed (`sine-bin`, `zed`, ...) | offered for reinstall, unchecked |
| `/etc/pacman.conf`, the archrepo key | the file restored (or just the `[archrepo]` block taken out if you edited it since); the key removed if kast imported it; packages still installed from archrepo are listed (they stop getting updates) and can be removed too |
| PAM files, the Howdy resume hook, `howdy set`, services | files restored or deleted; then the PAM files are checked (they parse, their modules and includes exist, `sudo` keeps an auth stack): if not, they're put back as they were and the undo stops, so `sudo` keeps working; values set back; only units kast enabled are disabled |
| A data partition | the folders copied back, the `fstab` line taken out (and the line it replaced put back), unmounted, the mount point removed if kast made it and it is empty; a busy mount stops the undo and names the processes |
| A restore | put back from its safety snapshot (as your user). Without a snapshot it can't be undone and shows greyed out |
| A backup | "delete the backup file", unchecked |
| The system upgrade | never undone by kast; with a snapshot the summary and the undo list give the rollback commands |

A file changed since kast wrote it is flagged with a short diff: restore it anyway or keep
yours (`kast undo` skips it unless `--force`). An undone change is marked as such and isn't
undone twice.

Menu `x` **Uninstall apps** lists the list's installed apps, marked "installed by kast" (with
the date) or "installed before kast / outside kast", none checked; removal keeps anything
another package needs. Uninstalls are recorded too.

```sh
sudo kast undo --list [--json]              # runs and their changes (needs sudo: /var/lib/kast is root's)
sudo kast undo <run> [--step services] [--dry-run] [--force]
sudo kast undo --last
sudo kast undo --prune [--keep 5]           # delete the file copies of old or fully undone runs
sudo kast uninstall zotero steam [--dry-run]
```

The last 10 runs keep their file copies by default (`keep_runs` in
`/var/lib/kast/settings.json` changes it); the undo screen shows how much they take and `p`
prunes the rest, after asking. `kast import FILE` / `kast backup ...` run as your own user
aren't recorded (they say so); run them with sudo to record them.

**Restores ask first:** "Snapshot your current app data before importing?", with its size
and time, yes by default. Saying no needs a second keypress after a warning: such an import
can't be undone. `--no-safety-backup` prints the same warning.

### Snapshots before the upgrade

| `/` is | kast offers |
| --- | --- |
| btrfs, snapper with a `root` config | a `snapper -c root create --type pre` snapshot before the upgrade and a `post` one after, on by default. With snap-pac installed (it already snapshots every pacman run) it's off by default but still offered. Roll back with your boot menu (limine-snapper-sync, grub-btrfs) or `sudo snapper -c root rollback <pre>` and reboot, or file by file with `snapper undochange <pre>..<post>`: the summary names the numbers |
| btrfs, Timeshift in btrfs mode | `timeshift --create --comments "kast before upgrade" --tags O`; restore with `sudo timeshift --restore` |
| btrfs, nothing set up | the hint to install snapper (on CachyOS likely `cachyos-snapper-support`); kast installs nothing |
| anything else | the warning, and the advice to make a backup first |

If the snapshot fails, the app asks whether to upgrade without one. `kast auto` stops
before the upgrade then, unless it was started with `--no-snapshot`.

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

## Credits and license

kast is released under the GNU General Public License, version 3 or later (`LICENSE`).

The Plasma settings export/import module (`kast plasma`, `src/plasma/`) is a Rust port of the logic of
[konsave](https://github.com/Prayag2/konsave) by Prayag2 and contributors, and of its refactored fork
[urban-1/konsave](https://github.com/urban-1/konsave) (both GPL-3.0). Thank you both: the profile model, the
save/export split, the config placeholders and the `config-check` idea come from them. The exact versions studied and
the differences are listed in `docs/konsave-notes.md`; `THIRD_PARTY.md` has the notice.
