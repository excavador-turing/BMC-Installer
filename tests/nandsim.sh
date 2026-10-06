#!/usr/bin/env bash
# Exercise install_on_mtd (the installer's UBI path) against the kernel's real UBI/UBIFS on nandsim.
#
#   sudo tests/nandsim.sh <path-to-install_on_mtd-binary>
#
# Exits 0 only if every case passes. Needs root, mtd-utils, erofs-utils and the nandsim, ubi,
# ubifs (and, optionally, erofs) kernel modules (linux-modules-extra-$(uname -r) on Ubuntu).
#
# The board is emulated as one MTD: 2 KiB pages, 128 KiB eraseblocks, 256 MiB (LEB = 126976), so
# UBI's bad-block reserve is computed from the same PEB count as on the real chip and
# `--device-pebs` is not needed. Every board is built the way the firmware does (uboot-env,
# rootfs, a UBIFS `overlay` full of settings, rootfs_prev), then reimaged with the binary while
# UBI is detached. UBI is always attached with `-O 2048` (VID header offset = one page), which is
# what the installer writes.
set -euo pipefail

BIN=${1:?usage: $0 <path-to-install_on_mtd-binary>}
BIN=$(readlink -f "$BIN")
[ -x "$BIN" ] || { echo "not an executable: $BIN" >&2; exit 2; }
[ "$(id -u)" -eq 0 ] || { echo "must run as root" >&2; exit 2; }

PAGE=2048
PEB_SIZE=131072
LEB_SIZE=$((PEB_SIZE - 2 * PAGE)) # 126976
FLASH_BYTES=$((256 * 1024 * 1024))
OVERLAY_LEBS=1250
UBI=0 # UBI device number used throughout

WORK=$(mktemp -d /tmp/nandsim-test.XXXXXX)
MNT="$WORK/mnt"
mkdir -p "$MNT"
MTD=""

# ---------------------------------------------------------------- helpers

log() { printf '    %s\n' "$*"; }
die() { printf '    ASSERT: %s\n' "$*" >&2; return 1; }

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }

# wait_for <seconds> <description> <command...>: poll a command, bounded.
wait_for() {
    local tries=$(($1 * 10)) what=$2
    shift 2
    while ! "$@" 2>/dev/null; do
        tries=$((tries - 1))
        [ "$tries" -gt 0 ] || { die "timed out waiting for $what"; return 1; }
        sleep 0.1
    done
}

cleanup_board() {
    # Best effort: nothing here may fail, and nothing may hang.
    mountpoint -q "$MNT" 2>/dev/null && umount "$MNT" 2>/dev/null
    mountpoint -q "$MNT/rootfs" 2>/dev/null && umount "$MNT/rootfs" 2>/dev/null
    local b
    for b in "/dev/ubiblock${UBI}_"*; do
        [ -e "$b" ] && ubiblock --remove "/dev/ubi${UBI}_${b##*_}" 2>/dev/null
    done
    [ -e "/sys/class/ubi/ubi$UBI" ] && ubidetach -d "$UBI" 2>/dev/null
    local i
    for i in 1 2 3 4 5 6 7 8 9 10; do
        grep -q '^nandsim ' /proc/modules || break
        rmmod nandsim 2>/dev/null && break
        sleep 1
    done
    return 0
}

cleanup_all() {
    local rc=$?
    set +e
    cleanup_board
    rm -rf "$WORK"
    exit "$rc"
}
trap cleanup_all EXIT

# fresh_nand: unload and reload nandsim so that every board starts from a blank chip.
fresh_nand() {
    cleanup_board
    # 2 Gbit x8, 2 KiB page, 128 KiB block (Micron MT29F2G08). Older kernels take first_id_byte=...
    if ! modprobe nandsim id_bytes=0x2c,0xda,0x90,0x95,0x06 2>/dev/null; then
        modprobe nandsim first_id_byte=0x2c second_id_byte=0xda third_id_byte=0x90 \
            fourth_id_byte=0x95
    fi
    modprobe ubi
    modprobe ubifs
    modprobe erofs 2>/dev/null || true # only for the optional read-only mount of the rootfs
    wait_for 10 "nandsim MTD" grep -q 'NAND simulator' /proc/mtd
    MTD=$(awk -F: '/NAND simulator/ { sub("mtd", "", $1); print $1; exit }' /proc/mtd)
    wait_for 10 "/dev/mtd$MTD" test -c "/dev/mtd$MTD"
    local ws es sz
    ws=$(<"/sys/class/mtd/mtd$MTD/writesize")
    es=$(<"/sys/class/mtd/mtd$MTD/erasesize")
    sz=$(<"/sys/class/mtd/mtd$MTD/size")
    if [ "$ws" -ne "$PAGE" ] || [ "$es" -ne "$PEB_SIZE" ] || [ "$sz" -ne "$FLASH_BYTES" ]; then
        mtdinfo "/dev/mtd$MTD" >&2 || true
        die "nandsim geometry is $ws/$es/$sz, want $PAGE/$PEB_SIZE/$FLASH_BYTES"
    fi
}

