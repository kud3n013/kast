#!/usr/bin/env bash
# ==============================================================================
# arch-setup.sh - Arch Linux / CachyOS Fast Setup TUI
# Automated, idempotent system setup with dual progress bars and accurate ETA
#
# Options:
#   1. Run automatic script (Steps 2 through 6 in sequence)
#   2. Setup universal data partition (/media/data fstab + user dir symlinks)
#   3. Add kud3n's repository ([archrepo] in /etc/pacman.conf)
#   4. Install packages (Official/CachyOS, kud3n repo, app addons)
#   5. Setup services (ProtonVPN, Vietnamese IME, Howdy Face Auth, Valkey/SearXNG)
#   6. Import data (App Vault restore: apps, KDE Plasma, Howdy)
#   7. Export data (App Vault backup: apps, KDE Plasma, Howdy)
# ==============================================================================

set -eo pipefail
shopt -s extglob


# --- Help Display (Unprivileged) ---
if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    cat <<EOF
Usage: sudo $0 [COMMAND]

Commands:
  auto        Run full automatic setup (Steps 2 to 6 in sequence)
  partition   Setup universal data partition (/media/data & symlinks)
  repo        Add kud3n's repository ([archrepo] in /etc/pacman.conf)
  packages    Install packages (Official, CachyOS, kud3n, add-ons)
  services    Setup services (install ProtonVPN & Fcitx5 packages, Howdy, Valkey/SearXNG)
  import      Import application data, KDE Plasma settings & face model from backup archive (.zip/.dat)
  export      Export application data, KDE Plasma settings & face model to backup archive (.zip/.dat)
  status      Show detected applications & Howdy status, paths & sizes
  info        Inspect a backup archive (.zip) manifest metadata
  vault       Direct access to embedded app vault engine

Run without arguments to launch interactive TUI menu.
EOF
    exit 0
fi

# --- Selective Privilege Elevation ---
case "${1:-}" in
    -h|--help|status|info|inspect|backup|export|import|restore|vault|test|noop)
        # Vault subcommands and test/help can run as current user or under sudo
        ;;
    *)
        # System setup commands and interactive menu require root
        if [ "${EUID}" -ne 0 ]; then
            echo "Privileges required. Re-executing with sudo..."
            exec sudo bash "$0" "$@"
        fi
        ;;
esac

# --- User & Environment Resolution ---
if [ "${EUID}" -ne 0 ]; then
    # Unprivileged vault commands always act on the invoking user
    TARGET_USER="$(id -un)"
elif [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
    TARGET_USER="$SUDO_USER"
elif [ -n "${PKEXEC_UID:-}" ]; then
    TARGET_USER="$(id -un "$PKEXEC_UID" 2>/dev/null || true)"
fi

if [ -z "${TARGET_USER:-}" ] || [ "$TARGET_USER" = "root" ]; then
    # Auto-detect primary desktop human user (UID >= 1000, excluding nobody)
    TARGET_USER="$(awk -F: '$3 >= 1000 && $3 < 65534 {print $1; exit}' /etc/passwd 2>/dev/null || true)"
fi

if [ -z "${TARGET_USER:-}" ] || [ "$TARGET_USER" = "root" ]; then
    TARGET_USER="$(logname 2>/dev/null || echo "$USER")"
fi

TARGET_UID="$(id -u "$TARGET_USER" 2>/dev/null || echo 1000)"
TARGET_GID="$(id -g "$TARGET_USER" 2>/dev/null || echo 1000)"
TARGET_HOME="$(getent passwd "$TARGET_USER" 2>/dev/null | cut -d: -f6)"
if [ -z "$TARGET_HOME" ] || [ ! -d "$TARGET_HOME" ]; then
    TARGET_HOME="/home/$TARGET_USER"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ "${EUID}" -eq 0 ]; then
    LOG_FILE="/var/log/arch-setup.log"
else
    LOG_FILE="${TARGET_HOME}/.local/state/arch-setup.log"
fi
mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null || true
touch "$LOG_FILE" 2>/dev/null || true
chmod 644 "$LOG_FILE" 2>/dev/null || true

# --- Visual Formatting & Colors ---
ESC=$(printf '\033')
C_RESET="${ESC}[0m"
C_BOLD="${ESC}[1m"
C_DIM="${ESC}[2m"
C_ITALIC="${ESC}[3m"
C_UNDERLINE="${ESC}[4m"

C_RED="${ESC}[31m"
C_GREEN="${ESC}[32m"
C_YELLOW="${ESC}[33m"
C_BLUE="${ESC}[34m"
C_MAGENTA="${ESC}[35m"
C_CYAN="${ESC}[36m"
C_WHITE="${ESC}[37m"
C_GRAY="${ESC}[90m"

C_BG_BLUE="${ESC}[44m"
C_BG_DARK="${ESC}[48;5;236m"

# Terminal Box Characters (Rounded Modern Unicode)
BOX_H="─"
BOX_V="│"
BOX_TL="╭"
BOX_TR="╮"
BOX_BL="╰"
BOX_BR="╯"
BOX_VL="├"
BOX_VR="┤"
BOX_TT="┬"
BOX_BT="┴"

# Activity Spinner & Layout Dimensions
SPINNER_CHARS=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
SPINNER_IDX=0
UI_BOX_WIDTH=80
UI_INNER_WIDTH=76

# Log helpers
log() {
    local level="$1"; shift
    local msg="$*"
    local ts; ts="$(date '+%Y-%m-%d %H:%M:%S')"
    echo "[$ts] [$level] $msg" >> "$LOG_FILE"
}

run_as_user() {
    local user_runtime="/run/user/${TARGET_UID}"
    local w_disp="${WAYLAND_DISPLAY:-}"
    local x_disp="${DISPLAY:-}"
    local dbus_bus="${DBUS_SESSION_BUS_ADDRESS:-}"
    if [ -z "$w_disp" ] && [ -e "${user_runtime}/wayland-1" ]; then
        w_disp="wayland-1"
    elif [ -z "$w_disp" ] && [ -e "${user_runtime}/wayland-0" ]; then
        w_disp="wayland-0"
    fi
    if [ -z "$dbus_bus" ] && [ -e "${user_runtime}/bus" ]; then
        dbus_bus="unix:path=${user_runtime}/bus"
    fi

    sudo -u "$TARGET_USER" env \
        HOME="$TARGET_HOME" \
        USER="$TARGET_USER" \
        LOGNAME="$TARGET_USER" \
        XDG_CONFIG_HOME="$TARGET_HOME/.config" \
        XDG_DATA_HOME="$TARGET_HOME/.local/share" \
        XDG_STATE_HOME="$TARGET_HOME/.local/state" \
        XDG_CACHE_HOME="$TARGET_HOME/.cache" \
        XDG_RUNTIME_DIR="$user_runtime" \
        WAYLAND_DISPLAY="$w_disp" \
        DISPLAY="$x_disp" \
        DBUS_SESSION_BUS_ADDRESS="$dbus_bus" \
        "$@"
}

