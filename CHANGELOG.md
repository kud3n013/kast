# Changelog

## 0.10.4

- **New backup target: `ollama`.** It carries Ollama's model store (`blobs/`, `manifests/` and
  Ollama 0.40's `manifests-v2/`) from `OLLAMA_MODELS`, `~/.ollama/models` or the store of
  `ollama.service` (`/var/lib/ollama` on Arch). Not in `all`: models are gigabytes that can be
  downloaded again. The weights are stored in the zip without compression.
- `--no-media` leaves the weights out; the restore downloads those models again with
  `ollama pull`. A restore into the system store goes through sudo and gives the files to the
  `ollama` user; undo puts the manifests back. The server's key, its caches (`metadata/`,
  `.ollama/cache`) and unfinished downloads are never backed up.

## 0.10.3

- **`obsidian` now carries the vault too.** Besides `~/.config/obsidian`, the backup takes
  `~/Documents/Obsidian Vault` (notes, attachments and the vault's own `.obsidian` settings,
  plugins and themes), with none of the cache or `*.log` excludes the app folders get. A
  restore puts it back in `~/Documents/Obsidian Vault` of the account you restore to, whatever
  that account is called and wherever its `~/Documents` points, and points Obsidian's vault
  list (`obsidian.json`) at it.
- The vault keeps its id across the restore. Obsidian stores Sync's link to the remote vault and
  its sync state under that id in `~/.config/obsidian`, so the restored vault is still connected
  to Sync, and the account stays signed in. The sign-in token is encrypted with the desktop
  keyring's key: restore the keyring too (`kast backup keyrings`) when you move to a new system.
- `kast status` shows the vault and its size; `kast info` lists it.

## 0.10.2

- **New backup targets: `claude` and `copilot`.** `claude` carries `~/.claude` (conversations,
  project memory, settings, skills, installed plugins) and `~/.claude.json` (user MCP servers,
  trusted projects, preferences); `copilot` carries `~/.copilot` without its logs. Both are in
  `all` and the schedule. Claude Code's sign-in token (`.credentials.json`) and what it
  rebuilds itself (`cache`, `paste-cache`, `shell-snapshots`, `sessions`, ...) are left out:
  sign in again after a restore. A restore takes a safety copy of the `~/.claude.json` it
  replaces, and undo puts it back (Zalo's `zalo-flags.conf` now gets the same).
- Running Claude Code or Copilot sessions don't block a backup or restore (kast may well be
  run from one), but quit Claude Code before restoring: it rewrites `~/.claude.json` as it runs.
- Paths inside `~/.claude.json` and the per-project folder names are not rewritten when you
  restore under a different home.

## 0.10.1

- **Proton Drive uploads show real progress.** The gauge fills with the bytes sent out of the
  total (instead of sitting at 0%), and its label shows sent / total, the upload speed and an
  ETA. Proton's CLI reports no progress when kast runs it, so kast measures the bytes its TCP
  connections had acknowledged by Proton (sock_diag; no extra privileges). The speed is smoothed
  over the last few seconds, so it follows the network as it changes; the ETA blends it with the
  average since the first byte, the way download managers estimate it, so it doesn't swing with
  every burst or stall. Where it can't be measured the screen says so rather than guessing.
- The log records each upload's size, time and average speed.

## 0.10.0

- **New backup targets: `kwallet` and `gnome-keyring`** (`kast backup keyrings` for both).
  The Chromium and Electron apps kast restores keep the key to their saved passwords and
  cookies in the desktop keyring, so a restore next to an empty keyring signed you out
  anyway. `kwallet` carries `~/.local/share/kwalletd` (each wallet's `.kwl` with its `.salt`
  and `_attributes.json`) and `kwalletrc`; `gnome-keyring` carries `~/.local/share/keyrings`.
  The files are copied encrypted as they are: kast never asks for, reads or unlocks a keyring
  password, and the manifest lists names, types and sizes only.
- **Opt-in**: `all` (and so the schedule) includes them only with `--with-keyrings` or
  `[vault] keyrings = true` in `~/.config/kast/settings.toml`. The backup screen lists both and
  has an "Include keyrings" option for `all`, off by default.
- **Plain-text keyrings** (a GNOME keyring with an empty password) never go to Proton Drive or
  a scheduled run, and a local backup takes them only with `--allow-plaintext-keyring` (or
  "Back up anyway" in the backup screen).
- **Restoring** puts the keyrings in before every app, with ksecretd/kwalletd6 or
  gnome-keyring-daemon stopped (gnome-keyring is started again the way it ran: systemd units or
  its own command line), keeps the new system's other keyrings, snapshots the replaced ones for
  `kast undo`, and leaves kwalletrc with `Enabled=true` and `First Use=false`. Afterwards it says
  what to expect at the next login (PAM auto-unlock, the old password, GPG wallets) and warns
  when another program now answers for the Secret Service.
- `kast status` shows the keyrings, their type, whether PAM unlocks them and who owns
  `org.freedesktop.secrets`; `kast info` lists an archive's keyrings. The overwrite question of
  `kast import` lists what each app's restore replaces.

## 0.9.5

- **Export / Import data covers Zotero** (`kast backup zotero`, and part of `all` and of
  scheduled backups): the library in `~/Zotero` (`zotero.sqlite`, styles, translators,
  attachments) and the profile in `~/.zotero` (preferences, plugins), without logs, lock
  files and old `zotero.sqlite.*.bak` copies. `--no-media` leaves the attachments in
  `~/Zotero/storage` out. A running Zotero is closed for the backup like the other apps.

## 0.9.4