attach() {
    ubiattach -m "$MTD" -d "$UBI" -O "$PAGE" >/dev/null || die "ubiattach -m $MTD -O $PAGE failed"
    wait_for 10 "ubi$UBI" test -c "/dev/ubi$UBI"
    udevadm settle --timeout=10 2>/dev/null || true
}

detach() { ubidetach -d "$UBI" || die "ubidetach failed"; }

# vols: one "id name reserved_pebs type" line per volume of the attached UBI, sorted by id.
vols() {
    local d
    for d in "/sys/class/ubi/ubi${UBI}_"*; do
        [ -e "$d/name" ] || continue
        printf '%s %s %s %s\n' "${d##*_}" "$(<"$d/name")" "$(<"$d/reserved_ebs")" "$(<"$d/type")"
    done | sort -n
}
vol_field() { vols | awk -v n="$1" -v f="$2" '$2 == n { print $f }'; }
vol_names() { vols | awk '{ print $2 }' | sort | tr '\n' ' ' | sed 's/ $//'; }

wait_vol_nodes() {
    local id
    while read -r id _; do
        wait_for 10 "/dev/ubi${UBI}_$id" test -c "/dev/ubi${UBI}_$id"
    done < <(vols)
}

# erofs_size <image>: blocks * block size from the superblock (what the installer sizes from).
erofs_size() {
    local blkbits blocks
    blkbits=$(od -An -tu1 -j$((1024 + 12)) -N1 "$1" | tr -d ' ')
    blocks=$(od -An -tu4 -j$((1024 + 36)) -N4 "$1" | tr -d ' ')
    echo $((blocks << blkbits))
}

# make_erofs <out> <seed> <MiB>: a real EROFS holding a marker and random data.
make_erofs() {
    local d="$WORK/src.$2"
    mkdir -p "$d/etc"
    echo "rootfs image $2" >"$d/etc/os-release"
    head -c $(($3 * 1024 * 1024)) /dev/urandom >"$d/blob"
    mkfs.erofs "$1" "$d" >/dev/null
    rm -rf "$d"
}

manifest() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 sha256sum); }

# populate_overlay <dir>: the kind of content the firmware keeps in the overlay's upper dir.
populate_overlay() {
    local o=$1/upper i
    mkdir -p "$o/etc/ssl/certs" "$o/etc/deep/nested/dir" "$o/data"
    # shellcheck disable=SC2016
    echo 'root:$6$fakesalt$0123456789abcdefghijklmnopqrstuvwxyz:19000:0:99999:7:::' >"$o/etc/shadow"
    printf -- '-----BEGIN CERTIFICATE-----\n%s\n-----END CERTIFICATE-----\n' \
        "$(head -c 900 /dev/urandom | base64 -w64)" >"$o/etc/ssl/certs/bmcd_cert.pem"
    for i in $(seq 1 20); do
        head -c $((15000 + i * 700)) /dev/urandom >"$o/data/file$i"
    done
    echo "nested" >"$o/etc/deep/nested/dir/leaf"
    head -c $((16 * 1024 * 1024)) /dev/urandom >"$o/data/big" # spans ~130 LEBs
    # churn, so that UBIFS has superseded and unmapped LEBs behind it
    head -c $((5 * 1024 * 1024)) /dev/urandom >"$1/tmp-churn"
    sync
    rm -f "$1/tmp-churn"
    sync
}

ROOTFS_A="$WORK/rootfs-a.erofs" # what the board runs before the first install
ROOTFS_B="$WORK/rootfs-b.erofs" # first reimage
ROOTFS_C="$WORK/rootfs-c.erofs" # second reimage