# mkdir -p that hands every directory it creates to the target user when running as root,
# so e.g. a missing ~/.local or ~/.cache never ends up root-owned.
mkdir_user() {
    local dir="$1"
    local created=()
    local p="$dir"
    while [ -n "$p" ] && [ "$p" != "/" ] && [ "$p" != "." ] && [ ! -e "$p" ]; do
        created+=("$p")
        p="$(dirname "$p")"
    done
    mkdir -p "$dir" || return 1
    if [ "${EUID}" -eq 0 ] && [ "${TARGET_UID:-0}" != "0" ] && [ ${#created[@]} -gt 0 ]; then
        chown "${TARGET_UID}:${TARGET_GID}" "${created[@]}" 2>/dev/null || true
    fi
}

# Problems hit during the automatic run, listed on the final summary screen
SETUP_ISSUES=()
add_issue() {
    SETUP_ISSUES+=("$*")
    log "WARN" "Issue recorded: $*"
}

# --- Cleanup & Terminal State ---
TUI_ACTIVE=false
cleanup_terminal() {
    # Only undo terminal changes if the TUI made them; otherwise rmcup can clobber CLI output
    [ "$TUI_ACTIVE" = true ] || return 0
    stty echo ixon icanon icrnl 2>/dev/null || true
    tput cnorm 2>/dev/null || true
    tput rmcup 2>/dev/null || true
    TUI_ACTIVE=false
}

# Staging dir of an in-progress vault backup/restore, removed on exit
VAULT_STAGING_DIR=""
cleanup_vault_staging() {
    if [ -n "${VAULT_STAGING_DIR:-}" ]; then
        rm -rf -- "$VAULT_STAGING_DIR" 2>/dev/null || true
        VAULT_STAGING_DIR=""
    fi
}

# yay builds as the target user and then runs `sudo pacman` itself. From the backgrounded,
# redirected task that sudo password prompt is invisible and hangs forever, and the sudo
# timestamp from launching this script expires during long installs. So for the duration
# of a yay call, let the (already sudo-capable) user run pacman without a password.
YAY_SUDOERS_FILE="/etc/sudoers.d/zz-arch-setup-yay"
grant_yay_sudo() {
    [ "${EUID}" -eq 0 ] || return 1
    # Never hand pacman to a user who couldn't already sudo
    if [ "$TARGET_USER" != "${SUDO_USER:-}" ] && ! id -nG "$TARGET_USER" 2>/dev/null | grep -qw wheel; then
        log "WARN" "$TARGET_USER is not a sudo user; skipping yay fallback."
        return 1
    fi
    local pacman_bin tmp
    pacman_bin="$(command -v pacman)" || return 1
    tmp="$(mktemp)" || return 1
    printf '%s ALL=(root) NOPASSWD: %s\n' "$TARGET_USER" "$pacman_bin" > "$tmp"
    if visudo -cf "$tmp" >/dev/null 2>&1 && install -m 0440 -o root -g root "$tmp" "$YAY_SUDOERS_FILE"; then
        rm -f "$tmp"
        return 0
    fi
    rm -f "$tmp"
    log "WARN" "Could not install temporary sudoers rule; skipping yay fallback."
    return 1
}
revoke_yay_sudo() {
    if [ -e "$YAY_SUDOERS_FILE" ]; then
        rm -f "$YAY_SUDOERS_FILE" 2>/dev/null || true
    fi
}

cleanup_temp_state() {
    cleanup_vault_staging
    revoke_yay_sudo
}

trap 'cleanup_temp_state; cleanup_terminal' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

init_terminal() {
    TUI_ACTIVE=true
    tput smcup 2>/dev/null || true
    tput civis 2>/dev/null || true
    stty -ixon -echo 2>/dev/null || true
    clear 2>/dev/null || true
}

restore_terminal_interactive() {
    if [ "$TUI_ACTIVE" = true ]; then
        tput rmcup 2>/dev/null || true
        TUI_ACTIVE=false
    fi
    tput cnorm 2>/dev/null || true
    stty echo ixon icanon icrnl 2>/dev/null || true
}

read_choice() {
    local prompt="$1"
    local var_name="${2:-}"
    local raw_val=""
    restore_terminal_interactive
    read -r -p "$prompt" raw_val || true
    local clean="$raw_val"
    clean="${clean//$'\r'/}"
    clean="${clean//$'\n'/}"
    clean="${clean#"${clean%%[![:space:]]*}"}"
    clean="${clean%"${clean##*[![:space:]]}"}"
    if [ -n "$var_name" ]; then
        printf -v "$var_name" "%s" "$clean"
    fi
}

get_term_dimensions() {
    local cols
    cols=$(tput cols 2>/dev/null || echo 80)
    if ! [[ "$cols" =~ ^[0-9]+$ ]] || (( cols <= 0 )); then
        cols=80
    fi

    local max_w=96
    local usable=$(( cols - 2 ))
    if (( usable < 36 )); then
        usable=36
    elif (( usable > max_w )); then
        usable=$max_w
    fi
    UI_BOX_WIDTH=$usable
    UI_INNER_WIDTH=$(( UI_BOX_WIDTH - 4 ))
    if (( UI_INNER_WIDTH < 20 )); then UI_INNER_WIDTH=20; fi
}

on_winch() {
    get_term_dimensions
}
trap on_winch WINCH

# --- String & Layout Math Helpers ---
strip_ansi() {
    local str="$1"
    echo "${str//$ESC\[*([0-9;])[a-zA-Z]/}"
}

sanitize_log() {
    local str="$1"
    str="${str//$'\r'/}"
    str="${str//$'\n'/}"
    str="${str//$'\t'/  }"
    strip_ansi "$str"
}

box_line() {
    local text="$1"
    local inner_width="${2:-$UI_INNER_WIDTH}"
    local raw; raw="$(strip_ansi "$text")"
    local raw_len=${#raw}

    if (( raw_len > inner_width )); then
        if [[ "$text" != *$ESC* ]]; then
            text="${text:0:$((inner_width - 3))}..."
            raw_len=$inner_width
        else
            text="${raw:0:$((inner_width - 3))}..."
            raw_len=$inner_width
        fi
    fi

    local pad_len=$(( inner_width - raw_len ))
    local pad=""
    if (( pad_len > 0 )); then
        printf -v pad "%*s" "$pad_len" ""
    fi
    printf "${C_CYAN}%s${C_RESET} %s%s ${C_CYAN}%s${C_RESET}\033[K\n" "$BOX_V" "$text" "$pad" "$BOX_V"
}

# --- Time and Progress Math ---
format_seconds() {
    local s=$1
    if (( s < 0 )); then s=0; fi
    local m=$(( s / 60 ))
    local sec=$(( s % 60 ))
    if (( m >= 60 )); then
        local h=$(( m / 60 ))
        m=$(( m % 60 ))
        printf "%02d:%02d:%02d" "$h" "$m" "$sec"
    else
        printf "%02d:%02d" "$m" "$sec"
    fi
}

calc_eta_countdown() {
    local start_time=$1
    local current=$2
    local total=$3
    local baseline_per_item=${4:-15}

    local now; now=$(date +%s)
    local elapsed=$(( now - start_time ))
    if (( elapsed < 0 )); then elapsed=0; fi

    if (( total <= 0 || current >= total )); then
        echo "00:00"
        return
    fi

    local remaining_items=$(( total - current ))
    local est_remaining=0

    if (( current <= 0 )); then
        est_remaining=$(( remaining_items * baseline_per_item ))
        est_remaining=$(( est_remaining - elapsed ))
        if (( est_remaining < 5 )); then est_remaining=5; fi
    else
        local avg_sec=$(( elapsed / current ))
        if (( avg_sec < 1 )); then avg_sec=1; fi
        est_remaining=$(( remaining_items * avg_sec ))
    fi

    format_seconds "$est_remaining"
}

calculate_eta() {
    calc_eta_countdown "$@"
}

calc_overall_progress() {
    local o_step=$1     # 1..6
    local s_step=$2     # current sub step
    local s_total=$3    # total sub steps

    local weights=(25 5 5 50 10 5)
    local cum_weight=0
    for (( i=0; i<o_step-1; i++ )); do
        cum_weight=$(( cum_weight + weights[i] ))
    done

    local cur_weight=${weights[$(( o_step - 1 ))]:-10}
    local sub_frac=0
    if (( s_total > 0 )); then
        sub_frac=$(( (s_step * cur_weight) / s_total ))
    fi

    local total_pct=$(( cum_weight + sub_frac ))
    if (( total_pct > 100 )); then total_pct=100; fi
    echo "$total_pct"
}

# --- Progress Bar Rendering ---
draw_bar() {
    local percent=$1
    local width=$2
    if (( percent > 100 )); then percent=100; fi
    if (( percent < 0 )); then percent=0; fi
    if (( width < 1 )); then width=1; fi
    local filled=$(( (percent * width) / 100 ))
    local empty=$(( width - filled ))
    local bar=""
    for (( i=0; i<filled; i++ )); do bar="${bar}▰"; done
    for (( i=0; i<empty; i++ )); do bar="${bar}▱"; done
    printf "%s" "$bar"
}

render_dual_dashboard() {
    local o_step=$1
    local o_total=$2
    local o_title=$3
    local o_start=$4
    local s_step=$5
    local s_total=$6
    local s_title=$7
    local s_item=$8
    local s_start=$9
    local recent_log="${10:-Ready}"
    local spinner="${11:-${SPINNER_CHARS[$SPINNER_IDX]}}"

    get_term_dimensions
    local box_width=$UI_BOX_WIDTH
    local inner_width=$UI_INNER_WIDTH

    local h_line=""
    printf -v h_line "%*s" "$(( box_width - 2 ))" ""
    h_line="${h_line// /$BOX_H}"

    local o_pct; o_pct="$(calc_overall_progress "$o_step" "$s_step" "$s_total")"
    local now; now=$(date +%s)
    local o_elapsed=$(( now - o_start ))
    if (( o_elapsed < 0 )); then o_elapsed=0; fi
    local o_elapsed_str; o_elapsed_str="$(format_seconds "$o_elapsed")"

    local o_eta_sec=0
    if (( o_pct >= 100 )); then
        o_eta_sec=0
    elif (( o_pct >= 2 )); then
        local total_est=$(( (o_elapsed * 100) / o_pct ))
        o_eta_sec=$(( total_est - o_elapsed ))
        if (( o_eta_sec < 5 )); then o_eta_sec=5; fi
    else
        o_eta_sec=$(( 420 - o_elapsed ))
        if (( o_eta_sec < 10 )); then o_eta_sec=10; fi
    fi
    local o_eta_str; o_eta_str="$(format_seconds "$o_eta_sec")"

    local s_pct=0
    if (( s_total > 0 )); then s_pct=$(( (s_step * 100) / s_total )); fi
    local s_elapsed=$(( now - s_start ))
    if (( s_elapsed < 0 )); then s_elapsed=0; fi
    local s_elapsed_str; s_elapsed_str="$(format_seconds "$s_elapsed")"
    local s_eta_str; s_eta_str="$(calc_eta_countdown "$s_start" "$s_step" "$s_total" 15)"

    local buf=""
    buf+="${C_CYAN}${BOX_TL}${h_line}${BOX_TR}${C_RESET}\033[K\n"
    if (( inner_width < 56 )); then
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ ARCH LINUX SETUP ◆${C_RESET}" "$inner_width")\n"
    else
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ ARCH LINUX / CACHYOS SETUP CONTROLLER ◆${C_RESET}" "$inner_width")\n"
    fi
    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"

    # Universal Progress Section
    local o_header
    if (( inner_width < 56 )); then
        o_header="${C_BLUE}${C_BOLD}[ALL]${C_RESET} Step ${o_step}/${o_total}: ${o_title}"
    else
        o_header="${C_BLUE}${C_BOLD}[UNIVERSAL PROGRESS]${C_RESET} Step ${o_step}/${o_total}: ${o_title}"
    fi
    buf+="$(box_line "$o_header" "$inner_width")\n"

    local o_info
    if (( inner_width < 45 )); then
        o_info=$(printf " %3d%% │ ETA:%s" "$o_pct" "$o_eta_str")
    elif (( inner_width < 58 )); then
        o_info=$(printf " %3d%% │ %s │ ETA:%s" "$o_pct" "$o_elapsed_str" "$o_eta_str")
    else
        o_info=$(printf " %3d%% │ Elapsed: %s │ ETA: %s" "$o_pct" "$o_elapsed_str" "$o_eta_str")
    fi
    local o_bar_w=$(( inner_width - 2 - ${#o_info} ))
    if (( o_bar_w < 4 )); then o_bar_w=4; fi
    local o_bar="$(draw_bar "$o_pct" "$o_bar_w")"
    buf+="$(box_line "${C_GREEN}[${o_bar}]${C_RESET}${o_info}" "$inner_width")\n"

    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"

    # State Progress Section
    local s_header
    if (( inner_width < 56 )); then
        s_header="${C_MAGENTA}${C_BOLD}[STATE]${C_RESET} ${s_step}/${s_total}: ${s_title}"
    else
        s_header="${C_MAGENTA}${C_BOLD}[STATE PROGRESS]${C_RESET} ${s_title} (${s_step}/${s_total})"
    fi
    buf+="$(box_line "$s_header" "$inner_width")\n"

    local s_info
    if (( inner_width < 45 )); then
        s_info=$(printf " %3d%% │ ETA:%s" "$s_pct" "$s_eta_str")
    elif (( inner_width < 58 )); then
        s_info=$(printf " %3d%% │ %s │ ETA:%s" "$s_pct" "$s_elapsed_str" "$s_eta_str")
    else
        s_info=$(printf " %3d%% │ Elapsed: %s │ ETA: %s" "$s_pct" "$s_elapsed_str" "$s_eta_str")
    fi
    local s_bar_w=$(( inner_width - 2 - ${#s_info} ))
    if (( s_bar_w < 4 )); then s_bar_w=4; fi
    local s_bar="$(draw_bar "$s_pct" "$s_bar_w")"
    buf+="$(box_line "${C_YELLOW}[${s_bar}]${C_RESET}${s_info}" "$inner_width")\n"

    buf+="$(box_line "  ${C_BOLD}Active:${C_RESET} ${s_item}" "$inner_width")\n"

    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"

    # Status / Live Log Line
    local clean_log; clean_log="$(sanitize_log "$recent_log")"
    buf+="$(box_line "${C_CYAN}${spinner}${C_RESET} ${C_GRAY}Log: ${clean_log}${C_RESET}" "$inner_width")\n"
    buf+="${C_CYAN}${BOX_BL}${h_line}${BOX_BR}${C_RESET}\033[K\n"

    tput cup 0 0 2>/dev/null || clear 2>/dev/null || true
    printf "%b" "$buf"
}

render_single_dashboard() {
    local s_step=$1
    local s_total=$2
    local s_title=$3
    local s_item=$4
    local s_start=$5
    local recent_log="${6:-Working...}"
    local spinner="${7:-${SPINNER_CHARS[$SPINNER_IDX]}}"

    get_term_dimensions
    local box_width=$UI_BOX_WIDTH
    local inner_width=$UI_INNER_WIDTH

    local h_line=""
    printf -v h_line "%*s" "$(( box_width - 2 ))" ""
    h_line="${h_line// /$BOX_H}"

    local now; now=$(date +%s)
    local s_pct=0
    if (( s_total > 0 )); then s_pct=$(( (s_step * 100) / s_total )); fi
    local s_elapsed=$(( now - s_start ))
    if (( s_elapsed < 0 )); then s_elapsed=0; fi
    local s_elapsed_str; s_elapsed_str="$(format_seconds "$s_elapsed")"
    local s_eta_str; s_eta_str="$(calc_eta_countdown "$s_start" "$s_step" "$s_total" 15)"

    local buf=""
    buf+="${C_CYAN}${BOX_TL}${h_line}${BOX_TR}${C_RESET}\033[K\n"
    if (( inner_width < 56 )); then
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ TASK: ${s_title^^} ◆${C_RESET}" "$inner_width")\n"
    else
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ TASK EXECUTION: ${s_title^^} ◆${C_RESET}" "$inner_width")\n"
    fi
    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"

    local s_header
    if (( inner_width < 56 )); then
        s_header="${C_MAGENTA}${C_BOLD}[PROGRESS]${C_RESET} ${s_step}/${s_total}: ${s_item}"
    else
        s_header="${C_MAGENTA}${C_BOLD}[PROGRESS]${C_RESET} ${s_step}/${s_total} - ${s_item}"
    fi
    buf+="$(box_line "$s_header" "$inner_width")\n"

    local s_info
    if (( inner_width < 45 )); then
        s_info=$(printf " %3d%% │ ETA:%s" "$s_pct" "$s_eta_str")
    elif (( inner_width < 58 )); then
        s_info=$(printf " %3d%% │ %s │ ETA:%s" "$s_pct" "$s_elapsed_str" "$s_eta_str")
    else
        s_info=$(printf " %3d%% │ Elapsed: %s │ ETA: %s" "$s_pct" "$s_elapsed_str" "$s_eta_str")
    fi
    local s_bar_w=$(( inner_width - 2 - ${#s_info} ))
    if (( s_bar_w < 4 )); then s_bar_w=4; fi
    local s_bar="$(draw_bar "$s_pct" "$s_bar_w")"
    buf+="$(box_line "${C_GREEN}[${s_bar}]${C_RESET}${s_info}" "$inner_width")\n"

    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"

    local clean_log; clean_log="$(sanitize_log "$recent_log")"
    buf+="$(box_line "${C_CYAN}${spinner}${C_RESET} ${C_GRAY}Log: ${clean_log}${C_RESET}" "$inner_width")\n"
    buf+="${C_CYAN}${BOX_BL}${h_line}${BOX_BR}${C_RESET}\033[K\n"

    tput cup 0 0 2>/dev/null || clear 2>/dev/null || true
    printf "%b" "$buf"
}

draw_dual_dashboard() {
    render_dual_dashboard "$@"
}

draw_single_dashboard() {
    render_single_dashboard "$@"
}

# --- Asynchronous Monitored Command Execution ---
exec_monitored_cmd() {
    local is_auto=$1
    local o_step=$2
    local o_total=$3
    local o_title=$4
    local o_start=$5
    local s_step=$6
    local s_total=$7
    local s_title=$8
    local s_item=$9
    local s_start=${10}
    local cmd_to_run="${11}"

    log "INFO" "Executing monitored task: $s_item ($cmd_to_run)"
    read -t 0.001 -n 10000 _ 2>/dev/null || true

    eval "$cmd_to_run" >> "$LOG_FILE" 2>&1 &
    local cmd_pid=$!

    local last_log="Starting task..."
    while kill -0 "$cmd_pid" 2>/dev/null; do
        SPINNER_IDX=$(( (SPINNER_IDX + 1) % ${#SPINNER_CHARS[@]} ))
        local cur_spinner="${SPINNER_CHARS[$SPINNER_IDX]}"

        local raw_log
        raw_log=$(tail -n 15 "$LOG_FILE" 2>/dev/null | grep -v '^[[:space:]]*$' | tail -n 1 || true)
        if [ -n "$raw_log" ]; then
            raw_log="${raw_log#*\[INFO\] }"
            raw_log="${raw_log#*\[WARN\] }"
            raw_log="${raw_log#*\[ERROR\] }"
            last_log="$raw_log"
        fi

        if [ "$is_auto" = true ]; then
            render_dual_dashboard "$o_step" "$o_total" "$o_title" "$o_start" \
                                  "$s_step" "$s_total" "$s_title" "$s_item" "$s_start" \
                                  "$last_log" "$cur_spinner"
        else
            render_single_dashboard "$s_step" "$s_total" "$s_title" "$s_item" "$s_start" \
                                    "$last_log" "$cur_spinner"
        fi

        sleep 0.2
    done

    local cmd_rc=0
    wait "$cmd_pid" || cmd_rc=$?
    read -t 0.001 -n 10000 _ 2>/dev/null || true
    return "$cmd_rc"
}

# ==============================================================================
# SUB-ROUTINES FOR EACH SETUP STATE
# ==============================================================================

# ------------------------------------------------------------------------------
# State 0: Systemwide Upgrade (Used at start of Automatic Script)
# ------------------------------------------------------------------------------
do_system_upgrade() {
    local is_auto=${1:-false}
    local o_start=${2:-0}
    local o_step=${3:-0}
    local o_total=${4:-6}

    local s_start; s_start=$(date +%s)
    local title="Systemwide Upgrade (pacman -Syuu)"

    log "INFO" "Starting systemwide upgrade..."
    local cmd_rc=0
    exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                       1 2 "$title" "Refreshing databases & upgrading" "$s_start" \
                       "pacman -Syuu --noconfirm" || cmd_rc=$?

    if (( cmd_rc != 0 )); then
        log "WARN" "pacman -Syuu returned non-zero code ($cmd_rc). Check $LOG_FILE for details."
        add_issue "System upgrade (pacman -Syuu) failed with exit code $cmd_rc"
        local finish_msg="Warning: pacman -Syuu finished with warnings/errors"
    else
        local finish_msg="System packages up to date."
    fi

    if [ "$is_auto" = true ]; then
        render_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" 2 2 "$title" "Upgrade Complete" "$s_start" "$finish_msg" "✔"
    else
        render_single_dashboard 2 2 "$title" "Upgrade Complete" "$s_start" "$finish_msg" "✔"
    fi
    sleep 1
    log "INFO" "Systemwide upgrade completed."
}

# ------------------------------------------------------------------------------
# State 2: Setup universal data partition
# ------------------------------------------------------------------------------
do_setup_data_partition() {
    local is_auto=${1:-false}
    local o_start=${2:-0}
    local o_step=${3:-1}
    local o_total=${4:-6}

    local s_start; s_start=$(date +%s)
    local s_total=6
    local title="Setup Universal Data Partition"

    update_p() {
        local s_step=$1
        local s_item=$2
        local log_msg=$3
        log "INFO" "Partition Step $s_step: $s_item ($log_msg)"
        if [ "$is_auto" = true ]; then
            draw_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" "$s_step" "$s_total" "$title" "$s_item" "$s_start" "$log_msg"
        else
            draw_single_dashboard "$s_step" "$s_total" "$title" "$s_item" "$s_start" "$log_msg"
        fi
    }

    update_p 0 "Initializing" "Verifying configuration..."
    local UUID="6C62ADEA62ADB96A"
    local detected_part
    detected_part="$(blkid -L "Data" 2>/dev/null || true)"
    if [ -n "$detected_part" ]; then
        local detected_uuid
        detected_uuid="$(blkid -s UUID -o value "$detected_part" 2>/dev/null || true)"
        [ -n "$detected_uuid" ] && UUID="$detected_uuid"
    fi
    local MOUNT_POINT="/media/data"
    local FSTAB_LINE="UUID=${UUID}                     ${MOUNT_POINT}    ntfs3   defaults,nofail,uid=${TARGET_UID},gid=${TARGET_GID},rw,windows_names,iocharset=utf8,umask=000,exec 0 0"

    # Sub-step 1: Ensure mount point exists
    update_p 1 "Mount Directory" "Ensuring $MOUNT_POINT exists..."
    mkdir -p "$MOUNT_POINT"
    chown "${TARGET_UID}:${TARGET_GID}" "$MOUNT_POINT" 2>/dev/null || true

    # Sub-step 2: Check & update /etc/fstab
    update_p 2 "Fstab Entry" "Checking /etc/fstab configuration..."
    local BACKUP_PATH="/etc/fstab.bak.$(date +%Y%m%d_%H%M%S)"
    if grep -qsE "[[:space:]]${MOUNT_POINT}[[:space:]]" /etc/fstab; then
        local current_fstab_line
        current_fstab_line="$(grep -E "[[:space:]]${MOUNT_POINT}[[:space:]]" /etc/fstab | tail -n 1)"
        if [[ "$current_fstab_line" == *"$UUID"* && "$current_fstab_line" == *"umask=000"* && "$current_fstab_line" == *"uid=${TARGET_UID}"* ]]; then
            log "INFO" "Fstab entry already up to date for $MOUNT_POINT."
        else
            cp /etc/fstab "$BACKUP_PATH"
            sed -i "\|[[:space:]]${MOUNT_POINT}[[:space:]]|c\\${FSTAB_LINE}" /etc/fstab
            log "INFO" "Updated existing /etc/fstab entry for $MOUNT_POINT (backup at $BACKUP_PATH)."
        fi
    else
        cp /etc/fstab "$BACKUP_PATH"
        echo "${FSTAB_LINE}" >> /etc/fstab
        log "INFO" "Added entry to /etc/fstab (backup at $BACKUP_PATH)."
    fi

    # Sub-step 3: Test mount
    update_p 3 "Mount Filesystem" "Verifying mount..."
    systemctl daemon-reload >> "$LOG_FILE" 2>&1 || true
    if ! findmnt "$MOUNT_POINT" >/dev/null 2>&1; then
        mount -a >> "$LOG_FILE" 2>&1 || mount "$MOUNT_POINT" >> "$LOG_FILE" 2>&1 || true
    else
        mount -o remount "$MOUNT_POINT" >> "$LOG_FILE" 2>&1 || true
    fi

    if ! findmnt "$MOUNT_POINT" >/dev/null 2>&1; then
        # Migrating now would move the user's files onto the root filesystem underneath
        # the mount point, where they get hidden as soon as the partition is mounted.
        log "ERROR" "Mount point $MOUNT_POINT is not mounted. Skipping user directory migration."
        add_issue "Data partition not mounted at $MOUNT_POINT; user folders were not migrated"
        update_p 6 "Aborted" "$MOUNT_POINT not mounted; user directories left untouched."
        sleep 2
        return 0
    fi

    # ntfs3 mounts read-only (or refuses rw) when Windows was hibernated / Fast Startup is on
    local probe="${MOUNT_POINT}/.arch-setup-write-test.$$"
    if ! touch "$probe" 2>/dev/null; then
        log "ERROR" "$MOUNT_POINT is mounted but not writable (Windows Fast Startup/hibernation?). Skipping migration."
        add_issue "$MOUNT_POINT is read-only (disable Windows Fast Startup, then re-run step 2)"
        update_p 6 "Aborted" "$MOUNT_POINT is read-only; user directories left untouched."
        sleep 2
        return 0
    fi
    rm -f "$probe"

    # Sub-step 4: Migrate user directories & symlink
    update_p 4 "Migrate User Directories" "Linking Desktop, Documents, Downloads, etc."
    local dirs=("Desktop" "Documents" "Downloads" "Music" "Pictures" "Public" "Templates" "Videos" "Projects")
    for d in "${dirs[@]}"; do
        local target_dir="${MOUNT_POINT}/${d}"
        local home_dir="${TARGET_HOME}/${d}"
        mkdir -p "$target_dir"
        chown -R "${TARGET_UID}:${TARGET_GID}" "$target_dir" 2>/dev/null || true

        if [ -e "$home_dir" ] && [ ! -L "$home_dir" ]; then
            # NTFS can't hold Unix owners/modes, so only preserve timestamps; never delete
            # the original unless the copy fully succeeded.
            if cp -RT --preserve=timestamps "$home_dir" "$target_dir" >> "$LOG_FILE" 2>&1; then
                rm -rf "$home_dir"
            else
                log "ERROR" "Copying $home_dir to $target_dir failed; leaving $home_dir in place."
                add_issue "Could not migrate ~/$d to $target_dir (original kept)"
                continue
            fi
        fi
        ln -snf "$target_dir" "$home_dir"
        chown -h "${TARGET_UID}:${TARGET_GID}" "$home_dir" 2>/dev/null || true
    done

    # Sub-step 5: Update XDG User Dirs as target user
    update_p 5 "XDG User Dirs" "Configuring user-dirs.dirs for $TARGET_USER..."
    if command -v xdg-user-dirs-update >/dev/null 2>&1; then
        run_as_user xdg-user-dirs-update \
            --set DESKTOP "${MOUNT_POINT}/Desktop" \
            --set DOCUMENTS "${MOUNT_POINT}/Documents" \
            --set DOWNLOAD "${MOUNT_POINT}/Downloads" \
            --set MUSIC "${MOUNT_POINT}/Music" \
            --set PICTURES "${MOUNT_POINT}/Pictures" \
            --set VIDEOS "${MOUNT_POINT}/Videos" \
            --set PUBLICSHARE "${MOUNT_POINT}/Public" \
            --set TEMPLATES "${MOUNT_POINT}/Templates" >> "$LOG_FILE" 2>&1 || true
    fi

    local user_dirs_cfg="${TARGET_HOME}/.config/user-dirs.dirs"
    mkdir_user "$(dirname "$user_dirs_cfg")"
    cat > "$user_dirs_cfg" <<EOF
XDG_DESKTOP_DIR="${MOUNT_POINT}/Desktop"
XDG_DOCUMENTS_DIR="${MOUNT_POINT}/Documents"
XDG_DOWNLOAD_DIR="${MOUNT_POINT}/Downloads"
XDG_MUSIC_DIR="${MOUNT_POINT}/Music"
XDG_PICTURES_DIR="${MOUNT_POINT}/Pictures"
XDG_VIDEOS_DIR="${MOUNT_POINT}/Videos"
XDG_PUBLICSHARE_DIR="${MOUNT_POINT}/Public"
XDG_TEMPLATES_DIR="${MOUNT_POINT}/Templates"
EOF
    chown "${TARGET_UID}:${TARGET_GID}" "$user_dirs_cfg" 2>/dev/null || true

    update_p 6 "Completed" "Data partition & directories configured successfully."
    sleep 1
}

# ------------------------------------------------------------------------------
# State 3: Add kud3n's repository
# ------------------------------------------------------------------------------
do_add_kud3n_repo() {
    local is_auto=${1:-false}
    local o_start=${2:-0}
    local o_step=${3:-2}
    local o_total=${4:-6}

    local s_start; s_start=$(date +%s)
    local s_total=3
    local title="Add kud3n's Repository"

    update_p() {
        local s_step=$1
        local s_item=$2
        local log_msg=$3
        log "INFO" "Repo Step $s_step: $s_item ($log_msg)"
        if [ "$is_auto" = true ]; then
            draw_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" "$s_step" "$s_total" "$title" "$s_item" "$s_start" "$log_msg"
        else
            draw_single_dashboard "$s_step" "$s_total" "$title" "$s_item" "$s_start" "$log_msg"
        fi
    }

    update_p 0 "Inspecting /etc/pacman.conf" "Reading repository definitions..."

    # Sub-step 1: Check & configure /etc/pacman.conf
    update_p 1 "Updating pacman.conf" "Configuring [archrepo]..."
    local repo_url="https://github.com/kud3n013/archrepo/releases/download/packages"
    if grep -q "\[archrepo\]" /etc/pacman.conf; then
        sed -i 's|Server = https://kud3n013.github.io/archrepo/x86_64|Server = https://github.com/kud3n013/archrepo/releases/download/packages|' /etc/pacman.conf
        log "INFO" "Updated existing [archrepo] in /etc/pacman.conf"
    else
        cat >> /etc/pacman.conf <<EOF

[archrepo]
SigLevel = Optional TrustAll
Server = ${repo_url}
EOF
        log "INFO" "Appended new [archrepo] block to /etc/pacman.conf"
    fi

    # Sub-step 2: Sync databases
    # Sync + upgrade together: a bare -Syy followed by later `pacman -S <pkg>` is a partial upgrade
    update_p 2 "Syncing Pacman" "Running pacman -Syyu..."
    exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                       2 "$s_total" "$title" "Syncing databases & upgrading (pacman -Syyu)" "$s_start" \
                       "pacman -Syyu --noconfirm" || {
        log "WARN" "pacman -Syyu finished with warnings. See $LOG_FILE"
        add_issue "Repository sync (pacman -Syyu) failed"
    }

    # Sub-step 3: Verify repo availability
    update_p 3 "Verifying" "Checking archrepo package list..."
    if pacman -Sl archrepo >/dev/null 2>&1; then
        log "INFO" "Repository archrepo verified successfully."
    else
        log "WARN" "Could not verify archrepo packages. Check internet connection."
        add_issue "[archrepo] repository could not be verified"
    fi

    sleep 1
}

# ------------------------------------------------------------------------------
# State 4: Installing packages (with per-package progress & ETA)
# ------------------------------------------------------------------------------
do_install_packages() {
    local is_auto=${1:-false}
    local o_start=${2:-0}
    local o_step=${3:-3}
    local o_total=${4:-6}

    local s_start; s_start=$(date +%s)
    local title="Install Packages"

    local pkgs_official=(
        "zen-browser-bin"
        "zotero"
        "proton-vpn-cli"
        "inkscape"
        "chromium"
        "obsidian"
        "fcitx5"
        "fcitx5-configtool"
        "fcitx5-gtk"
        "fcitx5-qt"
        "fcitx5-unikey"
        "vesktop-bin"
        "valkey"
        "kdenlive"
        "steam"
        "zed-globalmenu"
        "wine"
        "wine-gecko"
        "wine-mono"
        "gwenview"
        "haruna"
        "libreoffice-fresh"
        "libreoffice-fresh-vi"
    )

    local pkgs_kud3n=(
        "zalo-for-linux"
        "beeper"
        "howdy-next"
        "antigravity"
        "gdlauncher-carbon"
        "searxng"
        "archrepo/proton-mail"
        "archrepo/proton-pass"
        "archrepo/freedownloadmanager"
        "archrepo/sine"
        "archrepo/hyprmod"
    )

    local pkgs_addons=(
        "freedownloadmanager-elephant"
        "zotero-better-bibtex"
        "zotmoov"
        "zotero-better-notes"
        "zotero-ocr"
        "zotero-pdf-translate"
        "zotero-mcp"
    )

    local all_pkgs=("${pkgs_official[@]}" "${pkgs_kud3n[@]}" "${pkgs_addons[@]}")
    local s_total=${#all_pkgs[@]}
    local current_idx=0

    log "INFO" "Starting installation of ${s_total} packages..."

    for pkg in "${all_pkgs[@]}"; do
        ((current_idx++)) || true

        # Ensure archrepo versions are used instead of legacy/AUR packages
        if [ "$pkg" = "proton-mail-bin" ] || [ "$pkg" = "proton-mail" ]; then
            pkg="archrepo/proton-mail"
        elif [ "$pkg" = "proton-pass" ]; then
            pkg="archrepo/proton-pass"
        elif [ "$pkg" = "freedownloadmanager" ]; then
            pkg="archrepo/freedownloadmanager"
        elif [ "$pkg" = "sine" ] || [ "$pkg" = "sine-bin" ]; then
            pkg="archrepo/sine"
        elif [ "$pkg" = "hyprmod" ] || [ "$pkg" = "hyprmod-bin" ] || [ "$pkg" = "hyprmod-git" ]; then
            pkg="archrepo/hyprmod"
        elif [ "$pkg" = "zed" ]; then
            pkg="zed-globalmenu"
        fi

        local pkg_name="${pkg#*/}"

        # If archrepo package is targeted and conflicting legacy/bin package is installed, remove it to replace cleanly.
        # Only when the replacement is actually available, otherwise the app would just disappear.
        local legacy_pkg=""
        case "$pkg_name" in
            proton-mail) legacy_pkg="proton-mail-bin" ;;
            proton-pass) legacy_pkg="proton-pass-bin" ;;
            freedownloadmanager) legacy_pkg="freedownloadmanager-bin" ;;
            sine) legacy_pkg="sine-bin" ;;
            hyprmod)
                if pacman -Qq hyprmod-bin >/dev/null 2>&1; then legacy_pkg="hyprmod-bin"; else legacy_pkg="hyprmod-git"; fi
                ;;
            zed-globalmenu) legacy_pkg="zed" ;;
        esac
        if [ -n "$legacy_pkg" ] && pacman -Qq "$legacy_pkg" >/dev/null 2>&1; then
            if pacman -Si "$pkg" >/dev/null 2>&1; then
                log "INFO" "Detected $legacy_pkg; removing to replace with $pkg..."
                pacman -Rdd --noconfirm "$legacy_pkg" >> "$LOG_FILE" 2>&1 || true
            else
                log "WARN" "$pkg is not available in the sync databases; keeping installed $legacy_pkg."
            fi
        fi

        local is_installed=false
        if [[ "$pkg" == *"/"* ]]; then
            local repo="${pkg%%/*}"
            if pacman -Sl "$repo" 2>/dev/null | grep -E "^${repo}[[:space:]]+${pkg_name}[[:space:]]" | grep -q '\[installed\]$'; then
                is_installed=true
            fi
        elif pacman -Q "$pkg" >/dev/null 2>&1; then
            is_installed=true
        fi

        if [ "$is_installed" = true ]; then
            local status="[UP-TO-DATE] $pkg"
            log "INFO" "Package $pkg already installed."
            if [ "$is_auto" = true ]; then
                render_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" "$current_idx" "$s_total" "$title" "$pkg (Already installed)" "$s_start" "$status" "✔"
            else
                render_single_dashboard "$current_idx" "$s_total" "$title" "$pkg (Already installed)" "$s_start" "$status" "✔"
            fi
            continue
        fi

        local install_pkg="$pkg"
        if [ "$pkg" = "vesktop-bin" ]; then
            if ! pacman -Si vesktop-bin >/dev/null 2>&1 && pacman -Si vesktop >/dev/null 2>&1; then
                install_pkg="vesktop"
            fi
        fi

        local cmd_rc=0
        exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                           "$((current_idx - 1))" "$s_total" "$title" "Installing $install_pkg (pacman)" "$s_start" \
                           "pacman -S --needed --noconfirm '$install_pkg'" || cmd_rc=$?

        local status="[SUCCESS] $install_pkg installed"
        local mark="✔"
        if (( cmd_rc != 0 )); then
            if [ -n "${TARGET_USER:-}" ] && [ "$TARGET_USER" != "root" ] && command -v yay >/dev/null 2>&1 \
                && grant_yay_sudo; then
                log "WARN" "pacman failed for $install_pkg, attempting fallback with yay as $TARGET_USER..."
                # stdin from /dev/null: any prompt yay still raises fails fast instead of
                # waiting forever behind the dashboard
                cmd_rc=0
                exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                                   "$((current_idx - 1))" "$s_total" "$title" "Building $install_pkg (yay)" "$s_start" \
                                   "run_as_user yay -S --needed --noconfirm --answerclean None --answerdiff None --answeredit None '$install_pkg' </dev/null" || cmd_rc=$?
                revoke_yay_sudo
                if (( cmd_rc == 0 )); then
                    log "INFO" "Successfully installed $install_pkg via yay"
                    status="[SUCCESS] $install_pkg installed (yay)"
                    mark="✔"
                else
                    log "ERROR" "Failed to install $install_pkg via yay as well."
                    add_issue "Package failed to install: $install_pkg"
                    status="[FAILED] $install_pkg (see log)"
                    mark="✘"
                fi
            else
                log "ERROR" "Failed to install $install_pkg. Recorded in $LOG_FILE"
                add_issue "Package failed to install: $install_pkg"
                status="[FAILED] $install_pkg (see log)"
                mark="✘"
            fi
        fi

        if [ "$is_auto" = true ]; then
            render_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" "$current_idx" "$s_total" "$title" "$install_pkg" "$s_start" "$status" "$mark"
        else
            render_single_dashboard "$current_idx" "$s_total" "$title" "$install_pkg" "$s_start" "$status" "$mark"
        fi
    done

    log "INFO" "Completed package installation phase."
    sleep 1
}

# ------------------------------------------------------------------------------
# State 5: Setup services (VPN, Vietnamese IME, Howdy Face Auth, Valkey/SearXNG)
# ------------------------------------------------------------------------------
do_setup_services() {
    local is_auto=${1:-false}
    local o_start=${2:-0}
    local o_step=${3:-4}
    local o_total=${4:-6}

    local s_start; s_start=$(date +%s)
    local s_total=4
    local title="Setup Services"

    update_p() {
        local s_step=$1
        local s_item=$2
        local log_msg=$3
        log "INFO" "Services Step $s_step: $s_item ($log_msg)"
        if [ "$is_auto" = true ]; then
            draw_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" "$s_step" "$s_total" "$title" "$s_item" "$s_start" "$log_msg"
        else
            draw_single_dashboard "$s_step" "$s_total" "$title" "$s_item" "$s_start" "$log_msg"
        fi
    }

    # Sub-step 1: ProtonVPN (packages only — no env/autostart files)
    update_p 1 "ProtonVPN" "Installing ProtonVPN CLI package for $TARGET_USER..."
    exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                       1 "$s_total" "$title" "Installing proton-vpn-cli" "$s_start" \
                       "pacman -S --needed --noconfirm proton-vpn-cli" || true

    # Sub-step 2: Vietnamese typing (Fcitx5 packages only — no env/autostart files)
    update_p 2 "Vietnamese Typing (Fcitx5)" "Installing Fcitx5 packages (no environment file changes)..."
    exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                       2 "$s_total" "$title" "Installing Fcitx5 typing packages" "$s_start" \
                       "pacman -S --needed --noconfirm fcitx5 fcitx5-configtool fcitx5-gtk fcitx5-qt fcitx5-unikey" || true

    # Sub-step 3: Face Recognition (Howdy-Next Setup)
    update_p 3 "Face Recognition (Howdy)" "Detecting sensor, models & PAM rules..."
    exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                       3 "$s_total" "$title" "Installing v4l-utils for camera detection" "$s_start" \
                       "pacman -S --needed --noconfirm v4l-utils" || true

    local target_device=""
    for dev in /dev/video*; do
        [ -e "$dev" ] || continue
        if v4l2-ctl --device="$dev" --list-formats 2>/dev/null | grep -qE "'GREY'|'Y16 '"; then
            target_device="$dev"
            break
        fi
    done
    if [ -z "$target_device" ]; then
        target_device="/dev/video2"
    fi
    log "INFO" "Howdy device node resolved to $target_device"

    local models_dir="/usr/share/howdy/models"
    if [ -d "/usr/share/howdy" ]; then
        if [ ! -f "${models_dir}/face_detection_yunet_2026may.onnx" ] && \
           [ ! -f "${models_dir}/face_detection_yunet_2023mar.onnx" ]; then
            exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                               3 "$s_total" "$title" "Downloading Howdy AI face models" "$s_start" \
                               "howdy download-models" || true
        fi
    fi

    if [ -f /etc/howdy/config.ini ]; then
        howdy set device_path "$target_device" >> "$LOG_FILE" 2>&1 || true
        howdy set detection_notice true >> "$LOG_FILE" 2>&1 || true
    fi

    local pam_rule="auth      sufficient pam_howdy.so workaround=native"
    local howdy_pam_module=""
    for m in /usr/lib/security/pam_howdy.so /lib/security/pam_howdy.so /usr/lib64/security/pam_howdy.so; do
        if [ -e "$m" ]; then howdy_pam_module="$m"; break; fi
    done
    patch_pam() {
        local pfile="$1"
        local fallback="$2"
        if [ ! -f "$pfile" ] && [ -n "$fallback" ] && [ -f "$fallback" ]; then
            mkdir -p "$(dirname "$pfile")"
            cp "$fallback" "$pfile"
        fi
        if [ -f "$pfile" ]; then
            if ! grep -q "pam_howdy.so" "$pfile"; then
                cp "$pfile" "${pfile}.bak.$(date +%Y%m%d%H%M%S)"
                if grep -q "^#%PAM-1.0" "$pfile"; then
                    sed -i "/^#%PAM-1.0/a ${pam_rule}" "$pfile"
                else
                    sed -i "1i ${pam_rule}" "$pfile"
                fi
                log "INFO" "Patched PAM file $pfile for Howdy"
            fi
        fi
    }

    # Referencing a PAM module that does not exist breaks the whole auth stack
    # (sudo/login fail with "Module is unknown"), so only patch PAM once Howdy is installed.
    if [ -z "$howdy_pam_module" ]; then
        log "WARN" "pam_howdy.so not found (howdy-next not installed?). Skipping PAM patching and resume hook."
        add_issue "Howdy not installed; face authentication was not configured"
    else
        patch_pam "/etc/pam.d/sudo" ""
        patch_pam "/etc/pam.d/polkit-1" "/usr/lib/pam.d/polkit-1"
        patch_pam "/etc/pam.d/system-local-login" "/usr/lib/pam.d/system-local-login"
        patch_pam "/etc/pam.d/plasmalogin" "/usr/lib/pam.d/plasmalogin"
        patch_pam "/etc/pam.d/sddm" "/usr/lib/pam.d/sddm"
        patch_pam "/etc/pam.d/gdm-password" "/usr/lib/pam.d/gdm-password"
        patch_pam "/etc/pam.d/hyprlock" "/usr/lib/pam.d/hyprlock"
        patch_pam "/etc/pam.d/swaylock" "/usr/lib/pam.d/swaylock"

        local hook_file="/usr/lib/systemd/system-sleep/howdy-resume.sh"
        mkdir -p "$(dirname "$hook_file")"
        cat > "$hook_file" << EOF
#!/bin/sh
case \$1/\$2 in
  pre/*)
    /usr/bin/modprobe -r uvcvideo 2>/dev/null
    ;;
  post/*)
    /usr/bin/modprobe uvcvideo 2>/dev/null
    /usr/bin/udevadm settle --timeout=5 2>/dev/null
    count=0
    while [ ! -c ${target_device} ] && [ \$count -lt 30 ]; do
        sleep 0.1
        count=\$((count + 1))
    done
    sleep 0.5
    ;;
esac
EOF
        chmod +x "$hook_file"
    fi

    # Sub-step 4: Local search engine (Valkey & SearXNG)
    update_p 4 "Search Services" "Enabling and starting valkey & searxng..."
    systemctl enable --now valkey >> "$LOG_FILE" 2>&1 || add_issue "Could not enable/start valkey"
    systemctl enable --now searxng >> "$LOG_FILE" 2>&1 || add_issue "Could not enable/start searxng"

    sleep 1
    log "INFO" "Services setup completed."
}

# ==============================================================================
# EMBEDDED MODULE: APP VAULT SYNC (Backup, Export, Import & Status)
# Fully integrated - zero external script dependencies
# ==============================================================================
# --- Color formatting ---
COLOR_RESET="\033[0m"
COLOR_BOLD="\033[1m"
COLOR_RED="\033[31m"
COLOR_GREEN="\033[32m"
COLOR_YELLOW="\033[33m"
COLOR_BLUE="\033[34m"
COLOR_CYAN="\033[36m"
COLOR_GRAY="\033[90m"

log_info() {
    echo -e "${COLOR_BLUE}[INFO]${COLOR_RESET} $*"
}

log_success() {
    echo -e "${COLOR_GREEN}[SUCCESS]${COLOR_RESET} $*"
}

log_warn() {
    echo -e "${COLOR_YELLOW}[WARN]${COLOR_RESET} $*" >&2
}

log_error() {
    echo -e "${COLOR_RED}[ERROR]${COLOR_RESET} $*" >&2
}

log_step() {
    echo -e "${COLOR_CYAN}==>${COLOR_RESET} ${COLOR_BOLD}$*${COLOR_RESET}"
}

# --- Dependencies Check ---
check_tool() {
    command -v "$1" >/dev/null 2>&1
}

ARCHIVER=""
UNARCHIVER=""

# Set by cmd_backup/cmd_import when the user declines a prompt, so callers
# can tell "canceled" apart from "finished" (both return 0).
VAULT_CANCELED=false

# Only the vault commands need these; checking lazily keeps the setup steps usable
# on a fresh install where jq/rsync aren't present yet.
require_vault_tools() {
    local missing=()
    check_tool rsync || missing+=("rsync")
    check_tool jq || missing+=("jq")
    if [[ ${#missing[@]} -gt 0 && "$EUID" -eq 0 ]] && check_tool pacman; then
        log_info "Installing missing vault dependencies: ${missing[*]}"
        pacman -S --needed --noconfirm "${missing[@]}" >> "$LOG_FILE" 2>&1 || true
        missing=()
        check_tool rsync || missing+=("rsync")
        check_tool jq || missing+=("jq")
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        log_error "Required tool(s) not installed: ${missing[*]}"
        return 1
    fi

    if check_tool 7z; then
        ARCHIVER="7z"
    elif check_tool zip; then
        ARCHIVER="zip"
    elif check_tool bsdtar; then
        ARCHIVER="bsdtar"
    elif check_tool python3; then
        ARCHIVER="python3"
    else
        log_error "No supported zip archiver found. Please install 7zip, zip, or bsdtar."
        return 1
    fi

    if check_tool unzip; then
        UNARCHIVER="unzip"
    elif check_tool 7z; then
        UNARCHIVER="7z"
    elif check_tool bsdtar; then
        UNARCHIVER="bsdtar"
    fi
}

# --- Target User & Home Resolution (Safe under sudo) ---
ACTUAL_USER="${TARGET_USER:-${SUDO_USER:-$USER}}"
ACTUAL_HOME="${TARGET_HOME:-$HOME}"
# The vault works on the target user's folders through these script-local variables.
# HOME/XDG_* are deliberately NOT exported to the user's home: everything this script
# runs as root (pacman hooks, package scriptlets, howdy, archivers...) would otherwise
# write root-owned cache/config files into the user's home.
if [[ "$EUID" -eq 0 && "$ACTUAL_USER" != "root" ]]; then
    VAULT_CONFIG_HOME="$ACTUAL_HOME/.config"
    VAULT_DATA_HOME="$ACTUAL_HOME/.local/share"
    VAULT_STATE_HOME="$ACTUAL_HOME/.local/state"
    VAULT_CACHE_HOME="$ACTUAL_HOME/.cache"
else
    VAULT_CONFIG_HOME="${XDG_CONFIG_HOME:-$ACTUAL_HOME/.config}"
    VAULT_DATA_HOME="${XDG_DATA_HOME:-$ACTUAL_HOME/.local/share}"
    VAULT_STATE_HOME="${XDG_STATE_HOME:-$ACTUAL_HOME/.local/state}"
    VAULT_CACHE_HOME="${XDG_CACHE_HOME:-$ACTUAL_HOME/.cache}"
fi

# --- Default Paths & Detection ---
DEFAULT_BACKUP_DIR="${VAULT_DATA_HOME}/app-vault-sync/backups"
SAFETY_BACKUP_DIR="${VAULT_STATE_HOME}/app-vault-sync/safety-rollbacks"

# Beeper Paths
find_beeper_config() {
    if [[ -d "$VAULT_CONFIG_HOME/BeeperTexts" ]]; then
        echo "$VAULT_CONFIG_HOME/BeeperTexts"
    elif [[ -d "$VAULT_CONFIG_HOME/Beeper" ]]; then
        echo "$VAULT_CONFIG_HOME/Beeper"
    elif [[ -d "$VAULT_CONFIG_HOME/beeper" ]]; then
        echo "$VAULT_CONFIG_HOME/beeper"
    fi
}

# Zalo Paths
find_zalo_config() {
    if [[ -d "$VAULT_CONFIG_HOME/ZaloData" ]]; then
        echo "$VAULT_CONFIG_HOME/ZaloData"
    elif [[ -d "$VAULT_CONFIG_HOME/Zalo" ]]; then
        echo "$VAULT_CONFIG_HOME/Zalo"
    fi
}

find_zalo_flags() {
    if [[ -f "$VAULT_CONFIG_HOME/zalo-flags.conf" ]]; then
        echo "$VAULT_CONFIG_HOME/zalo-flags.conf"
    fi
}

# Vesktop Paths
find_vesktop_config() {
    if [[ -d "$VAULT_CONFIG_HOME/vesktop" ]]; then
        echo "$VAULT_CONFIG_HOME/vesktop"
    fi
}

# Zen Browser Paths
find_zen_config() {
    if [[ -d "$VAULT_CONFIG_HOME/zen" ]]; then
        echo "$VAULT_CONFIG_HOME/zen"
    elif [[ -d "$ACTUAL_HOME/.zen" ]]; then
        echo "$ACTUAL_HOME/.zen"
    fi
}

# Obsidian Paths
find_obsidian_config() {
    if [[ -d "$VAULT_CONFIG_HOME/obsidian" ]]; then
        echo "$VAULT_CONFIG_HOME/obsidian"
    fi
}

# Fcitx5 Paths
find_fcitx5_config() {
    if [[ -d "$VAULT_CONFIG_HOME/fcitx5" ]]; then
        echo "$VAULT_CONFIG_HOME/fcitx5"
    fi
}

# Hyprland Paths
find_hypr_config() {
    if [[ -d "$VAULT_CONFIG_HOME/hypr" ]]; then
        echo "$VAULT_CONFIG_HOME/hypr"
    fi
}

# KDE Plasma settings (XDG config + share). Curated from:
# Konsave conf_kde.yaml, ArchWiki KDE (~/.config), KDE Discuss migration threads,
# and official KDE Plasma 6 specifications. Intentionally omits caches, Akonadi heavy DBs,
# Baloo indexing DBs, and non-KDE application configs (browsers, Steam, etc.).
KDE_CONFIG_ENTRIES=(
    gtk-2.0 gtk-3.0 gtk-4.0 gtkrc gtkrc-2.0 xsettingsd
    kate Kvantum latte fontconfig
    dolphinrc katerc konsolerc konsolesshconfig
    kcminputrc kdeglobals kglobalshortcutsrc kxkbrc touchpadrc
    klipperrc krunnerrc kscreenlockerrc ksmserverrc
    kwinrc kwinrulesrc kwinoutputconfig.json
    plasma-org.kde.plasma.desktop-appletsrc plasmarc plasmashellrc
    lattedockrc breezerc oxygenrc lightlyrc ksplashrc khotkeysrc
    systemsettingsrc kdedrc kded5rc kded6rc kconf_updaterc kscreenrc
    plasma-systemmonitorrc spectaclerc plasmanotifyrc
    plasma-localerc plasma_calendar_holiday_regions plasma_calendar_alternatecalendar
    plasmacamerarc plasma-welcomerc
    xdg-desktop-portal-kderc kactivitymanagerdrc kactivitymanagerd-statsrc kactivitymanagerd
    baloofilerc mimeapps.list filetypesrc Trolltech.conf
    powermanagementprofilesrc powerdevilrc kcmdisplayrc plasmawindowedrc
    bluedevilglobalrc kiorc kioworkerrc kmenueditrc kwalletrc
    arkrc gwenviewrc kaccessrc klaunchrc kalarmrc kclockrc kclockdrc trashrc
    kdedefaults plasma-workspace autostart session
)
KDE_SHARE_ENTRIES=(
    plasma kwin konsole fonts color-schemes aurorae icons wallpapers yakuake
    kxmlgui5 kxmlgui6 knewstuff3 kactivitymanagerd palettes iconthemes
    user-places.xbel krunnerstaterc dolphin plasma-systemmonitor
    gwenview haruna kate themes
)
KDE_HOME_ENTRIES=(
    .fonts .themes .icons
)

kde_config_root() {
    echo "$VAULT_CONFIG_HOME"
}

kde_share_root() {
    echo "$VAULT_DATA_HOME"
}

kde_has_settings() {
    local cfg
    cfg="$(kde_config_root)"
    [[ -f "$cfg/kdeglobals" || -f "$cfg/kwinrc" || -f "$cfg/plasmashellrc" || -f "$cfg/plasma-org.kde.plasma.desktop-appletsrc" ]]
}

kde_config_entries() {
    local cfg; cfg="$(kde_config_root)"
    local -A seen=()
    local entries=()
    # 1. Curated baseline configuration files & directories
    for e in "${KDE_CONFIG_ENTRIES[@]}"; do
        if [[ -e "$cfg/$e" && -z "${seen[$e]:-}" ]]; then
            seen["$e"]=1
            entries+=("$e")
        fi
    done
    # 2. Dynamically include any active plasma* files and k*rc files in config root
    if [[ -d "$cfg" ]]; then
        for f in "$cfg"/plasma* "$cfg"/k*rc "$cfg"/*rc; do
            [[ -f "$f" ]] || continue
            local b; b="$(basename "$f")"
            if [[ -z "${seen[$b]:-}" ]]; then
                case "$b" in
                    bashrc|zshrc|vimrc|wgetrc|nanorc|inputrc|screenrc|tmux.conf|geminirc) continue ;;
                    *)
                        seen["$b"]=1
                        entries+=("$b")
                        ;;
                esac
            fi
        done
    fi
    printf '%s\n' "${entries[@]}"
}

kde_existing_paths() {
    local cfg share home_root
    cfg="$(kde_config_root)"
    share="$(kde_share_root)"
    home_root="${ACTUAL_HOME:-$HOME}"
    local e
    while IFS= read -r e; do
        [[ -n "$e" && -e "$cfg/$e" ]] && printf '%s\n' "$cfg/$e"
    done < <(kde_config_entries)
    for e in "${KDE_SHARE_ENTRIES[@]}"; do
        [[ -e "$share/$e" ]] && printf '%s\n' "$share/$e"
    done
    for e in "${KDE_HOME_ENTRIES[@]}"; do
        [[ -e "$home_root/$e" ]] && printf '%s\n' "$home_root/$e"
    done
}

kde_format_total_size() {
    local paths=()
    mapfile -t paths < <(kde_existing_paths)
    if [[ ${#paths[@]} -eq 0 ]]; then
        echo "0B"
        return
    fi
    du -sch "${paths[@]}" 2>/dev/null | tail -n1 | cut -f1 || echo "0B"
}

# Copy named entries from src_root into dest_root (rsync -a each existing name).
kde_copy_named() {
    local src_root="$1"
    local dest_root="$2"
    local dry_run="${3:-false}"
    shift 3
    local e
    [[ "$dry_run" == true ]] || mkdir -p "$dest_root"
    for e in "$@"; do
        [[ -e "$src_root/$e" ]] || continue
        if [[ "$dry_run" == true ]]; then
            log_info "  [Dry-run] Would sync: $src_root/$e -> $dest_root/"
        else
            rsync -a "$src_root/$e" "$dest_root/"
        fi
    done
}

# Antigravity Paths
find_antigravity_config() {
    if [[ -d "$VAULT_CONFIG_HOME/Antigravity" ]]; then
        echo "$VAULT_CONFIG_HOME/Antigravity"
    elif [[ -d "$VAULT_CONFIG_HOME/antigravity" ]]; then
        echo "$VAULT_CONFIG_HOME/antigravity"
    fi
}

# Howdy Face Model Paths
find_howdy_model() {
    local target_user="${1:-$ACTUAL_USER}"
    local p="/etc/howdy/models/${target_user}.dat"
    if [[ -f "$p" ]]; then
        echo "$p"
    elif [[ "$EUID" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
        if sudo -n test -f "$p" 2>/dev/null; then
            echo "$p"
        fi
    fi
}

find_howdy_models_dir() {
    if [[ -d "/etc/howdy/models" ]]; then
        echo "/etc/howdy/models"
    elif [[ "$EUID" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
        if sudo -n test -d "/etc/howdy/models" 2>/dev/null; then
            echo "/etc/howdy/models"
        fi
    fi
}

# Process Checking
get_running_pids() {
    local app="$1"
    local pids=""
    # Only the target user's processes (as root, -k must never kill other users' apps).
    # -f patterns use [^ ]* so a match stays inside one /usr or /opt path instead of
    # spanning arguments (".*zen" also matched e.g. "vim /usr/share/doc/citizen.txt").
    local u="${ACTUAL_USER:-$(id -un)}"
    case "$app" in
        beeper|beepertexts)
            pids=$(pgrep -u "$u" -x beeper 2>/dev/null || pgrep -u "$u" -x beepertexts 2>/dev/null || pgrep -u "$u" -f "/opt/beeper/beepertexts" 2>/dev/null || true)
            ;;
        zalo|zalodata)
            pids=$(pgrep -u "$u" -x zalo 2>/dev/null || pgrep -u "$u" -f "/opt/zalo-for-linux/zalo" 2>/dev/null || true)
            ;;
        vesktop)
            pids=$(pgrep -u "$u" -x vesktop 2>/dev/null || pgrep -u "$u" -f "/(usr|opt)/[^ ]*/vesktop(/| |$)" 2>/dev/null || true)
            ;;
        zen)
            pids=$(pgrep -u "$u" -x zen-bin 2>/dev/null || pgrep -u "$u" -x zen 2>/dev/null || pgrep -u "$u" -x zen-browser 2>/dev/null || pgrep -u "$u" -f "/(usr|opt)/([^ ]*/)?zen[^ /]*/zen[^ /]*( |$)" 2>/dev/null || true)
            ;;
        obsidian)
            pids=$(pgrep -u "$u" -x obsidian 2>/dev/null || pgrep -u "$u" -f "/(usr|opt)/(lib|bin|share)/obsidian" 2>/dev/null || true)
            ;;
        fcitx5)
            pids=$(pgrep -u "$u" -x fcitx5 2>/dev/null || pgrep -u "$u" -x fcitx5-daemon 2>/dev/null || true)
            ;;
        antigravity)
            pids=$(pgrep -u "$u" -x antigravity 2>/dev/null || pgrep -u "$u" -f "/(usr|opt)/[^ ]*/antigravity(/| |$)" 2>/dev/null || true)
            ;;
        howdy)
            # Howdy runs as root from PAM, so not filtered by user
            pids=$(pgrep -x howdy 2>/dev/null || pgrep -x howdy_compare 2>/dev/null || true)
            ;;
        kde|plasma|kde-plasma)
            # Do not terminate plasmashell/kwin; overlaying configs while Plasma is
            # running is safe enough to warn about, but killing the session is not.
            pids=""
            ;;
        hypr|hyprland)
            # Do not terminate Hyprland compositor session during backup/restore
            pids=""
            ;;
    esac
    # Filter out current script PID and format with spaces
    echo "$pids" | tr '\n' ' ' | sed -e "s/\b$$\b//g" -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

format_bytes() {
    local path="$1"
    if [[ -e "$path" ]]; then
        du -sh "$path" 2>/dev/null | cut -f1
    else
        echo "0B"
    fi
}

# Graceful Application Termination
terminate_apps() {
    local apps=("$@")
    local failed=false
    for app in "${apps[@]}"; do
        local pids
        pids="$(get_running_pids "$app")"
        if [[ -n "$pids" ]]; then
            log_step "Closing $app gracefully (PID: $pids)..."
            # Send SIGTERM first so databases commit and close gracefully
            kill $pids 2>/dev/null || true

            local count=0
            local stopped=false
            while [[ $count -lt 14 ]]; do
                sleep 0.5
                pids="$(get_running_pids "$app")"
                if [[ -z "$pids" ]]; then
                    stopped=true
                    break
                fi
                ((count++)) || true
            done

            if [[ "$stopped" == false ]]; then
                log_warn "$app did not shut down gracefully within 7 seconds. Forcing termination (SIGKILL)..."
                kill -9 $pids 2>/dev/null || true
                sleep 0.5
            fi

            pids="$(get_running_pids "$app")"
            if [[ -z "$pids" ]]; then
                log_success "$app terminated successfully."
            else
                log_error "Failed to stop $app (PID: $pids)."
                failed=true
            fi
        fi
    done

    if [[ "$failed" == true ]]; then
        return 1
    fi
    return 0
}

# --- Zip Operations ---
create_zip_archive() {
    local src_dir="$1"
    local zip_file="$2"
    mkdir -p "$(dirname "$zip_file")"
    rm -f "$zip_file"

    local current_pwd="$PWD"
    cd "$src_dir"

    case "$ARCHIVER" in
        7z)
            7z a -tzip -mx=6 "$zip_file" . >/dev/null
            ;;
        zip)
            zip -r -q "$zip_file" .
            ;;
        bsdtar)
            bsdtar -a -cf "$zip_file" .
            ;;
        python3)
            python3 -c "
import zipfile, os, sys
zf = zipfile.ZipFile(sys.argv[1], 'w', zipfile.ZIP_DEFLATED)
for root, dirs, files in os.walk('.'):
    for f in files:
        full_path = os.path.join(root, f)
        arcname = os.path.normpath(full_path)
        zf.write(full_path, arcname)
zf.close()
" "$zip_file"
            ;;
    esac

    cd "$current_pwd"
}

extract_zip_archive() {
    local zip_file="$1"
    local dest_dir="$2"
    mkdir -p "$dest_dir"

    case "$UNARCHIVER" in
        unzip)
            unzip -q -o "$zip_file" -d "$dest_dir"
            ;;
        7z)
            7z x -y -o"$dest_dir" "$zip_file" >/dev/null
            ;;
        bsdtar)
            bsdtar -xf "$zip_file" -C "$dest_dir"
            ;;
        *)
            python3 -c "
