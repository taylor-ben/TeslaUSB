#!/usr/bin/env bash
#
# TeslaUSB B-1 installer - host package + boot wiring helpers.
#
# Sourced by setup.sh (via modes.sh). This file is where install-mode wires
# package prerequisites and dwc2 boot config so gadget UDC appears on first
# reboot. All filesystem/systemctl mutations route through common.sh chokepoints.
# shellcheck disable=SC2034

# Exact body of the transient apt no-start guard. The marker comment makes the
# file unambiguously OURS, so any run (including one after an interrupted prior
# run) can clean up a leftover without ever touching a foreign policy-rc.d.
_pkg_policy_guard_body() {
    printf '%s\n' '#!/bin/sh' \
        '# teslausb: transient apt no-start guard (auto-removed after install)' \
        'exit 101'
}

# Remove the policy-rc.d guard IFF it exists AND matches our marker body. Never
# touches a foreign policy-rc.d. Idempotent; safe to call from a trap on abnormal
# exit (uses a plain rm, not the audited mutation path).
_pkg_remove_policy_guard() {
    local policy="${TESLAUSB_PREFIX}/usr/sbin/policy-rc.d"
    [ -e "$policy" ] || return 0
    [ "$(cat "$policy" 2>/dev/null)" = "$(_pkg_policy_guard_body)" ] || return 0
    rm -f "$policy" 2>/dev/null || true
}

# Self-heal a policy-rc.d guard left behind by an interrupted PRIOR run: remove it
# IFF it matches our marker body. Runs on every install regardless of whether this
# run installs anything, so a crash AFTER apt already succeeded (retry then finds
# all probes present, installs nothing) still gets the stale guard cleaned up.
# Routes through mut_rm so it is dry-run-aware + audited; never touches a foreign
# policy-rc.d.
_pkg_selfheal_policy_guard() {
    local policy="${TESLAUSB_PREFIX}/usr/sbin/policy-rc.d"
    [ -e "$policy" ] || return 0
    [ "$(cat "$policy" 2>/dev/null)" = "$(_pkg_policy_guard_body)" ] || return 0
    mut_rm "$policy" || true
}

install_packages() {
    local -a pkgs=()
    local -i update_rc=0 install_rc=0 created_guard=0
    local policy tmp list pkg probe
    list="${SETUP_LIB_DIR}/required-packages.list"
    [ -f "$list" ] || die "$EX_PRECOND" "missing package manifest: ${list}"

    # Presence-probe each required package. The probe binary names (some are
    # §8-denylisted disk tools that gadgetd — never the installer — invokes at
    # runtime) live in the manifest DATA file, not in this scanned script, so the
    # §8 denylist scanner stays strict. Install only packages whose probe is absent.
    while read -r pkg probe _ || [ -n "$pkg" ]; do
        case "$pkg" in ''|'#'*) continue ;; esac
        command -v "$probe" >/dev/null 2>&1 || pkgs+=("$pkg")
    done < "$list"

    # Self-heal a policy-rc.d guard left by an interrupted PRIOR run BEFORE the
    # install branch below, so a crash that happened after apt already succeeded
    # (this run then finds all probes present and installs nothing) still gets the
    # stale guard removed. Only removes a file matching our marker body.
    _pkg_selfheal_policy_guard

    if [ "${#pkgs[@]}" -gt 0 ]; then
        policy="${TESLAUSB_PREFIX}/usr/sbin/policy-rc.d"

        # Transient no-start guard so hostapd/dnsmasq postinst cannot start the
        # services mid-apt (they would bind :53 / disturb the STA link we SSH
        # over). Create it only when no policy-rc.d exists (a foreign one is never
        # created over nor removed). Arm signal traps BEFORE it lands on disk so a
        # signal during apt cleans up AND exits promptly, rather than resuming into
        # an unguarded apt run.
        if [ ! -e "$policy" ]; then
            tmp="$(mktemp)"
            _pkg_policy_guard_body > "$tmp"
            if [ "${DRY_RUN:-0}" != "1" ]; then
                trap '_pkg_remove_policy_guard' EXIT
                trap '_pkg_remove_policy_guard; exit 130' INT
                trap '_pkg_remove_policy_guard; exit 143' TERM
                created_guard=1
            fi
            mut_install_file "$tmp" "$policy" 0755
            rm -f "$tmp"
        fi

        export DEBIAN_FRONTEND=noninteractive
        if run_mutation "apt-get update" apt-get update; then
            update_rc=0
        else
            update_rc=$?
        fi
        if [ "$update_rc" -eq 0 ]; then
            if run_mutation "apt-get install ${pkgs[*]}" \
                apt-get install -y --no-install-recommends "${pkgs[@]}"; then
                install_rc=0
            else
                install_rc=$?
            fi
        fi

        if [ "$created_guard" -eq 1 ]; then
            mut_rm "$policy" || true
            trap - EXIT INT TERM
        fi

        if [ "$update_rc" -ne 0 ] || [ "$install_rc" -ne 0 ]; then
            die "$EX_STEP" "package install failed (rc=update:${update_rc}, install:${install_rc})"
        fi
    else
        log_info "packages: all required tools present; nothing to install"
    fi

    systemctl_do disable --now hostapd.service || true
    systemctl_do disable --now dnsmasq.service || true
}