# build_board <layout>: ubiformat + attach + the firmware's volumes, then detach.
# Layouts (volume ids in brackets):
#   std          uboot-env[0] rootfs[1] overlay[2] rootfs_prev[3]
#   overlay-last uboot-env[0] rootfs[1] rootfs_prev[2] overlay[3]
#   overlay-one  uboot-env[0] overlay[1] rootfs[2] rootfs_prev[3]
#   none         uboot-env[0] rootfs[1] rootfs_prev[2]      (no settings volume)
build_board() {
    local layout=$1 spec id name size
    case $layout in
        std) spec="0:uboot-env 1:rootfs 2:overlay 3:rootfs_prev" ;;
        overlay-last) spec="0:uboot-env 1:rootfs 2:rootfs_prev 3:overlay" ;;
        overlay-one) spec="0:uboot-env 1:overlay 2:rootfs 3:rootfs_prev" ;;
        none) spec="0:uboot-env 1:rootfs 2:rootfs_prev" ;;
        *) die "unknown layout $layout" ;;
    esac
    fresh_nand
    ubiformat "/dev/mtd$MTD" -O "$PAGE" -y >/dev/null || die "ubiformat failed"
    attach
    size=$(erofs_size "$ROOTFS_A")
    for item in $spec; do
        id=${item%%:*}
        name=${item#*:}
        case $name in
            uboot-env) ubimkvol "/dev/ubi$UBI" -n "$id" -N "$name" -s 65536 >/dev/null ;;
            rootfs | rootfs_prev) ubimkvol "/dev/ubi$UBI" -n "$id" -N "$name" -t static -s "$size" >/dev/null ;;
            overlay) ubimkvol "/dev/ubi$UBI" -n "$id" -N "$name" -s $((OVERLAY_LEBS * LEB_SIZE)) >/dev/null ;;
        esac
    done
    wait_vol_nodes
    for item in $spec; do
        id=${item%%:*}
        name=${item#*:}
        case $name in
            rootfs | rootfs_prev)
                ubiupdatevol "/dev/ubi${UBI}_$id" "$ROOTFS_A" || die "ubiupdatevol $name failed"
                ;;
            overlay)
                local empty="$WORK/empty"
                mkdir -p "$empty"
                mkfs.ubifs -r "$empty" -m "$PAGE" -e "$LEB_SIZE" -c "$OVERLAY_LEBS" \
                    -o "$WORK/overlay.ubifs" >/dev/null
                ubiupdatevol "/dev/ubi${UBI}_$id" "$WORK/overlay.ubifs"
                mount -t ubifs "ubi${UBI}:overlay" "$MNT"
                populate_overlay "$MNT"
                manifest "$MNT" >"$WORK/manifest"
                [ -s "$WORK/manifest" ] || die "empty manifest"
                umount "$MNT"
                ;;
        esac
    done
    vols >"$WORK/vols.before"
    log "board ($layout):"
    sed 's/^/        /' "$WORK/vols.before"
    detach
}

# install <rootfs> [extra args...]: run the binary; leaves its output in $WORK/out.
# Sets INSTALL_SECS.
install() {
    local img=$1 t0 t1 rc=0
    shift
    t0=$(now)
    "$BIN" --mtd "/dev/mtd$MTD" --rootfs "$img" "$@" >"$WORK/out" 2>&1 || rc=$?
    t1=$(now)
    INSTALL_SECS=$(elapsed "$t0" "$t1")
    sed 's/^/        | /' "$WORK/out"
    [ "$rc" -eq 0 ] || die "install_on_mtd exited with $rc"
    grep -qx 'done' "$WORK/out" || die "install_on_mtd did not print 'done'"
    log "install_on_mtd took ${INSTALL_SECS}s"
}

decision_is() { grep -qx "decision: $1" "$WORK/out" || die "expected 'decision: $1'"; }

# rootfs_matches <image>: the attached rootfs volume holds exactly this image.
rootfs_matches() {
    local id size
    id=$(vol_field rootfs 1)
    [ -n "$id" ] || die "no rootfs volume"
    size=$(erofs_size "$1")
    cmp -n "$size" "/dev/ubi${UBI}_$id" "$1" || die "rootfs volume differs from $1"
    # Bonus: mount it as a real EROFS through ubiblock, if the kernel supports it.
    mkdir -p "$MNT/rootfs"
    if ubiblock --create "/dev/ubi${UBI}_$id" 2>/dev/null &&
        wait_for 10 "ubiblock${UBI}_$id" test -b "/dev/ubiblock${UBI}_$id" &&
        mount -t erofs -o ro "/dev/ubiblock${UBI}_$id" "$MNT/rootfs" 2>/dev/null; then
        local got
        got=$(cat "$MNT/rootfs/etc/os-release")
        umount "$MNT/rootfs"
        ubiblock --remove "/dev/ubi${UBI}_$id"
        [ -n "$got" ] || die "mounted rootfs has no /etc/os-release"
        log "rootfs mounts as erofs: $got"
    else
        ubiblock --remove "/dev/ubi${UBI}_$id" 2>/dev/null || true
        log "SKIP erofs mount (no ubiblock/erofs support); byte comparison passed"
    fi
}