import zipfile, sys
with zipfile.ZipFile(sys.argv[1], 'r') as z:
    z.extractall(sys.argv[2])
" "$zip_file" "$dest_dir"
            ;;
    esac
}

read_zip_manifest() {
    local zip_file="$1"
    case "$UNARCHIVER" in
        unzip)
            unzip -p "$zip_file" backup_manifest.json 2>/dev/null || unzip -p "$zip_file" "*/backup_manifest.json" 2>/dev/null || true
            ;;
        7z)
            7z x -so "$zip_file" backup_manifest.json 2>/dev/null || true
            ;;
        bsdtar)
            bsdtar -O -xf "$zip_file" backup_manifest.json 2>/dev/null || true
            ;;
        *)
            python3 -c "
import zipfile, sys
try:
    with zipfile.ZipFile(sys.argv[1], 'r') as z:
        print(z.read('backup_manifest.json').decode('utf-8'))
except Exception:
    pass
" "$zip_file" 2>/dev/null || true
            ;;
    esac
}

# --- Safe Staging Directory Management (Avoids tmpfs/RAM exhaustion) ---
cleanup_stale_staging() {
    for c_dir in "/tmp" "/var/tmp" "/media/data/.vault-tmp" "$VAULT_CACHE_HOME/app-vault-sync"; do
        if [[ -d "$c_dir" && -w "$c_dir" ]]; then
            find "$c_dir" -maxdepth 1 \( -name "app-vault-sync-backup-*" -o -name "app-vault-sync-restore-*" \) \( -empty -o -mmin +15 \) -exec rm -rf {} + 2>/dev/null || true
        fi
    done
}

