# kas — Kud3n's Arch Setup

Arch Linux / CachyOS post-install tool, ported from `arch-setup.sh`. It runs the
setup steps behind a progress dashboard and includes the App Vault backup/restore
engine for app data, KDE Plasma, Hyprland and Howdy face models.

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
kas                  # interactive menu (re-runs itself through sudo)
kas auto             # upgrade, repo, packages, services (no partition, no import)
kas partition        # optional: /media/data + home folder symlinks
kas repo | packages | services
kas status           # detected apps, Howdy, KDE, Hyprland
kas backup all --no-media -o ~/all.zip
kas info ~/all.zip
kas restore          # optional: pick a backup to restore (not run by auto)
kas restore ~/all.zip --app kde --dry-run
kas --help           # every command and flag
```

Run it from your own account (`sudo kas`, not from a root shell): the
target user is taken from `SUDO_USER`. Setup commands need root and re-run
themselves through `sudo`; vault commands work as your own user.

Backups made by the old `arch-setup.sh` can be restored, and the other way round.