# no_ubi_errors: nothing in dmesg since the last `dmesg -C` complains about UBI or UBIFS.
no_ubi_errors() {
    local bad
    bad=$(dmesg | grep -iE 'ubi.*(error|err )|ubifs.*error' || true)
    [ -z "$bad" ] || { printf '%s\n' "$bad" | sed 's/^/        dmesg: /'; die "UBI/UBIFS errors in dmesg"; }
}

# overlay_intact <manifest>: mount, compare every hash, write a new file, remount, compare again.
overlay_intact() {
    local m=$1 stamp
    mount -t ubifs "ubi${UBI}:overlay" "$MNT" || die "overlay does not mount as ubifs"
    manifest "$MNT" >"$WORK/manifest.now"
    cmp -s "$m" "$WORK/manifest.now" || { diff "$m" "$WORK/manifest.now" | head; die "overlay content changed"; }
    stamp=$(head -c 300000 /dev/urandom | base64 -w0)
    printf '%s' "$stamp" >"$MNT/upper/etc/written-after-install"
    sync
    umount "$MNT"
    mount -t ubifs "ubi${UBI}:overlay" "$MNT" || die "overlay does not remount"
    [ "$(cat "$MNT/upper/etc/written-after-install")" = "$stamp" ] || die "new file lost after remount"
    manifest "$MNT" >"$m.new"
    grep -q 'written-after-install' "$m.new" || die "new file missing"
    umount "$MNT"
    mv "$m.new" "$m"
    log "overlay: all hashes match, new file written and read back after remount"
}

# verify_kept <rootfs image> <expected overlay id>
verify_kept() {
    local img=$1 id=$2 before after
    attach || return 1
    wait_vol_nodes
    log "after:"
    vols | sed 's/^/        /'
    [ "$(vol_names)" = "overlay rootfs uboot-env" ] ||
        die "volumes are '$(vol_names)', want 'overlay rootfs uboot-env' (no rootfs_prev)"
    [ "$(vol_field overlay 1)" = "$id" ] || die "overlay id is $(vol_field overlay 1), want $id"
    before=$(awk '$2 == "overlay"' "$WORK/vols.before")
    after=$(vols | awk '$2 == "overlay"')
    [ "$before" = "$after" ] || die "overlay changed: '$before' -> '$after'"
    before=$(awk '$2 == "uboot-env"' "$WORK/vols.before")
    after=$(vols | awk '$2 == "uboot-env"')
    [ "$before" = "$after" ] || die "uboot-env changed: '$before' -> '$after'"
    [ "$(vol_field uboot-env 1)" = 0 ] || die "uboot-env is not volume 0"
    rootfs_matches "$img"
    overlay_intact "$WORK/manifest"
    ubinfo -a >"$WORK/ubinfo" 2>&1 || true
    no_ubi_errors
    detach
}

# ---------------------------------------------------------------- cases

case_keep() {
    build_board std
    dmesg -C 2>/dev/null || true
    install "$ROOTFS_B"
    decision_is kept
    echo "$INSTALL_SECS" >"$WORK/time.keep"
    verify_kept "$ROOTFS_B" 2
    touch "$WORK/case-keep.ok"
}

case_second_install() {
    [ -e "$WORK/case-keep.ok" ] || die "needs case 'keep' to have passed"
    dmesg -C 2>/dev/null || true
    install "$ROOTFS_C"
    decision_is kept
    verify_kept "$ROOTFS_C" 2
}

case_factory_reset() {
    build_board std
    dmesg -C 2>/dev/null || true
    install "$ROOTFS_B" --factory-reset
    decision_is erased
    echo "$INSTALL_SECS" >"$WORK/time.reset"
    attach
    wait_vol_nodes
    vols | sed 's/^/        /'
    [ "$(vol_names)" = "rootfs uboot-env" ] || die "volumes are '$(vol_names)', want 'rootfs uboot-env'"
    [ -z "$(vol_field overlay 1)" ] || die "overlay survived a factory reset"
    rootfs_matches "$ROOTFS_B"
    no_ubi_errors
    detach
}