- **`kast import` stops `kglobalacceld` while it restores KDE shortcuts**, so restoring
  *All* brings back your shortcuts like *KDE Plasma settings only* does. The daemon kept
  KDE's defaults (Meta+L, the stock Meta+number bindings) in memory and wrote them over the
  restored `kglobalshortcutsrc` as soon as an app registered shortcuts with it, and *All*
  starts the apps it closed again right after the restore. It now runs again on the
  restored file before plasmashell and those apps start, and is started again on every
  error. Undoing a KDE restore does the same. The daemon check only counts your own
  session's daemon when kast runs under sudo (`kast plasma apply` too).

## 0.9.3

- **New `kast add-face` (`kast afm`)** adds a Howdy face model for you (`--user` for another
  account). `--ir` switches the IR emitter on first; without it, it's plain `howdy add`.
  Where kast tells you to add a face model, it now says `sudo kast afm --ir` (or `sudo kast
  afm` when the camera has no emitter kast can switch on).

## 0.9.2

- **Howdy works on lock screens.** A lock screen (KDE's included) checks your face as you,
  not as root. Howdy's setuid helper only steps in when you can't read
  `/etc/howdy/config.ini`, but some packages install it readable by everyone, so the lock
  screen tried to open the root-only face model itself ("Failed to open user model file")
  and fell back to the password. The Services step now gives `/etc/howdy` and `config.ini`
  Howdy's own modes (0750, 0640: the other users' bits go, nothing else changes); undo puts
  the old modes back. `kast status` warns while they're open.
- **KDE's lock screen (`kde`) is on the list of PAM files.** It is left alone when its auth
  stack already reaches Howdy through an include (Arch's includes `system-local-login`), so
  Howdy never scans twice.

## 0.9.1

- **The Services step raises Howdy's `dark_threshold` to 90** (`howdy set dark_threshold
  90`; a higher value you set is kept, undo puts the old one back).

## 0.9.0

- **Howdy uses the IR emitter of Windows Hello cameras.** Many of them keep the emitter off
  on Linux, so Howdy got dark frames. The Services step now checks the IR camera for
  Microsoft's face-authentication control (UVC 1.5) and, when it has one, adds `kast
  ir-emitter enable` directly above Howdy in every PAM file kast patches, so the emitter
  flashes for sudo, login, polkit and unlock. A hand-written `ir-msxu` line is replaced. The
  line is only added when the kast binary is root-owned (PAM runs it as root); the summary
  says what happened.
- **Howdy's `device_path` is the camera's `/dev/v4l/by-path` name**, which doesn't change
  when the nodes are renumbered.
- **New `kast ir-emitter` (`kast ir`)**: `probe` (read-only, `--json`), `enable`, `restore`,
  and `run -- howdy -U $USER add` to add a face model with the emitter on. Where kast tells
  you to add a face model, it gives that command when the camera has an emitter; `kast
  status` shows the emitter's state.

## 0.8.8

- **Export / Import data covers `~/.scripts`** (part of `all` and of scheduled backups;
  home paths in the scripts are adjusted when restoring on an account with another name).
- **Undoing an import puts back the `~/.local/share` folders** of FDM and fish too.

## 0.8.7

- **Export / Import data covers Helium, Free Download Manager, fish and fastfetch.**
  Helium's profile (`~/.config/net.imput.helium`, without its caches), FDM's settings and
  download list (`~/.config/Softdeluxe` and `~/.local/share/Softdeluxe/Free Download
  Manager`, without logs and cache), fish's config and history (`~/.config/fish`,
  `~/.local/share/fish`) and `~/.config/fastfetch`. They are part of `all` and of scheduled
  backups. A running fish shell is never closed for a backup.

## 0.8.6

- **`kast plasma save` stores Konsole's profiles, color schemes and key tabs**
  (`~/.local/share/konsole`) and its SSH manager settings (`konsolesshconfig`). Profiles were
  only stored with `--assets` or `export`, so `konsolerc` came back naming a default profile
  that wasn't there. Older exports still restore them.

## 0.8.5

- **`kast plasma apply --only shortcuts` restores every shortcut in System Settings >
  Shortcuts.** The `.desktop` files that "Add Application" and "Add Command or Script"
  shortcuts launch (`~/.local/share/applications`) moved from `apps` to `shortcuts`, so a
  command shortcut no longer comes back without its command. Profiles saved by 0.8.1-0.8.4
  restore them the same way.
- **Per-app shortcuts are saved** (`~/.local/share/kxmlgui5` and `kxmlgui6`, under
  `shortcuts`); `kxmlgui6` was only exported before.
- **Your KWin scripts and effects are saved** (`~/.local/share/kwin/scripts`, `effects`,
  `tabbox`, under `windows`), so the shortcuts they register keep working.

## 0.8.4

- **`kast repo` writes archrepo's block with `SigLevel = Optional TrustAll`.** A block an
  older version wrote with `SigLevel = Required` is changed to match; any other SigLevel you
  set is kept. The repository's signed package database is still checked by kast itself.

## 0.8.3

- **`kast plasma save/apply` includes the Web Search Keywords settings** (`kuriikwsfilterrc`:
  the delimiter, the default shortcut and the preferred shortcuts).

## 0.8.1

- **Backup and `kast plasma` now include more KDE service settings**: KRunner's and Dolphin's
  web search keywords (`~/.local/share/kf6/searchproviders`), custom service menus
  (`kservices5`, `kio/servicemenus`), your own desktop launchers (`applications`), and the
  time zone, Baloo file-info and Kate external-tools settings. `~/.config/kdeconnect` is left
  out on purpose: it holds the device's private key.

## 0.8.0

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
