# arch-setup

Arch Linux / CachyOS post-install tool, ported from `arch-setup.sh`. It runs the
setup steps behind a progress dashboard and includes the App Vault backup/restore
engine for app data, KDE Plasma, Hyprland and Howdy face models.

## Build & install

```sh
cargo build --release
sudo install -Dm755 target/release/arch-setup /usr/local/bin/arch-setup
```

Runtime tools: `pacman`, `rsync` (installed automatically when missing), plus
`yay`, `v4l2-ctl`, `howdy`, `blkid`, `findmnt` for the steps that use them.
Archives are read and written natively, so `zip`/`unzip`/`jq` are no longer needed.

## Usage

```sh
arch-setup                 # interactive menu (re-runs itself through sudo)
arch-setup auto            # full automatic setup
arch-setup partition | repo | packages | services
arch-setup status          # detected apps, Howdy, KDE, Hyprland
arch-setup backup all --no-media -o ~/all.zip
arch-setup info ~/all.zip
arch-setup restore ~/all.zip --app kde --dry-run
arch-setup --help          # every command and flag
```

Run it from your own account (`sudo arch-setup`, not from a root shell): the
target user is taken from `SUDO_USER`. Setup commands need root and re-run
themselves through `sudo`; vault commands work as your own user.

Backups made by the old `arch-setup.sh` can be restored, and the other way round.
