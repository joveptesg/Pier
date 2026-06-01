#!/usr/bin/env bash
# ============================================================================
# Pier PaaS — Shared swap helper
# Sourced by build-from-source.sh (swap BEFORE the build) and install.sh
# (runtime safety floor). Idempotent and safe to call repeatedly.
#
#   ensure_swap MIN_FLOOR_MB TARGET_EFFECTIVE_MB
#     Guarantees:  active swap >= MIN_FLOOR_MB   AND   RAM+swap >= TARGET_EFFECTIVE_MB
#     by growing /swapfile with only the missing deficit. Then nudges
#     vm.swappiness toward an overflow-friendly value (only if still default).
#
# Sizing rationale (see plan):
#   - A 4 GiB swap floor exists on every server as an OOM-killer relief valve,
#     even on high-RAM hosts where the build target is already met.
#   - The build target (RAM+swap >= N) covers the rustc memory peak of a cold
#     release build on low-RAM machines.
#   - `swap = RAM` is intentionally avoided: it under-provisions tiny VPS and
#     wastes disk on large ones.
# ============================================================================

# Override the swapfile path by exporting SWAPFILE before sourcing.
: "${SWAPFILE:=/swapfile}"

# Fall back to plain echoes if the host script didn't define loggers.
command -v info >/dev/null 2>&1 || info()  { echo "[INFO]  $*"; }
command -v warn >/dev/null 2>&1 || warn()  { echo "[WARN]  $*"; }
command -v step >/dev/null 2>&1 || step()  { echo "[STEP]  $*"; }

# tune_swappiness WANT
# Persist vm.swappiness=WANT, but ONLY when the current value is the kernel
# default (60). A non-default value means the operator tuned it on purpose —
# we never override that.
tune_swappiness() {
    local want="$1" cur
    cur=$(cat /proc/sys/vm/swappiness 2>/dev/null || echo "")
    if [[ "$cur" != "60" ]]; then
        info "vm.swappiness=${cur:-unknown} (не дефолт) — не меняю."
        return 0
    fi
    echo "vm.swappiness=${want}" > /etc/sysctl.d/99-pier-swap.conf 2>/dev/null \
        || { warn "Не удалось записать /etc/sysctl.d/99-pier-swap.conf — пропускаю swappiness."; return 0; }
    sysctl -q -w "vm.swappiness=${want}" >/dev/null 2>&1 || true
    info "vm.swappiness → ${want} (persist в /etc/sysctl.d/99-pier-swap.conf)."
}

# ensure_swap FLOOR_MB TARGET_EFFECTIVE_MB
ensure_swap() {
    local floor_mb="$1" target_mb="$2"
    local mem_mb swap_mb eff_mb wanted_total cap_ram add_mb disk_free_mb headroom_mb max_by_disk

    mem_mb=$(( $(awk '/^MemTotal:/ {print $2}' /proc/meminfo) / 1024 ))
    swap_mb=$(( $(awk '/^SwapTotal:/ {print $2}' /proc/meminfo) / 1024 ))
    eff_mb=$(( mem_mb + swap_mb ))

    # Desired TOTAL swap: a floor (e.g. 4 GiB), or enough that RAM+swap reaches the
    # build target — whichever is larger. Then cap at 2×RAM so tiny VPS don't get an
    # oversized swapfile (1 GiB RAM ⇒ ≤2 GiB swap).
    wanted_total=$(( floor_mb > target_mb - mem_mb ? floor_mb : target_mb - mem_mb ))
    cap_ram=$(( 2 * mem_mb ))
    (( wanted_total > cap_ram )) && wanted_total=$cap_ram

    add_mb=$(( wanted_total - swap_mb ))   # add only the deficit on top of existing swap
    if (( add_mb <= 0 )); then
        info "Swap достаточен (swap=${swap_mb}MiB, eff=${eff_mb}MiB; цель total=${wanted_total}MiB) — изменений нет."
        return 0
    fi
    add_mb=$(( ((add_mb + 1023) / 1024) * 1024 ))   # округлить вверх до ГиБ

    # Disk-cap: keep headroom free for build artifacts/system. Instead of skipping
    # entirely when the desired swap won't fit, shrink it to what fits (rounded DOWN
    # to GiB); only skip if less than 1 GiB can be spared.
    headroom_mb="${SWAP_DISK_HEADROOM_MB:-4096}"
    disk_free_mb=$(df -BM --output=avail / | tail -n1 | tr -dc '0-9')
    max_by_disk=$(( disk_free_mb - headroom_mb ))
    if (( max_by_disk < 1024 )); then
        warn "Мало места на / (${disk_free_mb}MiB свободно, запас ${headroom_mb}MiB) — swap пропущен."
        return 1
    fi
    if (( add_mb > max_by_disk )); then
        add_mb=$(( (max_by_disk / 1024) * 1024 ))   # округлить ВНИЗ до ГиБ
        warn "Урезал swap до ${add_mb}MiB из-за свободного места на / (${disk_free_mb}MiB)."
    fi

    step "Настройка swap: +${add_mb}MiB at ${SWAPFILE} (floor=${floor_mb}, target=${target_mb}, cap2xRAM=${cap_ram}, было swap=${swap_mb}, итого ≈ $(( swap_mb + add_mb ))MiB)..."

    if swapon --show=NAME --noheadings 2>/dev/null | grep -qx "$SWAPFILE"; then
        info "${SWAPFILE} уже активен — пропускаю создание."
    else
        if [[ -e "$SWAPFILE" ]]; then
            warn "${SWAPFILE} существует, но не активен — переиспользую как есть."
        else
            if command -v fallocate >/dev/null 2>&1; then
                fallocate -l "${add_mb}M" "$SWAPFILE"
            else
                dd if=/dev/zero of="$SWAPFILE" bs=1M count="$add_mb" status=progress
            fi
            chmod 600 "$SWAPFILE"
            mkswap "$SWAPFILE" >/dev/null
        fi
        swapon "$SWAPFILE"
        info "Swap активирован (+${add_mb}MiB, суммарный swap ≈ $(( swap_mb + add_mb ))MiB)."
    fi

    if ! grep -qE "^${SWAPFILE}[[:space:]]+" /etc/fstab 2>/dev/null; then
        echo "${SWAPFILE} none swap sw 0 0" >> /etc/fstab
        info "Добавил ${SWAPFILE} в /etc/fstab (переживёт перезагрузку)."
    else
        info "${SWAPFILE} уже в /etc/fstab — пропускаю."
    fi

    tune_swappiness 10
}