get_safe_staging_dir() {
    local prefix="$1"
    cleanup_stale_staging

    # 1. /media/data (high capacity nvme disk) - only if it keeps Unix permissions. On the
    #    NTFS data partition (umask=000) every staged file becomes 0777, which would be baked
    #    into the archive and restored into ~/.config that way.
    local data_fs
    data_fs="$(findmnt -no FSTYPE --target /media/data 2>/dev/null || true)"
    if [[ -d "/media/data" && -w "/media/data" ]] && [[ ! "$data_fs" =~ ^(ntfs|ntfs3|fuseblk|vfat|exfat|msdos)$ ]]; then
        local p="/media/data/.vault-tmp"
        mkdir -p "$p" 2>/dev/null || true
        if [[ "$EUID" -eq 0 && -n "${TARGET_UID:-}" && "$TARGET_UID" != "0" ]]; then
            chown "${TARGET_UID}:${TARGET_GID:-1000}" "$p" 2>/dev/null || true
        fi
        if [[ -d "$p" && -w "$p" ]]; then
            mktemp -d "${p}/${prefix}-XXXXXX"
            return
        fi
    fi

    # 2. /var/tmp (persistent nvme partition, NOT tmpfs/RAM)
    if [[ -d "/var/tmp" && -w "/var/tmp" ]]; then
        mktemp -d "/var/tmp/${prefix}-XXXXXX"
        return
    fi

    # 3. User cache directory
    local cache_dir="$VAULT_CACHE_HOME/app-vault-sync"
    mkdir_user "$cache_dir" 2>/dev/null || true
    if [[ -d "$cache_dir" && -w "$cache_dir" ]]; then
        mktemp -d "${cache_dir}/${prefix}-XXXXXX"
        return
    fi

    # 4. Fallback to /tmp
    mktemp -d "/tmp/${prefix}-XXXXXX"
}

