#!/bin/sh
# A stand-in for Proton's `proton-drive` CLI, for kast's tests. Tests copy it into a temp
# dir; its state lives next to it: `mode` (ok, signed_out, keyring, offline, upload_fail,
# download_fail, crash), `version`, `calls` (one line per call), and `drive/` (the remote
# tree: /my-files/x is drive/my-files/x).
D=$(cd "$(dirname "$0")" && pwd)
mode=$(cat "$D/mode" 2>/dev/null || echo ok)
echo "$*" >> "$D/calls"
[ -t 0 ] && { echo "stdin must not be a terminal" >&2; exit 9; }

fail() { echo "$1" >&2; exit 1; }
session() {
    case $mode in
        signed_out) fail "You need to login first" ;;
        keyring) fail "Failed to load session from secrets (ensure you have secrets available, read the README for more information): Cannot autolaunch D-Bus without X11 \$DISPLAY" ;;
        offline) fail "ConnectionError: fetch failed" ;;
        crash) kill -ILL $$ ;;
    esac
}
need() { for f in "$@"; do case " $ARGS " in *" $f "*) ;; *) fail "missing $f" ;; esac; done; }
node() { # path
    p=$D/drive$1
    name=$(basename "$1")
    if [ -d "$p" ]; then
        printf '{"uid":"u~%s","name":{"ok":true,"value":"%s"},"type":"folder","creationTime":"2026-10-01T10:00:00.000Z"}' "$name" "$name"
    else
        size=$(wc -c < "$p" | tr -d ' ')
        printf '{"uid":"u~%s","name":{"ok":true,"value":"%s"},"type":"file","activeRevision":{"claimedSize":%s,"claimedModificationTime":"2026-10-0%sT10:00:00.000Z"}}' "$name" "$name" "$size" "$(printf %s "$name" | cksum | cut -c1)"
    fi
}
ARGS="$*"
case "$1 $2" in
    "version "*)
        echo "Proton Drive CLI proton-drive-cli@$(cat "$D/version" 2>/dev/null || echo 0.9.0)"
        echo "Proton Drive SDK @protontech/drive-sdk@0.14.0"
        [ "$mode" = crash ] && kill -ILL $$
        exit 0 ;;
    "auth login")
        need --json
        echo "Opening https://account.proton.me/sso#key=SECRETKEY in the browser" >&2
        printf '{"signInUrl":"https://account.proton.me/sso#key=SECRETKEY"}\n'
        sleep "$(cat "$D/login_delay" 2>/dev/null || echo 1)"
        [ "$mode" = login_fail ] && fail "Authentication timed out"
        echo ok > "$D/mode"; exit 0 ;;
    "auth logout")
        echo signed_out > "$D/mode"; exit 0 ;;
    "filesystem info")
        session; need --json
        [ -e "$D/drive$3" ] || fail "Node not found: $(basename "$3")"
        node "$3"; echo; exit 0 ;;
    "filesystem list")
        session; need --json
        [ -d "$D/drive$3" ] || fail "Node not found: $(basename "$3")"
        printf '[\n'; first=1
        for p in "$D/drive$3"/*; do
            [ -e "$p" ] || continue
            [ $first = 1 ] || printf ',\n'; first=0
            node "$3/$(basename "$p")"
        done
        printf '\n]\n'; exit 0 ;;
    "filesystem create-folder")
        session
        [ -d "$D/drive$3" ] || fail "Node not found: $(basename "$3")"
        [ -e "$D/drive$3/$4" ] && fail "A file or folder with that name already exists"
        mkdir "$D/drive$3/$4"; echo "{}"; exit 0 ;;
    "filesystem upload")
        session; need --json --file-conflict-strategy --folder-conflict-strategy
        shift 2; files=""; parent=""
        while [ $# -gt 0 ]; do
            case $1 in --file-conflict-strategy|--folder-conflict-strategy) shift ;; --*) ;; *) files="$files $parent"; parent=$1 ;; esac
            shift
        done
        [ -d "$D/drive$parent" ] || fail "Node not found: $(basename "$parent")"
        n=0; bytes=0; skipped=0
        for f in $files; do
            base=$(basename "$f"); dest="$D/drive$parent/$base"
            if [ -e "$dest" ] && cmp -s "$f" "$dest"; then skipped=$((skipped+1)); continue; fi
            i=1; while [ -e "$dest" ]; do dest="$D/drive$parent/${base%.*} ($i).${base##*.}"; i=$((i+1)); done
            cp "$f" "$dest"; n=$((n+1)); bytes=$((bytes + $(wc -c < "$f")))
        done
        if [ "$mode" = upload_fail ]; then
            printf '{"transferredItems":0,"transferredBytes":0,"skippedItems":0,"failedItems":1,"failures":[{"name":"x","error":"ValidationError: Insufficient storage quota"}]}\n'
            fail "1 item(s) failed to upload"
        fi
        printf '{"transferredItems":%s,"transferredBytes":%s,"skippedItems":%s,"failedItems":0,"failures":[]}\n' $n $bytes $skipped; exit 0 ;;
    "filesystem download")
        session; need --json --file-conflict-strategy --folder-conflict-strategy
        shift 2; srcs=""; dir=""
        while [ $# -gt 0 ]; do
            case $1 in --file-conflict-strategy|--folder-conflict-strategy) shift ;; --*) ;; *) srcs="$srcs $dir"; dir=$1 ;; esac
            shift
        done
        n=0
        for r in $srcs; do
            [ -e "$D/drive$r" ] || fail "Node not found: $(basename "$r")"
            cp "$D/drive$r" "$dir/"; n=$((n+1))
        done
        if [ "$mode" = download_fail ]; then
            printf 'partial' > "$dir/$(basename "$r")"
            printf '{"transferredItems":0,"transferredBytes":0,"skippedItems":0,"failedItems":1,"failures":[{"name":"x","error":"IntegrityError: bad block"}]}\n'
            fail "1 item(s) failed to download"
        fi
        printf '{"transferredItems":%s,"transferredBytes":0,"skippedItems":0,"failedItems":0,"failures":[]}\n' $n; exit 0 ;;
    "filesystem trash")
        session; shift 2; mkdir -p "$D/trash"
        for r in "$@"; do case $r in --*) ;; *) [ -e "$D/drive$r" ] || fail "Node not found: $(basename "$r")"; mv "$D/drive$r" "$D/trash/" ;; esac; done
        echo "{}"; exit 0 ;;
esac
fail "fake proton-drive: unknown command: $*"
