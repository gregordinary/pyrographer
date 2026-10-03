#!/usr/bin/env bash
#
# The Block backend's bench pass, start to finish, against a loop device.
#
# Everything destructive happens to a scratch file in /tmp. The loop device is
# created here rather than named by hand, because a loop number is neither
# stable nor memorable and the whole point of a bench is that it lands where it
# was aimed.
#
# Usage:
#     ./bench/run-block-bench.sh [size]        # size defaults to 256M
#
# It asks for a password once: the passes past the inventory open a block
# device, which needs root or membership of `disk`. Everything it prints is
# teed to bench/reports/.
#
# **It exits non-zero if any pass failed.** Every pass runs regardless -- a bench
# that stopped at the first failure would leave the rest of the backend unmeasured
# on exactly the run where measuring it matters -- so failures are remembered and
# reported at the end rather than aborting. `set -e` would do the opposite, and
# `pipefail` is what carries the group's own status out through the `tee`.

set -u -o pipefail

SIZE="${1:-256M}"
# How the privileged passes are run. Overridable so the script's own plumbing can
# be exercised without root: `SUDO=: ./bench/run-block-bench.sh` runs everything
# that needs no privilege and lets the rest refuse.
SUDO="${SUDO:-sudo}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
IMG="/tmp/pyrographer-blockbench.img"
STAMP="$(date +%Y-%m-%d-%H%M%S)"
LOG="$HERE/bench/reports/block-$STAMP.log"
BIN="$HERE/target/release/examples/blockbench"

cleanup() {
    [ -n "${DEV:-}" ] || return 0
    udisksctl unmount -b "$DEV" >/dev/null 2>&1
    udisksctl loop-delete -b "$DEV" >/dev/null 2>&1
    rm -f "$IMG"
}
trap cleanup EXIT

echo "building the harness"
( cd "$HERE" && cargo build --release --example blockbench ) || exit 1

echo "making a $SIZE scratch file at $IMG"
rm -f "$IMG"
truncate -s "$SIZE" "$IMG" || exit 1

DEV="$(udisksctl loop-setup -f "$IMG" | grep -o '/dev/loop[0-9]*')"
if [ -z "${DEV:-}" ]; then
    echo "could not set up a loop device" >&2
    exit 1
fi
echo "loop device $DEV backed by $IMG"

# One password, cached for the passes below.
[ "$SUDO" = "sudo" ] && { sudo -v || exit 1; }

# Every pass runs, and a failure is remembered rather than fatal. `rc` lives in
# the subshell the pipeline below puts this group in, so the group ends by
# exiting with it and `pipefail` carries that out to the script.
{
    rc=0

    echo "# blockbench, $STAMP"
    echo
    echo "host:   $(uname -sr)"
    echo "target: $DEV backed by $IMG ($SIZE)"
    echo "rustc:  $(rustc --version)"
    echo

    echo "############ pass 1: the inventory, unprivileged ############"
    "$BIN" --target "$DEV" --expect-unprivileged || rc=1

    echo
    echo "############ pass 2: a GPT, read back through the backend ############"
    # A table written by somebody else's tool, so what the codec reads is a real
    # GPT and not one pyrographer produced and then agreed with.
    # The setup counts too: a pass run against a device that was never partitioned
    # measures nothing, and reporting that as green is the failure mode this exit
    # status exists to close.
    $SUDO sfdisk --quiet --label gpt "$DEV" <<'PARTS' || rc=1
start=2048, size=40960, name="esp", type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B
start=43008, size=81920, name="boot"
start=124928, size=204800, name="rootfs"
PARTS
    $SUDO partprobe "$DEV" >/dev/null 2>&1
    $SUDO "$BIN" --target "$DEV" --dump-bytes 4M --no-inventory || rc=1

    echo
    echo "############ pass 3: the whole read and write path ############"
    $SUDO "$BIN" --target "$DEV" --confirm "$DEV" --write --no-inventory || rc=1

    echo
    echo "############ pass 4: the mounted case, where O_EXCL must refuse ############"
    # Both halves, because a pass that ran against an unmounted device would have
    # `O_EXCL` succeed and would be recorded as the refusal not happening.
    { $SUDO mkfs.ext4 -q -F "$DEV" && udisksctl mount -b "$DEV"; } || rc=1
    $SUDO "$BIN" --target "$DEV" --confirm "$DEV" --expect-busy --allow-mounted \
        --no-inventory || rc=1
    udisksctl unmount -b "$DEV"

    echo
    if [ "$rc" -eq 0 ]; then
        echo "############ done: every pass green ############"
    else
        echo "############ done: A PASS FAILED -- read the transcript above ############"
    fi
    exit "$rc"
} 2>&1 | tee "$LOG"
STATUS=$?

echo
echo "transcript: $LOG"
if [ "$STATUS" -ne 0 ]; then
    echo "the bench did not pass; see the transcript." >&2
fi
exit "$STATUS"