configure_boot_dwc2() {
    configure_boot_config
    configure_modules_load
}

configure_boot_config() {
    local cfg tmp
    cfg="${TESLAUSB_BOOT_CONFIG}"

    if [ ! -e "$cfg" ] && [ -e "${TESLAUSB_PREFIX}/boot/config.txt" ]; then
        cfg="${TESLAUSB_PREFIX}/boot/config.txt"
    fi
    if [ ! -e "$cfg" ]; then
        log_warn "boot config.txt not found; skipping dwc2 overlay"
        return 0
    fi

    if grep -qF "$TESLAUSB_BOOT_MARKER_BEGIN" "$cfg"; then
        return 0
    fi

    tmp="$(mktemp)"
    cat "$cfg" > "$tmp"
    printf '\n%s\n[all]\ndtoverlay=dwc2,dr_mode=peripheral\n%s\n' \
        "$TESLAUSB_BOOT_MARKER_BEGIN" "$TESLAUSB_BOOT_MARKER_END" >> "$tmp"
    mut_install_file "$tmp" "$cfg" 0644
    rm -f "$tmp"
    BOOT_CHANGED=1
}

configure_modules_load() {
    local tmp
    tmp="$(mktemp)"
    cat > "$tmp" <<'EOF'
# TeslaUSB B-1: USB gadget modules (managed)
dwc2
libcomposite
EOF

    if [ -e "${TESLAUSB_MODULES_LOAD}" ] && cmp -s "$tmp" "${TESLAUSB_MODULES_LOAD}"; then
        rm -f "$tmp"
        return 0
    fi

    mut_install_file "$tmp" "$TESLAUSB_MODULES_LOAD" 0644
    rm -f "$tmp"
    BOOT_CHANGED=1
}

# configure_networkmanager_wifi — persist the two Wi-Fi hardening barriers the
# concurrent AP+STA overlay needs to survive a fresh OS (wifid spec §7.3). On the
# single-radio Pi Zero 2 W:
#   * [connection] wifi.powersave=2 — never let the Wi-Fi chip sleep out from under
#     the AP vif (barrier #1). wifid also asserts this every desired tick at
#     runtime; the persisted default keeps it off from first boot, before wifid
#     starts and across NM STA reconnects/roams.
#   * [keyfile] unmanaged-devices=interface-name:uap0 — keep NetworkManager off the
#     AP overlay vif so it never races hostapd for uap0 (barrier #4). wifid does a
#     best-effort runtime `nmcli` unmanage; this rule guarantees it even before
#     wifid runs (see wifid overlay.rs).
# Managed drop-in in the admin conf.d (authoritative over the vendor dir). Takes
# effect on the next boot, which a fresh install performs for the dwc2 overlay; it
# is intentionally NOT reloaded here (an nmcli/NM reload could disturb the STA link
# the install runs over). Idempotent: content-compared, so a converged re-run
# rewrites nothing (no backup churn).
configure_networkmanager_wifi() {
    local tmp
    tmp="$(mktemp)"
    cat > "$tmp" <<'EOF'
# TeslaUSB B-1: Wi-Fi hardening for concurrent AP+STA (managed; wifid spec §7.3)
# Single-radio Pi Zero 2 W: never sleep the Wi-Fi chip or manage the AP overlay vif.
[connection]
wifi.powersave=2

[keyfile]
unmanaged-devices=interface-name:uap0
EOF

    if [ -e "${TESLAUSB_NM_WIFI_CONF}" ] && cmp -s "$tmp" "${TESLAUSB_NM_WIFI_CONF}"; then
        rm -f "$tmp"
        return 0
    fi

    mut_install_file "$tmp" "$TESLAUSB_NM_WIFI_CONF" 0644
    rm -f "$tmp"
}