# --- Status Command ---
cmd_status() {
    echo -e "${COLOR_BOLD}=== Applications Status & Storage Breakdown ===${COLOR_RESET}\n"

    # [1] BeeperTexts
    echo -e "${COLOR_CYAN}[1] BeeperTexts (~/.config/BeeperTexts)${COLOR_RESET}"
    local beeper_conf beeper_pids
    beeper_conf="$(find_beeper_config)"
    beeper_pids="$(get_running_pids beeper)"
    if [[ -n "$beeper_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $beeper_conf ($(format_bytes "$beeper_conf"))"
        if [[ -d "$beeper_conf/media" ]]; then
            echo -e "  Media:       $(format_bytes "$beeper_conf/media")"
        fi
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$beeper_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $beeper_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [2] ZaloData
    echo -e "${COLOR_CYAN}[2] ZaloData (~/.config/ZaloData)${COLOR_RESET}"
    local zalo_conf zalo_flags zalo_pids
    zalo_conf="$(find_zalo_config)"
    zalo_flags="$(find_zalo_flags)"
    zalo_pids="$(get_running_pids zalo)"
    if [[ -n "$zalo_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $zalo_conf ($(format_bytes "$zalo_conf"))"
        if [[ -n "$zalo_flags" ]]; then
            echo -e "  Flags File:  $zalo_flags"
        fi
        if [[ -d "$zalo_conf/media" ]]; then
            echo -e "  Media:       $(format_bytes "$zalo_conf/media")"
        fi
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$zalo_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $zalo_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [3] Vesktop
    echo -e "${COLOR_CYAN}[3] Vesktop (~/.config/vesktop)${COLOR_RESET}"
    local vesk_conf vesk_pids
    vesk_conf="$(find_vesktop_config)"
    vesk_pids="$(get_running_pids vesktop)"
    if [[ -n "$vesk_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $vesk_conf ($(format_bytes "$vesk_conf"))"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$vesk_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $vesk_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [4] Zen Browser
    echo -e "${COLOR_CYAN}[4] Zen Browser (~/.config/zen)${COLOR_RESET}"
    local zen_conf zen_pids
    zen_conf="$(find_zen_config)"
    zen_pids="$(get_running_pids zen)"
    if [[ -n "$zen_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $zen_conf ($(format_bytes "$zen_conf"))"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$zen_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $zen_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [5] Obsidian
    echo -e "${COLOR_CYAN}[5] Obsidian (~/.config/obsidian)${COLOR_RESET}"
    local obs_conf obs_pids
    obs_conf="$(find_obsidian_config)"
    obs_pids="$(get_running_pids obsidian)"
    if [[ -n "$obs_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $obs_conf ($(format_bytes "$obs_conf"))"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$obs_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $obs_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [6] Fcitx5
    echo -e "${COLOR_CYAN}[6] Fcitx5 (~/.config/fcitx5)${COLOR_RESET}"
    local fcitx_conf fcitx_pids
    fcitx_conf="$(find_fcitx5_config)"
    fcitx_pids="$(get_running_pids fcitx5)"
    if [[ -n "$fcitx_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $fcitx_conf ($(format_bytes "$fcitx_conf"))"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$fcitx_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $fcitx_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [7] Antigravity
    echo -e "${COLOR_CYAN}[7] Antigravity (~/.config/Antigravity)${COLOR_RESET}"
    local ag_conf ag_pids
    ag_conf="$(find_antigravity_config)"
    ag_pids="$(get_running_pids antigravity)"
    if [[ -n "$ag_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Installed${COLOR_RESET}"
        echo -e "  Config Dir:  $ag_conf ($(format_bytes "$ag_conf"))"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if [[ -n "$ag_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Running (PID: $ag_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Stopped${COLOR_RESET}"
    fi
    echo ""

    # [8] Howdy Face Recognition
    echo -e "${COLOR_CYAN}[8] Howdy Face Recognition (/etc/howdy/models)${COLOR_RESET}"
    local howdy_pkg=""
    if pacman -Q howdy-next >/dev/null 2>&1; then
        howdy_pkg="howdy-next ($(pacman -Q howdy-next 2>/dev/null | awk '{print $2}'))"
    elif pacman -Q howdy-next-bin >/dev/null 2>&1; then
        howdy_pkg="howdy-next-bin ($(pacman -Q howdy-next-bin 2>/dev/null | awk '{print $2}'))"
    elif pacman -Q howdy >/dev/null 2>&1; then
        howdy_pkg="howdy ($(pacman -Q howdy 2>/dev/null | awk '{print $2}'))"
    elif command -v howdy >/dev/null 2>&1; then
        howdy_pkg="Installed ($(command -v howdy))"
    fi

    local howdy_pids
    howdy_pids="$(get_running_pids howdy)"
    local howdy_model="/etc/howdy/models/${ACTUAL_USER}.dat"

    if [[ -n "$howdy_pkg" ]]; then
        echo -e "  Package:     ${COLOR_GREEN}${howdy_pkg}${COLOR_RESET}"
    else
        echo -e "  Package:     ${COLOR_GRAY}Not detected (pacman / binary)${COLOR_RESET}"
    fi

    local model_exists=false
    local model_size="0B"
    local model_perms=""
    if [[ -f "$howdy_model" ]]; then
        model_exists=true
        model_size="$(format_bytes "$howdy_model")"
        model_perms="$(stat -c '%a %U:%G' "$howdy_model" 2>/dev/null || true)"
    elif [[ "$EUID" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
        if sudo -n test -f "$howdy_model" 2>/dev/null; then
            model_exists=true
            model_size="$(sudo -n du -sh "$howdy_model" 2>/dev/null | cut -f1 || echo "unknown")"
            model_perms="$(sudo -n stat -c '%a %U:%G' "$howdy_model" 2>/dev/null || true)"
        fi
    fi

    if [[ "$model_exists" == true ]]; then
        echo -e "  Face Model:  ${COLOR_GREEN}Enrolled${COLOR_RESET} ($howdy_model, $model_size)"
        echo -e "  Permissions: $model_perms (Expected: 600 root:root)"
        if command -v howdy >/dev/null 2>&1; then
            local face_count
            if [[ "$EUID" -eq 0 ]]; then
                face_count=$(howdy -U "$ACTUAL_USER" list 2>/dev/null | grep -cE '^[0-9]+' || echo "0")
            elif sudo -n true 2>/dev/null; then
                face_count=$(sudo -n howdy -U "$ACTUAL_USER" list 2>/dev/null | grep -cE '^[0-9]+' || echo "?")
            else
                face_count="Permissions required (run with sudo to inspect)"
            fi
            echo -e "  Enrolled ID: ${COLOR_BOLD}${face_count} face profile(s)${COLOR_RESET}"
        fi
    else
        if [[ "$EUID" -ne 0 ]] && ! sudo -n true 2>/dev/null; then
            echo -e "  Face Model:  ${COLOR_GRAY}Run with sudo to inspect /etc/howdy/models${COLOR_RESET}"
        else
            echo -e "  Face Model:  ${COLOR_YELLOW}Not enrolled for ${ACTUAL_USER}${COLOR_RESET} (/etc/howdy/models/${ACTUAL_USER}.dat)"
        fi
    fi

    if [[ -n "$howdy_pids" ]]; then
        echo -e "  Process:     ${COLOR_YELLOW}Active (PID: $howdy_pids)${COLOR_RESET}"
    else
        echo -e "  Process:     ${COLOR_GRAY}Idle / Standby${COLOR_RESET}"
    fi
    echo ""

    # [9] KDE Plasma
    echo -e "${COLOR_CYAN}[9] KDE Plasma (~/.config + ~/.local/share)${COLOR_RESET}"
    if kde_has_settings; then
        echo -e "  Status:      ${COLOR_GREEN}Detected${COLOR_RESET}"
        echo -e "  Config:      $(kde_config_root) (kdeglobals, kwinrc, Plasma applets, shortcuts, …)"
        echo -e "  Data:        $(kde_share_root) (plasma, kwin, icons, wallpapers, …)"
        echo -e "  Snapshot:    $(kde_format_total_size) (curated settings only, not all of ~/.config)"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if pgrep -x plasmashell >/dev/null 2>&1; then
        echo -e "  Session:     ${COLOR_YELLOW}plasmashell running${COLOR_RESET} (log out after restore for a clean apply)"
    else
        echo -e "  Session:     ${COLOR_GRAY}plasmashell not running${COLOR_RESET}"
    fi
    echo ""

    # [10] Hyprland
    echo -e "${COLOR_CYAN}[10] Hyprland (~/.config/hypr)${COLOR_RESET}"
    local hypr_conf
    hypr_conf="$(find_hypr_config)"
    if [[ -n "$hypr_conf" ]]; then
        echo -e "  Status:      ${COLOR_GREEN}Detected${COLOR_RESET}"
        echo -e "  Config Dir:  $hypr_conf ($(format_bytes "$hypr_conf"))"
    else
        echo -e "  Status:      ${COLOR_GRAY}Not detected${COLOR_RESET}"
    fi
    if pgrep -x Hyprland >/dev/null 2>&1; then
        echo -e "  Session:     ${COLOR_YELLOW}Hyprland running${COLOR_RESET}"
    else
        echo -e "  Session:     ${COLOR_GRAY}Hyprland not running${COLOR_RESET}"
    fi
}

# --- Backup / Export Command ---
cmd_backup() {
    VAULT_CANCELED=false
    require_vault_tools || return 1
    local target="all"
    local output_file=""
    local include_media=true
    local include_vaults=true
    local force=false
    local kill_apps=false
    local dry_run=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            all|beeper|beepertexts|zalo|zalodata|vesktop|zen|obsidian|fcitx5|antigravity|howdy|kde|plasma|kde-plasma|hypr|hyprland)
                target="$1"
                [[ "$target" == "beepertexts" ]] && target="beeper"
                [[ "$target" == "zalodata" ]] && target="zalo"
                [[ "$target" == "plasma" || "$target" == "kde-plasma" ]] && target="kde"
                [[ "$target" == "hyprland" ]] && target="hypr"
                shift
                ;;
            -o|--output)
                output_file="$2"
                shift 2
                ;;
            --no-media)
                include_media=false
                shift
                ;;
            --no-vaults)
                # Kept for backward compatibility
                shift
                ;;
            -k|--kill)
                kill_apps=true
                shift
                ;;
            -f|--force)
                force=true
                shift
                ;;
            --dry-run)
                dry_run=true
                shift
                ;;
            -h|--help)
                show_backup_help
                return 0
                ;;
            *)
                log_error "Unknown argument: $1"
                show_backup_help
                return 1
                ;;
        esac
    done

    # Set default output file if unspecified
    local timestamp
    timestamp="$(date +'%Y-%m-%d_%H%M%S')"
    if [[ -z "$output_file" ]]; then
        [[ "$dry_run" == true ]] || mkdir_user "$DEFAULT_BACKUP_DIR"
        output_file="${DEFAULT_BACKUP_DIR}/${target}_backup_${timestamp}.zip"
    fi
    # create_zip_archive cd's into the staging dir, so a relative path would land there and be deleted
    output_file="$(realpath -m -- "$output_file")"

    # Direct Howdy .dat standalone export
    if [[ "$target" == "howdy" && "$output_file" =~ \.dat$ ]]; then
        log_step "Exporting Howdy Face Model (.dat)..."
        local model_file
        model_file="$(find_howdy_model "$ACTUAL_USER")"
        if [[ -z "$model_file" && "$dry_run" == true ]]; then
            model_file="/etc/howdy/models/${ACTUAL_USER}.dat"
        fi
        if [[ -z "$model_file" ]]; then
            log_error "No enrolled face model found for user '${ACTUAL_USER}' at /etc/howdy/models/${ACTUAL_USER}.dat"
            return 1
        fi
        if [[ "$dry_run" == true ]]; then
            log_info "  [Dry-run] Would export face model: $model_file -> $output_file"
            return 0
        fi
        mkdir_user "$(dirname "$output_file")"
        if [[ "$EUID" -eq 0 ]]; then
            cp -p "$model_file" "$output_file"
        else
            sudo cp -p "$model_file" "$output_file"
        fi
        local file_uid="${SUDO_UID:-${TARGET_UID:-1000}}"
        local file_gid="${SUDO_GID:-${TARGET_GID:-1000}}"
        if [[ "$file_uid" != "0" ]]; then
            if [[ "$EUID" -eq 0 ]]; then
                chown "${file_uid}:${file_gid}" "$output_file" 2>/dev/null || true
            else
                sudo chown "${file_uid}:${file_gid}" "$output_file" 2>/dev/null || true
            fi
        fi
        log_success "Exported Howdy face model for '${ACTUAL_USER}' to: ${output_file}"
        return 0
    fi

    log_step "Preparing backup for target: ${COLOR_BOLD}$target${COLOR_RESET}"
    log_info "Archive format:       ZIP ($ARCHIVER)"
    log_info "Output path:          $output_file"
    log_info "Include media:        $include_media"
    if [[ "$dry_run" == true ]]; then
        log_warn "DRY-RUN MODE ENABLED: No files will be archived."
    fi

    # Determine which apps to process
    local apps_to_backup=()
    if [[ "$target" == "all" ]]; then
        apps_to_backup=("beeper" "zalo" "vesktop" "zen" "obsidian" "fcitx5" "antigravity" "howdy" "kde" "hypr")
    else
        apps_to_backup=("$target")
    fi

    # Process Check
    local running_apps=()
    for app in "${apps_to_backup[@]}"; do
        local pids
        pids="$(get_running_pids "$app")"
        if [[ -n "$pids" ]]; then
            log_warn "Application '$app' is currently running (PID: $pids)."
            running_apps+=("$app")
        fi
    done

    if [[ ${#running_apps[@]} -gt 0 && ( "$force" == false || "$kill_apps" == true ) ]]; then
        if [[ "$dry_run" == true ]]; then
            log_warn "Notice: Running applications detected (${running_apps[*]}). During a live backup, you will be prompted to close/kill them or use -k/--kill."
        elif [[ "$kill_apps" == true ]]; then
            if ! terminate_apps "${running_apps[@]}"; then
                log_error "Could not terminate all running applications. Backup aborted to protect data."
                return 1
            fi
        else
            echo ""
            log_warn "Backing up applications while active can produce inconsistent or corrupted database snapshots (SQLite WAL / LevelDB locks)."
            read_choice "Close/kill active application(s) (${running_apps[*]}) to run backup safely? [Y/n]: " kill_choice
            if [[ -z "$kill_choice" || "$kill_choice" =~ ^[Yy]$ ]]; then
                if ! terminate_apps "${running_apps[@]}"; then
                    log_error "Could not terminate all running applications. Backup aborted to protect data."
                    return 1
                fi
            else
                read_choice "Proceed anyway with applications running (risks inconsistent data)? [y/N]: " proceed_choice
                if [[ ! "$proceed_choice" =~ ^[Yy]$ ]]; then
                    log_info "Backup aborted by user. Please close running applications and retry."
                    VAULT_CANCELED=true
                    return 0
                fi
                log_warn "Proceeding with live backup as requested. Note: open database files might be dirty."
            fi
        fi
    fi

    # Staging setup
    local staging_dir
    staging_dir="$(get_safe_staging_dir "app-vault-sync-backup")"
    VAULT_STAGING_DIR="$staging_dir"

    # Manifest initialization
    local manifest_file="$staging_dir/backup_manifest.json"
    cat <<EOF > "$manifest_file"
{
  "version": "2.0",
  "created_at": "$(date --iso-8601=seconds 2>/dev/null || date)",
  "hostname": "$(hostname 2>/dev/null || echo "unknown")",
  "user": "$(whoami)",
  "target": "$target",
  "include_media": $include_media,
  "apps": {}
}
EOF

    # Common exclusions for browser/electron cache
    local common_excludes=(
        --exclude='Cache'
        --exclude='Code Cache'
        --exclude='GPUCache'
        --exclude='DawnCache'
        --exclude='DawnGraphiteCache'
        --exclude='DawnWebGPUCache'
        --exclude='Crashpad'
        --exclude='blob_storage'
        --exclude='SingletonCookie'
        --exclude='SingletonLock'
        --exclude='SingletonSocket'
        --exclude='*.log'
        --exclude='dumps'
        --exclude='shader-cache'
        --exclude='startupCache'
    )

    # App Backup Logic
    for app in "${apps_to_backup[@]}"; do
        case "$app" in
            beeper)
                local beeper_cfg
                beeper_cfg="$(find_beeper_config)"
                if [[ -z "$beeper_cfg" || ! -d "$beeper_cfg" ]]; then
                    log_warn "BeeperTexts config not found, skipping..."
                    continue
                fi

                log_step "Processing BeeperTexts (~/.config/BeeperTexts)..."
                local beeper_excludes=("${common_excludes[@]}")
                if [[ "$include_media" == false ]]; then
                    beeper_excludes+=(--exclude='media')
                fi

                mkdir -p "$staging_dir/BeeperTexts"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $beeper_cfg -> BeeperTexts"
                else
                    rsync -a "${beeper_excludes[@]}" "$beeper_cfg/" "$staging_dir/BeeperTexts/"
                fi

                local beeper_manifest
                beeper_manifest=$(jq -n \
                    --arg cfg "$beeper_cfg" \
                    --arg folder "BeeperTexts" \
                    --argjson inc_media "$include_media" \
                    '{"included": true, "folder": $folder, "config_path": $cfg, "include_media": $inc_media}')
                jq --argjson bm "$beeper_manifest" '.apps.beeper = $bm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            zalo)
                local zalo_cfg zalo_flags
                zalo_cfg="$(find_zalo_config)"
                zalo_flags="$(find_zalo_flags)"
                if [[ -z "$zalo_cfg" || ! -d "$zalo_cfg" ]]; then
                    log_warn "ZaloData not found, skipping..."
                    continue
                fi

                log_step "Processing ZaloData (~/.config/ZaloData)..."
                local zalo_excludes=(
                    "${common_excludes[@]}"
                    --exclude='resp_cache'
                    --exclude='zcall-wine'
                    --exclude='zcall-wine-validate'
                )
                if [[ "$include_media" == false ]]; then
                    zalo_excludes+=(--exclude='media')
                fi

                mkdir -p "$staging_dir/ZaloData"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $zalo_cfg -> ZaloData"
                else
                    rsync -a "${zalo_excludes[@]}" "$zalo_cfg/" "$staging_dir/ZaloData/"
                fi

                if [[ -n "$zalo_flags" && -f "$zalo_flags" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would sync: $zalo_flags -> zalo-flags.conf"
                    else
                        cp -p "$zalo_flags" "$staging_dir/zalo-flags.conf"
                    fi
                fi

                local zalo_manifest
                zalo_manifest=$(jq -n \
                    --arg cfg "$zalo_cfg" \
                    --arg folder "ZaloData" \
                    --arg flags "${zalo_flags:-}" \
                    --argjson inc_media "$include_media" \
                    '{"included": true, "folder": $folder, "config_path": $cfg, "flags_path": $flags, "include_media": $inc_media}')
                jq --argjson zm "$zalo_manifest" '.apps.zalo = $zm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            vesktop)
                local vesk_cfg
                vesk_cfg="$(find_vesktop_config)"
                if [[ -z "$vesk_cfg" || ! -d "$vesk_cfg" ]]; then
                    log_warn "Vesktop config not found, skipping..."
                    continue
                fi

                log_step "Processing Vesktop (~/.config/vesktop)..."
                local vesk_excludes=("${common_excludes[@]}")

                mkdir -p "$staging_dir/vesktop"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $vesk_cfg -> vesktop"
                else
                    rsync -a "${vesk_excludes[@]}" "$vesk_cfg/" "$staging_dir/vesktop/"
                fi

                local vesk_manifest
                vesk_manifest=$(jq -n \
                    --arg cfg "$vesk_cfg" \
                    --arg folder "vesktop" \
                    '{"included": true, "folder": $folder, "config_path": $cfg}')
                jq --argjson vm "$vesk_manifest" '.apps.vesktop = $vm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            zen)
                local zen_cfg
                zen_cfg="$(find_zen_config)"
                if [[ -z "$zen_cfg" || ! -d "$zen_cfg" ]]; then
                    log_warn "Zen Browser config not found, skipping..."
                    continue
                fi

                log_step "Processing Zen Browser (~/.config/zen)..."
                local zen_excludes=(
                    "${common_excludes[@]}"
                    --exclude='lock'
                    --exclude='.parentlock'
                    --exclude='crashes'
                    --exclude='datareporting'
                    --exclude='shader-cache'
                    --exclude='startupCache'
                    --exclude='storage/temporary'
                )
                if [[ "$include_media" == false ]]; then
                    zen_excludes+=(--exclude='storage/default/https+++*')
                fi

                mkdir -p "$staging_dir/zen"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $zen_cfg -> zen"
                else
                    rsync -a "${zen_excludes[@]}" "$zen_cfg/" "$staging_dir/zen/"
                fi

                local zen_manifest
                zen_manifest=$(jq -n \
                    --arg cfg "$zen_cfg" \
                    --arg folder "zen" \
                    --argjson inc_media "$include_media" \
                    '{"included": true, "folder": $folder, "config_path": $cfg, "include_media": $inc_media}')
                jq --argjson zm "$zen_manifest" '.apps.zen = $zm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            obsidian)
                local obs_cfg
                obs_cfg="$(find_obsidian_config)"
                if [[ -z "$obs_cfg" || ! -d "$obs_cfg" ]]; then
                    log_warn "Obsidian config not found, skipping..."
                    continue
                fi

                log_step "Processing Obsidian (~/.config/obsidian)..."
                local obs_excludes=("${common_excludes[@]}")

                mkdir -p "$staging_dir/obsidian"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $obs_cfg -> obsidian"
                else
                    rsync -a "${obs_excludes[@]}" "$obs_cfg/" "$staging_dir/obsidian/"
                fi

                local obs_manifest
                obs_manifest=$(jq -n \
                    --arg cfg "$obs_cfg" \
                    --arg folder "obsidian" \
                    '{"included": true, "folder": $folder, "config_path": $cfg}')
                jq --argjson om "$obs_manifest" '.apps.obsidian = $om' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            fcitx5)
                local fcitx_cfg
                fcitx_cfg="$(find_fcitx5_config)"
                if [[ -z "$fcitx_cfg" || ! -d "$fcitx_cfg" ]]; then
                    log_warn "Fcitx5 config not found, skipping..."
                    continue
                fi

                log_step "Processing Fcitx5 (~/.config/fcitx5)..."
                local fcitx_excludes=("${common_excludes[@]}")

                mkdir -p "$staging_dir/fcitx5"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $fcitx_cfg -> fcitx5"
                else
                    rsync -a "${fcitx_excludes[@]}" "$fcitx_cfg/" "$staging_dir/fcitx5/"
                fi

                local fcitx_manifest
                fcitx_manifest=$(jq -n \
                    --arg cfg "$fcitx_cfg" \
                    --arg folder "fcitx5" \
                    '{"included": true, "folder": $folder, "config_path": $cfg}')
                jq --argjson fm "$fcitx_manifest" '.apps.fcitx5 = $fm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            antigravity)
                local ag_cfg
                ag_cfg="$(find_antigravity_config)"
                if [[ -z "$ag_cfg" || ! -d "$ag_cfg" ]]; then
                    log_warn "Antigravity config not found, skipping..."
                    continue
                fi

                log_step "Processing Antigravity (~/.config/Antigravity)..."
                local ag_excludes=("${common_excludes[@]}" --exclude='crashes' --exclude='DevToolsActivePort')

                mkdir -p "$staging_dir/Antigravity"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync config: $ag_cfg -> Antigravity"
                else
                    rsync -a "${ag_excludes[@]}" "$ag_cfg/" "$staging_dir/Antigravity/"
                fi

                local ag_manifest
                ag_manifest=$(jq -n \
                    --arg cfg "$ag_cfg" \
                    --arg folder "Antigravity" \
                    '{"included": true, "folder": $folder, "config_path": $cfg}')
                jq --argjson am "$ag_manifest" '.apps.antigravity = $am' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            howdy)
                local h_dir="/etc/howdy/models"
                local h_file="$h_dir/${ACTUAL_USER}.dat"
                local has_howdy=false

                if [[ -d "$h_dir" || -f "$h_file" ]]; then
                    has_howdy=true
                elif [[ "$dry_run" == true ]]; then
                    if pacman -Q howdy-next >/dev/null 2>&1 || pacman -Q howdy-next-bin >/dev/null 2>&1 || command -v howdy >/dev/null 2>&1; then
                        has_howdy=true
                    elif sudo -n test -d "$h_dir" 2>/dev/null || sudo -n test -f "$h_file" 2>/dev/null; then
                        has_howdy=true
                    fi
                elif [[ "$EUID" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
                    if sudo -n test -d "$h_dir" 2>/dev/null || sudo -n test -f "$h_file" 2>/dev/null; then
                        has_howdy=true
                    elif [[ -t 0 ]]; then
                        if sudo test -d "$h_dir" 2>/dev/null || sudo test -f "$h_file" 2>/dev/null; then
                            has_howdy=true
                        fi
                    fi
                fi

                if [[ "$has_howdy" == false ]]; then
                    log_warn "Howdy face model not found (/etc/howdy/models/${ACTUAL_USER}.dat), skipping..."
                    continue
                fi

                log_step "Processing Howdy Face Recognition Model..."
                mkdir -p "$staging_dir/howdy/models"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $h_dir -> howdy/models"
                else
                    # Only this user's model: the archive ends up owned by the user, and other
                    # users' biometric data must not leak into it
                    if [[ "$EUID" -eq 0 ]]; then
                        if [[ -f "$h_file" ]]; then
                            cp -p "$h_file" "$staging_dir/howdy/models/" 2>/dev/null || true
                        fi
                    else
                        sudo cp -p "$h_file" "$staging_dir/howdy/models/" 2>/dev/null || true
                        sudo chown -R "$(id -u):$(id -g)" "$staging_dir/howdy" 2>/dev/null || true
                    fi
                fi

                local h_manifest
                h_manifest=$(jq -n \
                    --arg user "$ACTUAL_USER" \
                    --arg dir "$h_dir" \
                    --arg file "${ACTUAL_USER}.dat" \
                    '{"included": true, "user": $user, "models_dir": $dir, "model_file": $file}')
                jq --argjson hm "$h_manifest" '.apps.howdy = $hm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            kde)
                if ! kde_has_settings; then
                    log_warn "KDE Plasma settings not found (no kdeglobals/kwinrc/plasmashellrc), skipping..."
                    continue
                fi

                log_step "Processing KDE Plasma (curated ~/.config + ~/.local/share)..."
                if pgrep -x plasmashell >/dev/null 2>&1; then
                    log_warn "plasmashell is running. Settings are still copied; log out once after restore so Plasma reloads them cleanly."
                fi

                local kde_cfg kde_share kde_home
                kde_cfg="$(kde_config_root)"
                kde_share="$(kde_share_root)"
                kde_home="${ACTUAL_HOME:-$HOME}"
                mkdir -p "$staging_dir/kde/config" "$staging_dir/kde/share" "$staging_dir/kde/home"
                local dynamic_cfg=()
                mapfile -t dynamic_cfg < <(kde_config_entries)
                kde_copy_named "$kde_cfg" "$staging_dir/kde/config" "$dry_run" "${dynamic_cfg[@]}"
                kde_copy_named "$kde_share" "$staging_dir/kde/share" "$dry_run" "${KDE_SHARE_ENTRIES[@]}"
                kde_copy_named "$kde_home" "$staging_dir/kde/home" "$dry_run" "${KDE_HOME_ENTRIES[@]}"

                local kde_manifest
                kde_manifest=$(jq -n \
                    --arg cfg "$kde_cfg" \
                    --arg share "$kde_share" \
                    --arg folder "kde" \
                    '{"included": true, "folder": $folder, "config_path": $cfg, "share_path": $share, "kind": "plasma"}')
                jq --argjson km "$kde_manifest" '.apps.kde = $km' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;

            hypr)
                local hypr_cfg
                hypr_cfg="$(find_hypr_config)"
                if [[ -z "$hypr_cfg" || ! -d "$hypr_cfg" ]]; then
                    log_warn "Hyprland config directory (~/.config/hypr) not found, skipping..."
                    continue
                fi

                log_step "Processing Hyprland (~/.config/hypr)..."
                local hypr_excludes=("${common_excludes[@]}")

                mkdir -p "$staging_dir/hypr"
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would sync: $hypr_cfg -> hypr"
                else
                    rsync -a "${hypr_excludes[@]}" "$hypr_cfg/" "$staging_dir/hypr/"
                fi

                local hypr_manifest
                hypr_manifest=$(jq -n \
                    --arg cfg "$hypr_cfg" \
                    --arg folder "hypr" \
                    '{"included": true, "folder": $folder, "config_path": $cfg}')
                jq --argjson hm "$hypr_manifest" '.apps.hypr = $hm' "$manifest_file" > "$manifest_file.tmp" && mv "$manifest_file.tmp" "$manifest_file"
                ;;
        esac
    done

    if [[ "$dry_run" == true ]]; then
        cleanup_vault_staging
        echo ""
        log_success "Dry run complete. No files were written."
        return 0
    fi

    log_step "Compressing archive to: $output_file"
    mkdir_user "$(dirname "$output_file")"
    create_zip_archive "$staging_dir" "$output_file"
    local file_uid="${SUDO_UID:-${TARGET_UID:-1000}}"
    local file_gid="${SUDO_GID:-${TARGET_GID:-1000}}"
    if [[ "$file_uid" != "0" ]]; then
        # Only the archive itself: the parent may be any directory (-o /tmp/x.zip), and
        # directories we created were already handed over by mkdir_user
        chown "${file_uid}:${file_gid}" "$output_file" 2>/dev/null || true
    fi

    # Explicit staging cleanup
    cleanup_vault_staging

    local archive_size
    archive_size=$(format_bytes "$output_file")
    echo ""
    log_success "Backup completed successfully!"
    log_info "Archive:    $output_file"
    log_info "Total Size: $archive_size"
}

# --- Import / Restore Command ---
cmd_import() {
    VAULT_CANCELED=false
    require_vault_tools || return 1
    local zip_file=""
    local target_app=""
    local safety_backup=true
    local force=false
    local kill_apps=false
    local dry_run=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --app)
                target_app="$2"
                shift 2
                ;;
            --no-safety-backup)
                safety_backup=false
                shift
                ;;
            -k|--kill)
                kill_apps=true
                shift
                ;;
            -f|--force)
                force=true
                shift
                ;;
            --dry-run)
                dry_run=true
                shift
                ;;
            -h|--help)
                show_import_help
                return 0
                ;;
            *)
                if [[ -z "$zip_file" ]]; then
                    zip_file="$1"
                    shift
                else
                    log_error "Unknown argument: $1"
                    show_import_help
                    return 1
                fi
                ;;
        esac
    done

    if [[ -z "$zip_file" ]]; then
        log_error "Missing zip archive parameter."
        show_import_help
        return 1
    fi

    if [[ ! -f "$zip_file" ]]; then
        log_error "Archive file not found: $zip_file"
        return 1
    fi

    # Direct Howdy .dat standalone restore
    if [[ "$zip_file" =~ \.dat$ ]]; then
        log_step "Detected standalone Howdy face model: ${COLOR_BOLD}$zip_file${COLOR_RESET}"
        if [[ "$dry_run" == true ]]; then
            log_info "  [Dry-run] Would restore face model to /etc/howdy/models/${ACTUAL_USER}.dat"
            return 0
        fi

        if [[ "$force" == false ]]; then
            echo ""
            log_warn "Restoring will install this face model for user '${ACTUAL_USER}' at /etc/howdy/models/${ACTUAL_USER}.dat."
            read_choice "Proceed with face model import? [y/N]: " confirm_dat
            if [[ ! "$confirm_dat" =~ ^[Yy]$ ]]; then
                log_info "Import operation canceled."
                VAULT_CANCELED=true
                return 0
            fi
        fi

        local dest_dir="/etc/howdy/models"
        local dest_file="$dest_dir/${ACTUAL_USER}.dat"
        if [[ "$safety_backup" == true ]]; then
            local has_old=false
            if [[ -f "$dest_file" ]]; then
                has_old=true
            elif [[ "$EUID" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
                sudo test -f "$dest_file" 2>/dev/null && has_old=true
            fi
            if [[ "$has_old" == true ]]; then
                local rollback_dir="${SAFETY_BACKUP_DIR}/safety_rollback_$(date +'%Y-%m-%d_%H%M%S')"
                mkdir_user "$rollback_dir/howdy/models"
                if [[ "$EUID" -eq 0 ]]; then
                    cp -p "$dest_file" "$rollback_dir/howdy/models/" 2>/dev/null || true
                else
                    sudo cp -p "$dest_file" "$rollback_dir/howdy/models/" 2>/dev/null || true
                    sudo chown -R "$(id -u):$(id -g)" "$rollback_dir" 2>/dev/null || true
                fi
                log_info "Safety snapshot of existing model saved to: $rollback_dir"
            fi
        fi

        if [[ "$EUID" -eq 0 ]]; then
            mkdir -p "$dest_dir"
            chmod 700 "$dest_dir"
            chown root:root "$dest_dir"
            cp "$zip_file" "$dest_file"
            chown root:root "$dest_file"
            chmod 600 "$dest_file"
        else
            sudo mkdir -p "$dest_dir"
            sudo chmod 700 "$dest_dir"
            sudo chown root:root "$dest_dir"
            sudo cp "$zip_file" "$dest_file"
            sudo chown root:root "$dest_file"
            sudo chmod 600 "$dest_file"
        fi

        log_success "Imported Howdy face model for '${ACTUAL_USER}' from: ${zip_file}"
        if command -v howdy >/dev/null 2>&1; then
            echo ""
            log_info "Verifying enrolled models with howdy list:"
            if [[ "$EUID" -eq 0 ]]; then
                howdy -U "$ACTUAL_USER" list 2>/dev/null || howdy list 2>/dev/null || true
            else
                sudo howdy -U "$ACTUAL_USER" list 2>/dev/null || sudo howdy list 2>/dev/null || true
            fi
        fi
        return 0
    fi

    log_step "Inspecting archive: ${COLOR_BOLD}$zip_file${COLOR_RESET}"
    local manifest_raw
    manifest_raw="$(read_zip_manifest "$zip_file")"
    if [[ -z "$manifest_raw" ]]; then
        log_error "Archive is missing 'backup_manifest.json' or format is unrecognized."
        return 1
    fi

    local available_apps
    available_apps=$(echo "$manifest_raw" | jq -r '.apps | keys[]' 2>/dev/null || true)
    if [[ -z "$available_apps" ]]; then
        log_warn "No applications found in manifest. Will attempt auto-detection during extraction."
    fi

    local apps_to_restore=()
    if [[ -n "$target_app" ]]; then
        [[ "$target_app" == "beepertexts" ]] && target_app="beeper"
        [[ "$target_app" == "zalodata" ]] && target_app="zalo"
        [[ "$target_app" == "plasma" || "$target_app" == "kde-plasma" ]] && target_app="kde"
        [[ "$target_app" == "hyprland" ]] && target_app="hypr"
        if [[ -n "$available_apps" ]] && ! echo "$available_apps" | grep -qx "$target_app"; then
            log_error "Application '$target_app' is not listed in archive manifest."
            log_info "Available in archive: $(echo "$available_apps" | tr '\n' ' ')"
            return 1
        fi
        apps_to_restore=("$target_app")
    elif [[ -n "$available_apps" ]]; then
        while IFS= read -r app; do
            [[ -n "$app" ]] && apps_to_restore+=("$app")
        done <<< "$available_apps"
    fi

    if [[ ${#apps_to_restore[@]} -gt 0 ]]; then
        log_info "Apps to restore: ${COLOR_BOLD}${apps_to_restore[*]}${COLOR_RESET}"
    fi

    # Verify no target apps are actively running
    local running_apps=()
    for app in "${apps_to_restore[@]}"; do
        local pids
        pids="$(get_running_pids "$app")"
        if [[ -n "$pids" ]]; then
            log_warn "Application '$app' is running (PID: $pids)."
            running_apps+=("$app")
        fi
    done

    if [[ ${#running_apps[@]} -gt 0 ]]; then
        if [[ "$dry_run" == true ]]; then
            log_warn "Notice: Running applications detected (${running_apps[*]}). During a live restore, they will need to be closed."
        elif [[ "$kill_apps" == true ]]; then
            if ! terminate_apps "${running_apps[@]}"; then
                log_error "Could not terminate running apps. Restore aborted."
                return 1
            fi
        elif [[ "$force" == true ]]; then
            log_warn "--force flag used: Proceeding with restoration while apps are running. This is hazardous!"
        else
            echo ""
            log_warn "Target application(s) (${running_apps[*]}) are currently running. Overwriting open files will cause crashes or data corruption."
            read_choice "Close/kill running application(s) (${running_apps[*]}) to proceed with restore? [Y/n]: " kill_choice
            if [[ -z "$kill_choice" || "$kill_choice" =~ ^[Yy]$ ]]; then
                if ! terminate_apps "${running_apps[@]}"; then
                    log_error "Could not terminate running applications. Restore aborted."
                    return 1
                fi
            else
                log_error "Cannot safely restore files while target applications are running. Please close them and re-run."
                return 1
            fi
        fi
    fi

    # Confirmation Prompt
    if [[ "$force" == false && "$dry_run" == false ]]; then
        echo ""
        log_warn "Restoring will OVERWRITE existing application settings, login credentials, and databases!"
        read_choice "Are you sure you want to proceed? [y/N]: " confirm
        if [[ ! "$confirm" =~ ^[Yy]$ ]]; then
            log_info "Restore operation canceled."
            VAULT_CANCELED=true
            return 0
        fi
    fi

    # Extract archive to temporary staging folder
    local staging_dir
    staging_dir="$(get_safe_staging_dir "app-vault-sync-restore")"
    VAULT_STAGING_DIR="$staging_dir"

    log_step "Extracting archive to temporary workspace..."
    extract_zip_archive "$zip_file" "$staging_dir"

    # If no apps were found in manifest, auto-detect from extracted folders
    if [[ ${#apps_to_restore[@]} -eq 0 ]]; then
        [[ -d "$staging_dir/BeeperTexts" || -d "$staging_dir/beeper" ]] && apps_to_restore+=("beeper")
        [[ -d "$staging_dir/ZaloData" || -d "$staging_dir/zalo" ]] && apps_to_restore+=("zalo")
        [[ -d "$staging_dir/vesktop" ]] && apps_to_restore+=("vesktop")
        [[ -d "$staging_dir/zen" ]] && apps_to_restore+=("zen")
        [[ -d "$staging_dir/obsidian" ]] && apps_to_restore+=("obsidian")
        [[ -d "$staging_dir/fcitx5" ]] && apps_to_restore+=("fcitx5")
        [[ -d "$staging_dir/Antigravity" || -d "$staging_dir/antigravity" ]] && apps_to_restore+=("antigravity")
        [[ -d "$staging_dir/howdy" ]] && apps_to_restore+=("howdy")
        [[ -d "$staging_dir/kde" ]] && apps_to_restore+=("kde")
        [[ -d "$staging_dir/hypr" ]] && apps_to_restore+=("hypr")
        log_info "Auto-detected apps in archive: ${COLOR_BOLD}${apps_to_restore[*]}${COLOR_RESET}"
    fi

    # Rollback snapshot directory
    local rollback_dir="${SAFETY_BACKUP_DIR}/safety_rollback_$(date +'%Y-%m-%d_%H%M%S')"
    local config_base="$VAULT_CONFIG_HOME"
    if [[ "$dry_run" == false ]]; then
        mkdir_user "$config_base"
        if [[ "$safety_backup" == true ]]; then
            mkdir_user "$SAFETY_BACKUP_DIR"
        fi
    fi

    for app in "${apps_to_restore[@]}"; do
        log_step "Restoring: ${COLOR_BOLD}$app${COLOR_RESET}"
        case "$app" in
            beeper)
                local src=""
                if [[ -d "$staging_dir/BeeperTexts" ]]; then
                    src="$staging_dir/BeeperTexts"
                elif [[ -d "$staging_dir/beeper/config" ]]; then
                    src="$staging_dir/beeper/config"
                elif [[ -d "$staging_dir/beeper" ]]; then
                    src="$staging_dir/beeper"
                fi

                local dest_beeper="${config_base}/BeeperTexts"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore BeeperTexts data to: $dest_beeper"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_beeper" ]]; then
                            mkdir -p "$rollback_dir/BeeperTexts"
                            rsync -a "$dest_beeper/" "$rollback_dir/BeeperTexts/"
                        fi
                        mkdir -p "$dest_beeper"
                        rsync -a --chmod=go-w "$src/" "$dest_beeper/"
                        chmod 700 "$dest_beeper"
                    fi
                fi
                ;;

            zalo)
                local src=""
                if [[ -d "$staging_dir/ZaloData" ]]; then
                    src="$staging_dir/ZaloData"
                elif [[ -d "$staging_dir/zalo/config" ]]; then
                    src="$staging_dir/zalo/config"
                elif [[ -d "$staging_dir/zalo" ]]; then
                    src="$staging_dir/zalo"
                fi

                local dest_zalo="${config_base}/ZaloData"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore ZaloData to: $dest_zalo"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_zalo" ]]; then
                            mkdir -p "$rollback_dir/ZaloData"
                            rsync -a "$dest_zalo/" "$rollback_dir/ZaloData/"
                        fi
                        mkdir -p "$dest_zalo"
                        rsync -a --chmod=go-w "$src/" "$dest_zalo/"
                        chmod 700 "$dest_zalo"

                        if [[ -f "$staging_dir/zalo-flags.conf" ]]; then
                            cp -p "$staging_dir/zalo-flags.conf" "${config_base}/zalo-flags.conf"
                        elif [[ -f "$staging_dir/zalo/extra/zalo-flags.conf" ]]; then
                            cp -p "$staging_dir/zalo/extra/zalo-flags.conf" "${config_base}/zalo-flags.conf"
                        fi
                    fi
                fi
                ;;

            vesktop)
                local src=""
                if [[ -d "$staging_dir/vesktop" ]]; then
                    src="$staging_dir/vesktop"
                elif [[ -d "$staging_dir/vesktop/config" ]]; then
                    src="$staging_dir/vesktop/config"
                fi

                local dest_vesk="${config_base}/vesktop"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Vesktop data to: $dest_vesk"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_vesk" ]]; then
                            mkdir -p "$rollback_dir/vesktop"
                            rsync -a "$dest_vesk/" "$rollback_dir/vesktop/"
                        fi
                        mkdir -p "$dest_vesk"
                        rsync -a --chmod=go-w "$src/" "$dest_vesk/"
                        chmod 700 "$dest_vesk"
                    fi
                fi
                ;;

            zen)
                local src=""
                if [[ -d "$staging_dir/zen" ]]; then
                    src="$staging_dir/zen"
                elif [[ -d "$staging_dir/zen/config" ]]; then
                    src="$staging_dir/zen/config"
                fi

                local dest_zen="${config_base}/zen"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Zen Browser data to: $dest_zen"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_zen" ]]; then
                            mkdir -p "$rollback_dir/zen"
                            rsync -a "$dest_zen/" "$rollback_dir/zen/"
                        fi
                        mkdir -p "$dest_zen"
                        rsync -a --chmod=go-w "$src/" "$dest_zen/"
                        chmod 700 "$dest_zen"
                    fi
                fi
                ;;

            obsidian)
                local src=""
                if [[ -d "$staging_dir/obsidian" ]]; then
                    src="$staging_dir/obsidian"
                elif [[ -d "$staging_dir/obsidian/config" ]]; then
                    src="$staging_dir/obsidian/config"
                fi

                local dest_obs="${config_base}/obsidian"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Obsidian config to: $dest_obs"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_obs" ]]; then
                            mkdir -p "$rollback_dir/obsidian"
                            rsync -a "$dest_obs/" "$rollback_dir/obsidian/"
                        fi
                        mkdir -p "$dest_obs"
                        rsync -a --chmod=go-w "$src/" "$dest_obs/"
                        chmod 700 "$dest_obs"
                    fi
                fi
                ;;

            fcitx5)
                local src=""
                if [[ -d "$staging_dir/fcitx5" ]]; then
                    src="$staging_dir/fcitx5"
                elif [[ -d "$staging_dir/fcitx5/config" ]]; then
                    src="$staging_dir/fcitx5/config"
                fi

                local dest_fcitx="${config_base}/fcitx5"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Fcitx5 config to: $dest_fcitx"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_fcitx" ]]; then
                            mkdir -p "$rollback_dir/fcitx5"
                            rsync -a "$dest_fcitx/" "$rollback_dir/fcitx5/"
                        fi
                        mkdir -p "$dest_fcitx"
                        rsync -a --chmod=go-w "$src/" "$dest_fcitx/"
                        chmod 700 "$dest_fcitx"
                    fi
                fi
                ;;

            antigravity)
                local src=""
                if [[ -d "$staging_dir/Antigravity" ]]; then
                    src="$staging_dir/Antigravity"
                elif [[ -d "$staging_dir/antigravity/config" ]]; then
                    src="$staging_dir/antigravity/config"
                elif [[ -d "$staging_dir/antigravity" ]]; then
                    src="$staging_dir/antigravity"
                fi

                local dest_ag="${config_base}/Antigravity"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Antigravity config to: $dest_ag"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_ag" ]]; then
                            mkdir -p "$rollback_dir/Antigravity"
                            rsync -a "$dest_ag/" "$rollback_dir/Antigravity/"
                        fi
                        mkdir -p "$dest_ag"
                        rsync -a --chmod=go-w "$src/" "$dest_ag/"
                        chmod 700 "$dest_ag"
                    fi
                fi
                ;;

            howdy)
                local dest_howdy_dir="/etc/howdy/models"
                local howdy_user
                howdy_user="$(echo "$manifest_raw" | jq -r '.apps.howdy.user // empty' 2>/dev/null || true)"
                [[ -z "$howdy_user" ]] && howdy_user="$ACTUAL_USER"

                if [[ -d "$staging_dir/howdy/models" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Howdy face models to: $dest_howdy_dir"
                    else
                        if [[ "$safety_backup" == true ]]; then
                            local has_models=false
                            if [[ -d "$dest_howdy_dir" ]]; then has_models=true; fi
                            if [[ "$has_models" == true ]]; then
                                mkdir -p "$rollback_dir/howdy/models"
                                if [[ "$EUID" -eq 0 ]]; then
                                    cp -p "$dest_howdy_dir"/*.dat "$rollback_dir/howdy/models/" 2>/dev/null || true
                                else
                                    sudo cp -p "$dest_howdy_dir"/*.dat "$rollback_dir/howdy/models/" 2>/dev/null || true
                                fi
                            fi
                        fi

                        if [[ "$EUID" -eq 0 ]]; then
                            mkdir -p "$dest_howdy_dir"
                            chmod 700 "$dest_howdy_dir"
                            chown root:root "$dest_howdy_dir"

                            cp -p "$staging_dir/howdy/models/"*.dat "$dest_howdy_dir/" 2>/dev/null || true
                            if [[ -f "$staging_dir/howdy/models/${howdy_user}.dat" && ! -f "$dest_howdy_dir/${ACTUAL_USER}.dat" ]]; then
                                cp -p "$staging_dir/howdy/models/${howdy_user}.dat" "$dest_howdy_dir/${ACTUAL_USER}.dat" 2>/dev/null || true
                            fi
                            chown root:root "$dest_howdy_dir"/*.dat 2>/dev/null || true
                            chmod 600 "$dest_howdy_dir"/*.dat 2>/dev/null || true
                        else
                            sudo mkdir -p "$dest_howdy_dir"
                            sudo chmod 700 "$dest_howdy_dir"
                            sudo chown root:root "$dest_howdy_dir"

                            sudo cp -p "$staging_dir/howdy/models/"*.dat "$dest_howdy_dir/" 2>/dev/null || true
                            if [[ -f "$staging_dir/howdy/models/${howdy_user}.dat" ]]; then
                                sudo cp -p "$staging_dir/howdy/models/${howdy_user}.dat" "$dest_howdy_dir/${ACTUAL_USER}.dat" 2>/dev/null || true
                            fi
                            sudo chown root:root "$dest_howdy_dir"/*.dat 2>/dev/null || true
                            sudo chmod 600 "$dest_howdy_dir"/*.dat 2>/dev/null || true
                        fi

                        log_success "Howdy face models restored with permissions 0600 (root:root)."
                        if command -v howdy >/dev/null 2>&1; then
                            echo ""
                            log_info "Verifying enrolled models with howdy list:"
                            if [[ "$EUID" -eq 0 ]]; then
                                howdy -U "$ACTUAL_USER" list 2>/dev/null || howdy list 2>/dev/null || true
                            else
                                sudo howdy -U "$ACTUAL_USER" list 2>/dev/null || sudo howdy list 2>/dev/null || true
                            fi
                        fi
                    fi
                fi
                ;;

            kde)
                local dest_cfg dest_share dest_home
                dest_cfg="$VAULT_CONFIG_HOME"
                dest_share="$VAULT_DATA_HOME"
                dest_home="${ACTUAL_HOME:-$HOME}"
                if [[ ! -d "$staging_dir/kde" ]]; then
                    log_warn "KDE snapshot folder missing in archive, skipping..."
                    continue
                fi
                if [[ "$dry_run" == true ]]; then
                    log_info "  [Dry-run] Would restore KDE config to: $dest_cfg"
                    log_info "  [Dry-run] Would restore KDE share data to: $dest_share"
                    log_info "  [Dry-run] Would restore ~/.fonts ~/.themes ~/.icons if present"
                else
                    if [[ "$safety_backup" == true ]]; then
                        mkdir -p "$rollback_dir/kde/config" "$rollback_dir/kde/share" "$rollback_dir/kde/home"
                        local dynamic_cfg=()
                        mapfile -t dynamic_cfg < <(kde_config_entries)
                        kde_copy_named "$dest_cfg" "$rollback_dir/kde/config" false "${dynamic_cfg[@]}"
                        kde_copy_named "$dest_share" "$rollback_dir/kde/share" false "${KDE_SHARE_ENTRIES[@]}"
                        kde_copy_named "$dest_home" "$rollback_dir/kde/home" false "${KDE_HOME_ENTRIES[@]}"
                    fi

                    # Gracefully stop plasmashell if running so it commits its memory and does NOT overwrite restored files on logout
                    local plasma_was_running=false
                    if pgrep -u "$ACTUAL_USER" -x plasmashell >/dev/null 2>&1; then
                        plasma_was_running=true
                        log_info "Stopping plasmashell to prevent in-memory state from overwriting restored settings on logout..."
                        if [[ "$EUID" -eq 0 ]]; then
                            run_as_user systemctl --user stop plasma-plasmashell.service 2>/dev/null || run_as_user kquitapp6 plasmashell 2>/dev/null || true
                        else
                            systemctl --user stop plasma-plasmashell.service 2>/dev/null || kquitapp6 plasmashell 2>/dev/null || true
                        fi
                        sleep 1
                    fi

                    if [[ -d "$staging_dir/kde/config" ]]; then
                        mkdir_user "$dest_cfg"
                        rsync -a --chmod=go-w "$staging_dir/kde/config/" "$dest_cfg/"
                    fi
                    if [[ -d "$staging_dir/kde/share" ]]; then
                        mkdir_user "$dest_share"
                        rsync -a --chmod=go-w "$staging_dir/kde/share/" "$dest_share/"
                    fi
                    if [[ -d "$staging_dir/kde/home" ]]; then
                        rsync -a --chmod=go-w "$staging_dir/kde/home/" "$dest_home/"
                    fi

                    # Restart plasmashell and reconfigure KWin if it was running
                    if [[ "$plasma_was_running" == true ]]; then
                        log_info "Restarting plasmashell and reloading KWin settings..."
                        if [[ "$EUID" -eq 0 ]]; then
                            run_as_user systemctl --user start plasma-plasmashell.service 2>/dev/null || run_as_user kstart plasmashell >/dev/null 2>&1 &
                            run_as_user qdbus6 org.kde.KWin /KWin reconfigure 2>/dev/null || true
                        else
                            systemctl --user start plasma-plasmashell.service 2>/dev/null || kstart plasmashell >/dev/null 2>&1 &
                            qdbus6 org.kde.KWin /KWin reconfigure 2>/dev/null || true
                        fi
                    fi

                    log_success "KDE Plasma settings restored successfully!"
                    log_info "Note: For a complete refresh of all panel widgets and icons, logging out and back in is recommended."
                fi
                ;;

            hypr)
                local src=""
                if [[ -d "$staging_dir/hypr" ]]; then
                    src="$staging_dir/hypr"
                elif [[ -d "$staging_dir/hypr/config" ]]; then
                    src="$staging_dir/hypr/config"
                fi

                local dest_hypr="${config_base}/hypr"
                if [[ -n "$src" && -d "$src" ]]; then
                    if [[ "$dry_run" == true ]]; then
                        log_info "  [Dry-run] Would restore Hyprland config to: $dest_hypr"
                    else
                        if [[ "$safety_backup" == true && -d "$dest_hypr" ]]; then
                            mkdir -p "$rollback_dir/hypr"
                            rsync -a "$dest_hypr/" "$rollback_dir/hypr/"
                        fi
                        mkdir -p "$dest_hypr"
                        rsync -a --chmod=go-w "$src/" "$dest_hypr/"
                        chmod 700 "$dest_hypr"

                        if command -v hyprctl &>/dev/null && pgrep -x Hyprland >/dev/null 2>&1; then
                            log_info "Hyprland is running; triggering reload via hyprctl reload..."
                            if [[ "$EUID" -eq 0 ]]; then
                                run_as_user hyprctl reload 2>/dev/null || true
                            else
                                hyprctl reload 2>/dev/null || true
                            fi
                        fi
                        log_success "Hyprland configuration restored successfully!"
                    fi
                fi
                ;;
        esac
    done

    if [[ "$dry_run" == true ]]; then
        cleanup_vault_staging
        echo ""
        log_success "Dry run complete. No modifications were made."
        return 0
    fi

    local restore_uid="${SUDO_UID:-}"
    local restore_gid="${SUDO_GID:-}"
    if [[ -z "$restore_uid" && -n "${TARGET_UID:-}" && "$TARGET_UID" != "0" ]]; then
        restore_uid="$TARGET_UID"
        restore_gid="${TARGET_GID:-1000}"
    fi
    if [[ -z "$restore_uid" || "$restore_uid" == "0" ]]; then
        if [[ "$ACTUAL_USER" != "root" ]]; then
            restore_uid="$(id -u "$ACTUAL_USER" 2>/dev/null || true)"
            restore_gid="$(id -g "$ACTUAL_USER" 2>/dev/null || true)"
        fi
    fi

    if [[ -n "$restore_uid" && "$restore_uid" != "0" ]]; then
        for app in "${apps_to_restore[@]}"; do
            case "$app" in
                beeper) [[ -d "${config_base}/BeeperTexts" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/BeeperTexts" 2>/dev/null || true ;;
                zalo)
                    [[ -d "${config_base}/ZaloData" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/ZaloData" 2>/dev/null || true
                    [[ -f "${config_base}/zalo-flags.conf" ]] && chown "${restore_uid}:${restore_gid}" "${config_base}/zalo-flags.conf" 2>/dev/null || true
                    ;;
                vesktop) [[ -d "${config_base}/vesktop" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/vesktop" 2>/dev/null || true ;;
                zen) [[ -d "${config_base}/zen" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/zen" 2>/dev/null || true ;;
                obsidian) [[ -d "${config_base}/obsidian" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/obsidian" 2>/dev/null || true ;;
                fcitx5) [[ -d "${config_base}/fcitx5" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/fcitx5" 2>/dev/null || true ;;
                antigravity) [[ -d "${config_base}/Antigravity" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/Antigravity" 2>/dev/null || true ;;
                hypr) [[ -d "${config_base}/hypr" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/hypr" 2>/dev/null || true ;;
                kde)
                    local share_base="$VAULT_DATA_HOME"
                    local e
                    while IFS= read -r e; do
                        [[ -n "$e" && -e "${config_base}/$e" ]] && chown -R "${restore_uid}:${restore_gid}" "${config_base}/$e" 2>/dev/null || true
                    done < <(kde_config_entries)
                    for e in "${KDE_SHARE_ENTRIES[@]}"; do
                        [[ -e "${share_base}/$e" ]] && chown -R "${restore_uid}:${restore_gid}" "${share_base}/$e" 2>/dev/null || true
                    done
                    for e in "${KDE_HOME_ENTRIES[@]}"; do
                        [[ -e "${ACTUAL_HOME}/$e" ]] && chown -R "${restore_uid}:${restore_gid}" "${ACTUAL_HOME}/$e" 2>/dev/null || true
                    done
                    ;;
            esac
        done
        if [[ -d "$SAFETY_BACKUP_DIR" ]]; then
            chown -R "${restore_uid}:${restore_gid}" "$SAFETY_BACKUP_DIR" 2>/dev/null || true
            # The models snapshot can contain every user's face model; keep it root-only
            if [[ -d "$rollback_dir/howdy" ]]; then
                chown -R root:root "$rollback_dir/howdy" 2>/dev/null || true
                chmod -R go-rwx "$rollback_dir/howdy" 2>/dev/null || true
            fi
        fi
    fi

    # Explicit staging cleanup
    cleanup_vault_staging

    echo ""
    log_success "Restore process finished successfully!"
    if [[ "$safety_backup" == true && -d "$rollback_dir" ]]; then
        log_info "Safety rollback snapshot created at: $rollback_dir"
    fi
}

# --- Info Command ---
cmd_info() {
    require_vault_tools || return 1
    local zip_file="${1:-}"
    if [[ -z "$zip_file" ]]; then
        log_error "Please specify a zip archive to inspect."
        echo "Usage: $0 info <archive.zip>"
        return 1
    fi
    if [[ ! -f "$zip_file" ]]; then
        log_error "Archive file not found: $zip_file"
        return 1
    fi

    local manifest_raw
    manifest_raw="$(read_zip_manifest "$zip_file")"
    if [[ -z "$manifest_raw" ]]; then
        log_error "No 'backup_manifest.json' found inside $zip_file"
        return 1
    fi

    echo -e "${COLOR_BOLD}=== Backup Archive Information ===${COLOR_RESET}"
    echo -e "  Archive File:     $zip_file ($(format_bytes "$zip_file"))"
    echo -e "  Created At:       $(echo "$manifest_raw" | jq -r '.created_at // "Unknown"')"
    echo -e "  Source Host:      $(echo "$manifest_raw" | jq -r '.hostname // "Unknown"') ($(echo "$manifest_raw" | jq -r '.user // "Unknown"'))"
    echo -e "  Backup Target:    $(echo "$manifest_raw" | jq -r '.target // "Unknown"')"
    echo -e "  Media Included:   $(echo "$manifest_raw" | jq -r 'if has("include_media") then .include_media else "Unknown" end')"
    echo ""
    echo -e "${COLOR_BOLD}Included Applications & Folders:${COLOR_RESET}"
    echo "$manifest_raw" | jq -r '.apps | to_entries[] | "  - \(.key | ascii_upcase): \(.value)"' 2>/dev/null | while read -r line; do
        local app_name
        app_name=$(echo "$line" | cut -d: -f1 | tr -d ' -')
        echo -e "  ${COLOR_CYAN}• $app_name${COLOR_RESET}"
        case "$app_name" in
            BEEPER)
                local bcfg bfold
                bcfg=$(echo "$manifest_raw" | jq -r '.apps.beeper.config_path // empty')
                bfold=$(echo "$manifest_raw" | jq -r '.apps.beeper.folder // "BeeperTexts"')
                echo -e "      Folder:      ~/.config/$bfold"
                [[ -n "$bcfg" ]] && echo -e "      Source:      $bcfg"
                ;;
            ZALO)
                local zcfg zfold
                zcfg=$(echo "$manifest_raw" | jq -r '.apps.zalo.config_path // empty')
                zfold=$(echo "$manifest_raw" | jq -r '.apps.zalo.folder // "ZaloData"')
                echo -e "      Folder:      ~/.config/$zfold"
                [[ -n "$zcfg" ]] && echo -e "      Source:      $zcfg"
                ;;
            VESKTOP)
                local vcfg
                vcfg=$(echo "$manifest_raw" | jq -r '.apps.vesktop.config_path // empty')
                echo -e "      Folder:      ~/.config/vesktop"
                [[ -n "$vcfg" ]] && echo -e "      Source:      $vcfg"
                ;;
            ZEN)
                local zencfg
                zencfg=$(echo "$manifest_raw" | jq -r '.apps.zen.config_path // empty')
                echo -e "      Folder:      ~/.config/zen"
                [[ -n "$zencfg" ]] && echo -e "      Source:      $zencfg"
                ;;
            OBSIDIAN)
                local obscfg
                obscfg=$(echo "$manifest_raw" | jq -r '.apps.obsidian.config_path // empty')
                echo -e "      Folder:      ~/.config/obsidian"
                [[ -n "$obscfg" ]] && echo -e "      Source:      $obscfg"
                ;;
            FCITX5)
                local fcfg
                fcfg=$(echo "$manifest_raw" | jq -r '.apps.fcitx5.config_path // empty')
                echo -e "      Folder:      ~/.config/fcitx5"
                [[ -n "$fcfg" ]] && echo -e "      Source:      $fcfg"
                ;;
            ANTIGRAVITY)
                local agcfg agfold
                agcfg=$(echo "$manifest_raw" | jq -r '.apps.antigravity.config_path // empty')
                agfold=$(echo "$manifest_raw" | jq -r '.apps.antigravity.folder // "Antigravity"')
                echo -e "      Folder:      ~/.config/$agfold"
                [[ -n "$agcfg" ]] && echo -e "      Source:      $agcfg"
                ;;
            HOWDY)
                local huser hfile
                huser=$(echo "$manifest_raw" | jq -r '.apps.howdy.user // empty')
                hfile=$(echo "$manifest_raw" | jq -r '.apps.howdy.model_file // empty')
                echo -e "      Models Dir:  /etc/howdy/models"
                echo -e "      User Model:  ${hfile:-${huser}.dat}"
                ;;
            KDE)
                local kcfg kshare
                kcfg=$(echo "$manifest_raw" | jq -r '.apps.kde.config_path // empty')
                kshare=$(echo "$manifest_raw" | jq -r '.apps.kde.share_path // empty')
                echo -e "      Config:      ~/.config (kdeglobals, kwinrc, Plasma applets, shortcuts)"
                echo -e "      Share:       ~/.local/share (plasma, kwin, icons, wallpapers)"
                [[ -n "$kcfg" ]] && echo -e "      Source cfg:  $kcfg"
                [[ -n "$kshare" ]] && echo -e "      Source data: $kshare"
                ;;
            HYPR)
                local hpcfg
                hpcfg=$(echo "$manifest_raw" | jq -r '.apps.hypr.config_path // empty')
                echo -e "      Folder:      ~/.config/hypr"
                [[ -n "$hpcfg" ]] && echo -e "      Source:      $hpcfg"
                ;;
        esac
    done
}

# --- Help Screens ---
show_backup_help() {
    cat <<EOF
Usage: $0 backup [TARGET] [OPTIONS]
       $0 export [TARGET] [OPTIONS]

Targets:
  all               Backup all detected applications & Howdy face model (default)
  beeper            Backup Beeper (~/.config/BeeperTexts)
  zalo              Backup Zalo (~/.config/ZaloData)
  vesktop           Backup Vesktop (~/.config/vesktop)
  zen               Backup Zen Browser (~/.config/zen)
  obsidian          Backup Obsidian (~/.config/obsidian)
  fcitx5            Backup Fcitx5 (~/.config/fcitx5)
  antigravity       Backup Antigravity (~/.config/Antigravity)
  howdy             Backup Howdy face recognition model (/etc/howdy/models)
  kde               Backup KDE Plasma settings (~/.config + ~/.local/share, curated)
  hypr              Backup Hyprland settings (~/.config/hypr)

Options:
  -o, --output <file.zip|.dat> Destination archive (.zip) or face model (.dat) file
  --no-media                Exclude large media folders (ZaloData/media, BeeperTexts/media, Zen web storage)
  -k, --kill                Automatically close/kill active target applications to prevent dirty snapshots
  -f, --force               Bypass confirmation prompts and proceed even if apps are running
  --dry-run                 Show actions without copying files or creating an archive
  -h, --help                Show this help screen
EOF
}

show_import_help() {
    cat <<EOF
Usage: $0 import <archive.zip | model.dat> [OPTIONS]
       $0 restore <archive.zip | model.dat> [OPTIONS]

Arguments:
  <archive.zip|model.dat>   The backup zip file or standalone face model (.dat) to restore

Options:
  --app <app>               Only restore a specific app (beeper, zalo, vesktop, zen, obsidian, fcitx5, antigravity, howdy, kde, hypr)
  --no-safety-backup        Skip creating a safety snapshot of existing files before restoring
  -k, --kill                Automatically close/kill active target applications before restoring
  -f, --force               Bypass confirmation prompts and process running checks
  --dry-run                 Show actions without making changes
  -h, --help                Show this help screen
EOF
}

show_general_help() {
    cat <<EOF
${COLOR_BOLD}app-vault-sync.sh${COLOR_RESET} - Backup, export, and import tool for BeeperTexts, ZaloData, Vesktop, Zen Browser, Obsidian, Fcitx5, Antigravity, Howdy Face Model, KDE Plasma, and Hyprland settings.

${COLOR_BOLD}COMMANDS:${COLOR_RESET}
  ${COLOR_CYAN}status${COLOR_RESET}                            Show detected app paths, Howdy status, and process state
  ${COLOR_CYAN}backup | export [target] [flags]${COLOR_RESET}  Create a .zip backup or .dat face model export
  ${COLOR_CYAN}import | restore <file> [flags]${COLOR_RESET}   Restore applications from an existing backup zip or .dat face model
  ${COLOR_CYAN}info <file.zip>${COLOR_RESET}                   Inspect an archive and view embedded manifest metadata
  ${COLOR_CYAN}help${COLOR_RESET}                              Display this message

${COLOR_BOLD}EXAMPLES:${COLOR_RESET}
  $0 status
  $0 backup all
  $0 backup howdy
  $0 backup howdy -o ~/my_face_backup.dat
  $0 backup all --no-media -o ~/all-apps-compact.zip
  $0 backup zen --no-media
  $0 backup fcitx5
  $0 backup kde
  $0 backup hypr
  $0 info ~/all-apps-compact.zip
  $0 import ~/all-apps-compact.zip --app howdy
  $0 import ~/all-apps-compact.zip --app kde
  $0 import ~/all-apps-compact.zip --app hypr
  $0 import ~/my_face_backup.dat
  $0 restore ~/all-apps-compact.zip --dry-run
EOF
}


# ------------------------------------------------------------------------------
# State 6: Import data (App Vault Restore & Face Model)
# ------------------------------------------------------------------------------
do_import_data() {
    local is_auto=${1:-false}
    local o_start=${2:-0}
    local o_step=${3:-5}
    local o_total=${4:-6}

    local s_start; s_start=$(date +%s)
    local title="Import Data"

    log "INFO" "Starting App Vault & Howdy Face Model import..."

    local candidate_dirs=(
        "/media/data/backups"
        "/media/data"
        "${TARGET_HOME}/.local/share/app-vault-sync/backups"
        "${TARGET_HOME}/Downloads"
    )

    local found_zips=()
    local found_models=()
    for d in "${candidate_dirs[@]}"; do
        if [ -d "$d" ]; then
            while IFS= read -r z; do
                [ -f "$z" ] && found_zips+=("$z")
            done < <(find "$d" -maxdepth 2 -type f \( -name "*backup*.zip" -o -name "all_*.zip" -o -name "*vault*.zip" -o -name "*kde*.zip" -o -name "*plasma*.zip" -o -name "*hypr*.zip" \) 2>/dev/null || true)

            while IFS= read -r m; do
                [ -f "$m" ] && found_models+=("$m")
            done < <(find "$d" -maxdepth 2 -type f \( -name "*face*.dat" -o -name "*howdy*.dat" -o -name "${TARGET_USER}.dat" \) 2>/dev/null || true)
        fi
    done

    local all_candidates=("${found_zips[@]}" "${found_models[@]}")
    local target_item=""
    local import_rc=0

    if [ "$is_auto" = true ]; then
        if [ ${#found_zips[@]} -gt 0 ]; then
            target_item="${found_zips[0]}"
            exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                               1 2 "$title" "Restoring: $(basename "$target_item")" "$s_start" \
                               "cmd_import $(printf '%q' "$target_item") -f -k" || import_rc=$?
        elif [ ${#found_models[@]} -gt 0 ]; then
            target_item="${found_models[0]}"
            exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                               1 2 "$title" "Restoring face model: $(basename "$target_item")" "$s_start" \
                               "cmd_import $(printf '%q' "$target_item") -f" || import_rc=$?
        else
            log "INFO" "No backup archive or face model found in default paths. Skipping import in auto mode."
            render_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" 2 2 "$title" "No backup files found" "$s_start" "Skipped (no archive or model specified)" "✔"
            sleep 1
            return 0
        fi

        # If an archive was restored but Howdy face model is still missing, and a standalone model file exists, restore it as well
        if [ ! -f "/etc/howdy/models/${TARGET_USER}.dat" ] && [ ${#found_models[@]} -gt 0 ]; then
            log "INFO" "Restoring standalone face model: ${found_models[0]}"
            exec_monitored_cmd "$is_auto" "$o_step" "$o_total" "$title" "$o_start" \
                               2 2 "$title" "Restoring face model: $(basename "${found_models[0]}")" "$s_start" \
                               "cmd_import $(printf '%q' "${found_models[0]}") -f" || import_rc=$?
        fi

        if (( import_rc != 0 )); then
            log "ERROR" "Data import finished with errors (exit code $import_rc). See $LOG_FILE"
            add_issue "Data import failed (exit code $import_rc)"
            render_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" 2 2 "$title" "Restore Failed" "$s_start" "Import failed (exit code $import_rc); see $LOG_FILE" "✘"
        else
            render_dual_dashboard "$o_step" "$o_total" "$title" "$o_start" 2 2 "$title" "Restore Finished" "$s_start" "Application data, desktop settings & face model restored." "✔"
        fi
        sleep 1
        return 0
    else
        restore_terminal_interactive
        clear
        echo -e "${C_CYAN}${C_BOLD}=== Import / Restore Application Data, Desktop Settings & Face Models ===${C_RESET}\n"
        if [ ${#all_candidates[@]} -gt 0 ]; then
            echo -e "${C_BOLD}Found backup archives & face models:${C_RESET}"
            local idx=1
            for item in "${all_candidates[@]}"; do
                local sz; sz=$(du -sh "$item" 2>/dev/null | cut -f1)
                local tag="[Archive]"
                if [[ "$item" =~ \.dat$ ]]; then
                    tag="[Howdy Face Model]"
                elif [[ "$item" =~ kde|plasma ]]; then
                    tag="[KDE Plasma Backup]"
                elif [[ "$item" =~ hypr ]]; then
                    tag="[Hyprland Backup]"
                elif [[ "$item" =~ all ]]; then
                    tag="[Full System Backup]"
                fi
                echo -e "  [$idx] $item ($sz) ${C_GRAY}${tag}${C_RESET}"
                ((idx++)) || true
            done
            echo -e "  [c] Enter custom path (.zip or .dat)"
            echo -e "  [q] Cancel and return to menu"
            echo ""
            read_choice "Select file [1-${#all_candidates[@]}, c, q]: " choice
            if [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] && [ "$choice" -le ${#all_candidates[@]} ]; then
                target_item="${all_candidates[$((choice - 1))]}"
            elif [ "$choice" = "c" ] || [ "$choice" = "C" ]; then
                read_choice "Enter full path to backup .zip or .dat: " custom_path
                target_item="$custom_path"
            else
                echo "Import canceled."
                sleep 1
                return 0
            fi
        else
            read_choice "Enter full path to backup .zip archive or .dat face model (or 'q' to cancel): " custom_path
            if [ "$custom_path" = "q" ] || [ "$custom_path" = "Q" ] || [ -z "$custom_path" ]; then
                echo "Import canceled."
                sleep 1
                return 0
            fi
            target_item="$custom_path"
        fi
    fi

    if [ ! -f "$target_item" ]; then
        log "ERROR" "File not found: $target_item"
        echo "File not found: $target_item"
        sleep 2
        return 1
    fi

    log "INFO" "Executing restore directly for file $target_item..."
    restore_terminal_interactive
    if [[ "$target_item" =~ \.dat$ ]]; then
        echo -e "\n${C_BLUE}[INFO]${C_RESET} Standalone Howdy Face Model: $(basename "$target_item")"
        echo -e "  Target User: ${C_BOLD}${TARGET_USER}${C_RESET}"
        echo -e "  Destination: /etc/howdy/models/${TARGET_USER}.dat"
        echo ""
        read_choice "Proceed with face model restoration? (y/N): " confirm
        if [[ "$confirm" =~ ^[Yy]$ ]]; then
            echo -e "${C_CYAN}==>${C_RESET} Restoring face model as root for ${C_BOLD}${TARGET_USER}${C_RESET}..."
            cmd_import "$target_item" -f
            if [ "$VAULT_CANCELED" = true ]; then
                echo -e "\n${C_YELLOW}[CANCELED]${C_RESET} Face model was not restored."
            else
                echo -e "\n${C_GREEN}[SUCCESS]${C_RESET} Face model restoration complete!"
            fi
            read_choice "Press Enter to return to menu..." _
        else
            echo "Restoration aborted."
            sleep 1
        fi
    else
        echo -e "\n${C_BLUE}[INFO]${C_RESET} Inspecting archive manifest:"
        cmd_info "$target_item" || true
        echo ""

        local has_kde=false
        local has_hypr=false
        local manifest_raw
        manifest_raw="$(read_zip_manifest "$target_item" 2>/dev/null || true)"
        if echo "$manifest_raw" | jq -e '.apps.kde' >/dev/null 2>&1; then
            has_kde=true
        fi
        if echo "$manifest_raw" | jq -e '.apps.hypr' >/dev/null 2>&1; then
            has_hypr=true
        fi

        local app_count
        app_count=$(echo "$manifest_raw" | jq -r '.apps | keys | length' 2>/dev/null || echo 1)

        local import_extra_flags=""
        if [ "$app_count" -gt 1 ]; then
            echo -e "${C_BOLD}Restore options for archive:${C_RESET}"
            echo "  1) Restore all applications & desktop configurations [Recommended]"
            local opt_num=2
            local kde_opt=""
            local hypr_opt=""
            if [ "$has_kde" = true ]; then
                echo "  ${opt_num}) Restore KDE Plasma settings only"
                kde_opt="$opt_num"
                ((opt_num++)) || true
            fi
            if [ "$has_hypr" = true ]; then
                echo "  ${opt_num}) Restore Hyprland settings only"
                hypr_opt="$opt_num"
                ((opt_num++)) || true
            fi
            echo "  ${opt_num}) Restore specific application"
            local spec_opt="$opt_num"
            read_choice "Choose option [1-${opt_num}, default: 1]: " rest_opt
            if [ -n "$kde_opt" ] && [ "$rest_opt" = "$kde_opt" ]; then
                import_extra_flags="--app kde"
            elif [ -n "$hypr_opt" ] && [ "$rest_opt" = "$hypr_opt" ]; then
                import_extra_flags="--app hypr"
            elif [ "$rest_opt" = "$spec_opt" ]; then
                read_choice "Enter application name to restore (e.g. beeper, obsidian, hypr, kde): " chosen_app
                [ -n "$chosen_app" ] && import_extra_flags="--app $chosen_app"
            else
                import_extra_flags=""
            fi
        fi

        read_choice "Proceed with restoration? (y/N): " confirm
        if [[ "$confirm" =~ ^[Yy]$ ]]; then
            echo -e "${C_CYAN}==>${C_RESET} Restoring files as ${C_BOLD}${TARGET_USER}${C_RESET}..."
            cmd_import "$target_item" $import_extra_flags -k
            if [ "$VAULT_CANCELED" = true ]; then
                echo -e "\n${C_YELLOW}[CANCELED]${C_RESET} Nothing was restored."
            else
                echo -e "\n${C_GREEN}[SUCCESS]${C_RESET} Restoration complete!"
            fi
            read_choice "Press Enter to return to menu..." _
        else
            echo "Restoration aborted."
            sleep 1
        fi
    fi
}

# ------------------------------------------------------------------------------
# State 7: Export data (App Vault Backup & Face Model)
# ------------------------------------------------------------------------------
do_export_data() {
    restore_terminal_interactive
    clear
    echo -e "${C_CYAN}${C_BOLD}=== Export / Backup Application Data & Face Models ===${C_RESET}\n"
    echo "Targets:"
    echo "  1) All detected applications & Howdy face model (Recommended)"
    echo "  2) BeeperTexts (~/.config/BeeperTexts)"
    echo "  3) ZaloData (~/.config/ZaloData)"
    echo "  4) Vesktop (~/.config/vesktop)"
    echo "  5) Zen Browser (~/.config/zen)"
    echo "  6) Obsidian (~/.config/obsidian)"
    echo "  7) Fcitx5 (~/.config/fcitx5)"
    echo "  8) Antigravity (~/.config/Antigravity)"
    echo "  9) Howdy Face Recognition Model (/etc/howdy/models)"
    echo "  10) KDE Plasma settings (curated ~/.config + ~/.local/share)"
    echo "  11) Hyprland settings (~/.config/hypr)"
    echo "  q) Cancel"
    echo ""
    read_choice "Choose target [1-11, q]: " target_choice

    local target="all"
    case "$target_choice" in
        1) target="all" ;;
        2) target="beeper" ;;
        3) target="zalo" ;;
        4) target="vesktop" ;;
        5) target="zen" ;;
        6) target="obsidian" ;;
        7) target="fcitx5" ;;
        8) target="antigravity" ;;
        9) target="howdy" ;;
        10) target="kde" ;;
        11) target="hypr" ;;
        q|Q|"") echo "Backup canceled."; sleep 1; return 0 ;;
        *) echo "Invalid choice: '$target_choice'"; sleep 1; return 0 ;;
    esac

    local is_dat_export=false
    if [ "$target" = "howdy" ]; then
        echo ""
        echo "Export Format for Howdy:"
        echo "  1) Standalone .dat file (compatible with setup_howdy.sh & manual import) [Recommended]"
        echo "  2) App Vault .zip archive (standard container with manifest)"
        read_choice "Choose format [1-2, default: 1]: " fmt_choice
        if [ "$fmt_choice" = "2" ]; then
            is_dat_export=false
        else
            is_dat_export=true
        fi
    fi

    local media_flag=""
    if [ "$target" != "howdy" ] && [ "$target" != "kde" ] && [ "$target" != "hypr" ]; then
        read_choice "Include large media/attachments? (Y/n): " inc_media
        if [[ "$inc_media" =~ ^[Nn]$ ]]; then
            media_flag="--no-media"
        fi
    fi

    local def_dest="/media/data/backups"
    if [ ! -d "/media/data" ]; then
        def_dest="${TARGET_HOME}/.local/share/app-vault-sync/backups"
    fi
    mkdir_user "$def_dest"

    local ts; ts="$(date +'%Y-%m-%d_%H%M%S')"
    local out_file
    if [ "$is_dat_export" = true ]; then
        out_file="${def_dest}/${TARGET_USER}_face_model_${ts}.dat"
    else
        out_file="${def_dest}/${target}_backup_${ts}.zip"
    fi

    echo -e "\nDestination: ${C_BOLD}${out_file}${C_RESET}"
    read_choice "Press Enter to start backup (or Ctrl+C to abort)..." _

    echo -e "\n${C_CYAN}==>${C_RESET} Executing backup as ${C_BOLD}${TARGET_USER}${C_RESET}..."
    cmd_backup "$target" -o "$out_file" $media_flag -k

    if [ "$VAULT_CANCELED" = true ]; then
        echo -e "\n${C_YELLOW}[CANCELED]${C_RESET} No backup was created."
    else
        echo -e "\n${C_GREEN}[SUCCESS]${C_RESET} Backup completed: ${out_file}"
    fi
    read_choice "Press Enter to return to menu..." _
}

# ------------------------------------------------------------------------------
# State 1: Run automatic script (Executes Steps 2..6 in order)
# ------------------------------------------------------------------------------
do_run_automatic_script() {
    local o_start; o_start=$(date +%s)
    local o_total=6
    SETUP_ISSUES=()

    log "INFO" "=========================================="
    log "INFO" "Starting Full Automated Setup Sequence..."
    log "INFO" "=========================================="

    # Step 1: Systemwide Upgrade
    do_system_upgrade true "$o_start" 1 "$o_total"

    # Step 2: Setup universal data partition
    do_setup_data_partition true "$o_start" 2 "$o_total"

    # Step 3: Add kud3n's repository
    do_add_kud3n_repo true "$o_start" 3 "$o_total"

    # Step 4: Install packages
    do_install_packages true "$o_start" 4 "$o_total"

    # Step 5: Setup services
    do_setup_services true "$o_start" 5 "$o_total"

    # Step 6: Import data
    do_import_data true "$o_start" 6 "$o_total"

    # Final Summary Screen
    restore_terminal_interactive
    clear
    local o_elapsed=$(( $(date +%s) - o_start ))
    local o_elapsed_str; o_elapsed_str="$(format_seconds "$o_elapsed")"

    local partition_state="${C_YELLOW}not mounted - user folders not migrated${C_RESET}"
    if findmnt /media/data >/dev/null 2>&1; then
        partition_state="/media/data mounted"
        [ -L "${TARGET_HOME}/Documents" ] && partition_state+=" & user folders symlinked"
    fi
    local repo_state="${C_YELLOW}not configured${C_RESET}"
    if grep -q '^\[archrepo\]' /etc/pacman.conf 2>/dev/null; then
        repo_state="Configured in /etc/pacman.conf"
    fi

    if [ ${#SETUP_ISSUES[@]} -eq 0 ]; then
        echo -e "${C_GREEN}${C_BOLD}"
        echo "================================================================================"
        echo "           🎉 AUTOMATED ARCH LINUX SETUP COMPLETED SUCCESSFULLY! 🎉           "
        echo "================================================================================"
    else
        echo -e "${C_YELLOW}${C_BOLD}"
        echo "================================================================================"
        printf "           ⚠  AUTOMATED SETUP FINISHED WITH %d ISSUE(S)  ⚠\n" "${#SETUP_ISSUES[@]}"
        echo "================================================================================"
    fi
    echo -e "${C_RESET}"
    echo -e "  • Total Time Elapsed: ${C_BOLD}${o_elapsed_str}${C_RESET}"
    echo -e "  • Target User:        ${C_BOLD}${TARGET_USER}${C_RESET} (UID: ${TARGET_UID}, GID: ${TARGET_GID})"
    echo -e "  • Data Partition:     ${partition_state}"
    echo -e "  • Kud3n Repo:         ${repo_state}"
    echo -e "  • Log File:           ${LOG_FILE}"
    if [ ${#SETUP_ISSUES[@]} -gt 0 ]; then
        echo ""
        echo -e "${C_YELLOW}${C_BOLD}Issues:${C_RESET}"
        local issue
        for issue in "${SETUP_ISSUES[@]}"; do
            echo -e "  ${C_RED}✘${C_RESET} ${issue}"
        done
    fi
    echo ""
    if [ -f "/etc/howdy/models/${TARGET_USER}.dat" ]; then
        echo -e "  • Howdy Face Model:   ${C_GREEN}Enrolled / Restored for ${TARGET_USER}${C_RESET}"
    else
        echo -e "${C_YELLOW}Note:${C_RESET} If you have not enrolled your face in Howdy yet, run:"
        echo "  sudo howdy -U ${TARGET_USER} add"
    fi
    echo ""
    read_choice "Press Enter to return to master menu..." _
}

# ==============================================================================
# MAIN MENU TUI CONTROLLER
# ==============================================================================
draw_menu() {
    local selected=$1
    get_term_dimensions
    local box_width=$UI_BOX_WIDTH
    local inner_width=$UI_INNER_WIDTH

    local h_line=""
    printf -v h_line "%*s" "$(( box_width - 2 ))" ""
    h_line="${h_line// /$BOX_H}"

    local buf=""
    buf+="${C_CYAN}${BOX_TL}${h_line}${BOX_TR}${C_RESET}\033[K\n"
    if (( inner_width < 50 )); then
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ ARCH / CACHYOS SETUP ◆${C_RESET}" "$inner_width")\n"
        buf+="$(box_line "${C_GRAY}Target: ${TARGET_USER}${C_RESET}" "$inner_width")\n"
    elif (( inner_width < 68 )); then
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ ARCH LINUX / CACHYOS SETUP ◆${C_RESET}" "$inner_width")\n"
        buf+="$(box_line "${C_GRAY}Target: ${TARGET_USER} │ Home: ${TARGET_HOME}${C_RESET}" "$inner_width")\n"
    else
        buf+="$(box_line "${C_BOLD}${C_WHITE}◆ ARCH LINUX / CACHYOS FAST SETUP CONTROLLER ◆${C_RESET}" "$inner_width")\n"
        buf+="$(box_line "${C_GRAY}Target: ${TARGET_USER} (UID: ${TARGET_UID}, GID: ${TARGET_GID}) │ Home: ${TARGET_HOME}${C_RESET}" "$inner_width")\n"
    fi
    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"

    local options=()
    if (( inner_width < 64 )); then
        options=(
            "1. Run full auto setup (Steps 2 -> 6)"
            "2. Setup data partition (/media/data)"
            "3. Add kud3n's repository"
            "4. Install packages (Official & custom)"
            "5. Setup services (VPN, IME, Howdy...)"
            "6. Import data (Vault, KDE & Face restore)"
            "7. Export data (Vault, KDE & Face backup)"
            "Q. Exit"
        )
    else
        options=(
            "1. Run automatic script (Complete setup: Steps 2 -> 6)"
            "2. Setup universal data partition (/media/data & symlinks)"
            "3. Add kud3n's repository ([archrepo] in /etc/pacman.conf)"
            "4. Install packages (Official, CachyOS, kud3n, add-ons)"
            "5. Setup services (ProtonVPN, IME, Howdy, Valkey/SearXNG)"
            "6. Import data (App Vault, KDE Plasma & Face Model restore)"
            "7. Export data (App Vault, KDE Plasma & Face Model backup)"
            "Q. Exit"
        )
    fi

    for i in "${!options[@]}"; do
        local opt="${options[$i]}"
        if [ "$i" -eq "$selected" ]; then
            buf+="$(box_line "${C_BG_BLUE}${C_WHITE}${C_BOLD} ❯ ${opt} ${C_RESET}" "$inner_width")\n"
        else
            buf+="$(box_line "   ${opt}" "$inner_width")\n"
        fi
    done

    buf+="${C_CYAN}${BOX_VL}${h_line}${BOX_VR}${C_RESET}\033[K\n"
    if (( inner_width < 50 )); then
        buf+="$(box_line "${C_GRAY}[↑/↓] Move │ [Enter] Run │ [Q] Quit${C_RESET}" "$inner_width")\n"
    elif (( inner_width < 68 )); then
        buf+="$(box_line "${C_GRAY}[↑/↓] Move │ [1-7] Pick │ [Enter] Run │ [Q] Quit${C_RESET}" "$inner_width")\n"
    else
        buf+="$(box_line "${C_GRAY}Controls: [↑/↓] Navigate │ [1-7/Q] Select │ [Enter] Run │ [Ctrl+C] Exit${C_RESET}" "$inner_width")\n"
    fi
    buf+="${C_CYAN}${BOX_BL}${h_line}${BOX_BR}${C_RESET}\033[K\n"

    tput cup 0 0 2>/dev/null || clear 2>/dev/null || true
    printf "%b" "$buf"
}

# Run a menu action in a subshell with errexit still active inside it, so a failure
# aborts that action and returns to the menu instead of killing the whole TUI.
run_menu_action() {
    local rc=0
    set +e
    ( set -e; trap cleanup_temp_state EXIT; "$@" )
    rc=$?
    set -e
    if (( rc != 0 )); then
        restore_terminal_interactive
        echo -e "\n${C_RED}[ERROR]${C_RESET} Action failed (exit code $rc). See ${LOG_FILE}"
        read_choice "Press Enter to return to menu..." _
    fi
}

run_menu() {
    init_terminal
    local selected=0
    local total_options=8

    while true; do
        draw_menu "$selected"

        IFS= read -rsn1 key
        if [[ "$key" == "$ESC" ]]; then
            read -rsn2 -t 0.1 rest || true
            key="${key}${rest}"
            case "$key" in
                "${ESC}[A")
                    ((selected--)) || true
                    if (( selected < 0 )); then selected=$(( total_options - 1 )); fi
                    ;;
                "${ESC}[B")
                    ((selected++)) || true
                    if (( selected >= total_options )); then selected=0; fi
                    ;;
            esac
        elif [[ "$key" == "" ]]; then
            case "$selected" in
                0) run_menu_action do_run_automatic_script ;;
                1) run_menu_action do_setup_data_partition false 0 1 1 ;;
                2) run_menu_action do_add_kud3n_repo false 0 1 1 ;;
                3) run_menu_action do_install_packages false 0 1 1 ;;
                4) run_menu_action do_setup_services false 0 1 1 ;;
                5) run_menu_action do_import_data false 0 1 1 ;;
                6) run_menu_action do_export_data ;;
                7) break ;;
            esac
            init_terminal
        elif [[ "$key" =~ ^[1-7]$ ]]; then
            case "$key" in
                1) run_menu_action do_run_automatic_script ;;
                2) run_menu_action do_setup_data_partition false 0 1 1 ;;
                3) run_menu_action do_add_kud3n_repo false 0 1 1 ;;
                4) run_menu_action do_install_packages false 0 1 1 ;;
                5) run_menu_action do_setup_services false 0 1 1 ;;
                6) run_menu_action do_import_data false 0 1 1 ;;
                7) run_menu_action do_export_data ;;
            esac
            init_terminal
        elif [[ "$key" =~ ^[qQ]$ ]]; then
            break
        fi
    done

    cleanup_terminal
    echo -e "${C_GREEN}Arch Linux Setup Controller exited.${C_RESET}"
}

# --- Direct CLI Parameter Handling ---
case "${1:-}" in
    auto|automatic)
        do_run_automatic_script
        ;;
    partition|data)
        do_setup_data_partition false 0 1 1
        ;;
    repo|archrepo)
        do_add_kud3n_repo false 0 1 1
        ;;
    packages|pkgs)
        do_install_packages false 0 1 1
        ;;
    services)
        do_setup_services false 0 1 1
        ;;
    import|restore)
        shift
        if [ $# -gt 0 ]; then
            cmd_import "$@"
        else
            do_import_data false 0 1 1
        fi
        ;;
    export|backup)
        shift
        if [ $# -gt 0 ]; then
            cmd_backup "$@"
        else
            do_export_data
        fi
        ;;
    status)
        shift
        cmd_status "$@"
        ;;
    info|inspect)
        shift
        cmd_info "$@"
        ;;
    vault)
        shift
        sub="${1:-}"
        shift || true
        case "$sub" in
            status) cmd_status "$@" ;;
            backup|export) cmd_backup "$@" ;;
            import|restore) cmd_import "$@" ;;
            info|inspect) cmd_info "$@" ;;
            *) echo "Usage: sudo $0 vault [status|backup|import|info]"; exit 1 ;;
        esac
        ;;
    test|noop)
        return 0 2>/dev/null || exit 0
        ;;
    "")
        run_menu
        ;;
    *)
        echo "Usage: sudo $0 [auto|partition|repo|packages|services|import|export|status|backup|restore|info]"
        echo "Run without arguments to launch interactive TUI menu."
        exit 1
        ;;
esac