case_no_overlay() {
    build_board none
    dmesg -C 2>/dev/null || true
    install "$ROOTFS_B"
    decision_is erased
    grep -i '^reason:' "$WORK/out" | grep -qi overlay || die "reason does not mention the overlay"
    attach
    wait_vol_nodes
    [ "$(vol_names)" = "rootfs uboot-env" ] || die "volumes are '$(vol_names)'"
    rootfs_matches "$ROOTFS_B"
    no_ubi_errors
    detach
}

case_corrupt_superblock() {
    build_board std
    attach
    wait_vol_nodes
    local id vol="/dev/ubi${UBI}_2"
    id=$(vol_field overlay 1)
    [ "$id" = 2 ] || die "overlay is not volume 2"
    # Zero the start of LEB 0 (the UBIFS superblock node, magic 0x06101831) and rewrite the volume.
    head -c "$LEB_SIZE" "$vol" >"$WORK/leb0"
    head -c 4096 /dev/zero | dd of="$WORK/leb0" bs=4096 count=1 conv=notrunc status=none
    ubiupdatevol "$vol" "$WORK/leb0"
    if mount -t ubifs "ubi${UBI}:overlay" "$MNT" 2>/dev/null; then
        umount "$MNT"
        die "test setup: corrupted overlay still mounts"
    fi
    detach
    dmesg -C 2>/dev/null || true
    install "$ROOTFS_B"
    decision_is erased
    attach
    wait_vol_nodes
    [ "$(vol_names)" = "rootfs uboot-env" ] || die "volumes are '$(vol_names)'"
    rootfs_matches "$ROOTFS_B"
    no_ubi_errors
    detach
}

case_overlay_other_id() { # $1 = layout, $2 = expected overlay id
    build_board "$1"
    dmesg -C 2>/dev/null || true
    install "$ROOTFS_B"
    decision_is kept
    grep -q "(id $2," "$WORK/out" || die "detail does not report id $2"
    verify_kept "$ROOTFS_B" "$2"
}

# ---------------------------------------------------------------- driver

FAILED=0
RESULTS=()

run_case() {
    local name=$1 rc=0
    shift
    echo "=== CASE $name"
    set +e
    (
        set -eEuo pipefail
        trap 'echo "    failed at line $LINENO: $BASH_COMMAND" >&2' ERR
        "$@"
    )
    rc=$?
    set -e
    if [ "$rc" -eq 0 ]; then
        echo "PASS: $name"
        RESULTS+=("PASS $name")
    else
        echo "FAIL: $name"
        RESULTS+=("FAIL $name")
        FAILED=$((FAILED + 1))
        echo "--- dmesg | tail -50"
        dmesg | tail -50
        echo "---"
    fi
}

echo "binary: $BIN"
echo "kernel: $(uname -r)"
make_erofs "$ROOTFS_A" a 6
make_erofs "$ROOTFS_B" b 6
make_erofs "$ROOTFS_C" c 7
cmp -s "$ROOTFS_A" "$ROOTFS_B" && { echo "test setup: identical images" >&2; exit 2; }

run_case "1 keep: reimage keeps the overlay, rootfs replaced, UBIFS writable" case_keep
run_case "2 second install on top of case 1" case_second_install
run_case "3 factory reset erases the overlay" case_factory_reset
run_case "4 no overlay: erased, UBI attaches" case_no_overlay
run_case "5 corrupt UBIFS superblock: erased" case_corrupt_superblock
run_case "6a overlay at id 3 (after rootfs_prev): kept, id preserved" \
    case_overlay_other_id overlay-last 3
run_case "6b overlay at id 1 (rootfs must move): kept, id preserved" \
    case_overlay_other_id overlay-one 1

echo
echo "=== SUMMARY"
printf '%s\n' "${RESULTS[@]}"
if [ -e "$WORK/time.keep" ] && [ -e "$WORK/time.reset" ]; then
    echo "timing: keep run $(<"$WORK/time.keep")s, factory-reset run $(<"$WORK/time.reset")s"
fi
if [ "$FAILED" -ne 0 ]; then
    echo "$FAILED case(s) FAILED"
    exit 1
fi
echo "all cases passed"
