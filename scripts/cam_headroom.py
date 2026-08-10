#!/usr/bin/env python3
"""
Keep free space on the cam image above a floor while the car is recording.

Tesla writes about 180 MB per minute of RecentClips across six cameras, so a
cam image that starts an evening with 8 GB free is full within the hour. A full
drive does not degrade gracefully: the car stops recording ENTIRELY, so there is
no dashcam footage, no SentryClips folder for a Sentry event, and nothing for
the uploader to send. That is an availability failure, which is why this runs on
a timer instead of at boot.

Boot cleanup cannot do this job. Its policy protects every file younger than one
hour, and Tesla's rolling buffer is only about an hour long, so the policy can
never reach the bytes that actually fill the disk.

Two facts learned the hard way on 2026-08-08, both encoded below:

  * Never ask the filesystem how much room is left. A vfat volume's free-cluster
    count comes from the FSINFO hint sector, and the car does not maintain it
    while it holds the drive: the car wrote 2.7 GB of clips and `statvfs`
    reported the same 8.04 GB free at both ends, against a true 5.06 GB. That
    number is not merely stale, it is frozen, so a guard watching it would never
    fire while the disk filled underneath. On a long-lived mount it is worse
    still: the standing read-only mount reported 5.4 GB free against a real
    475 MB, which is how a full drive went unnoticed for three hours. Usage is
    therefore counted by adding up the sizes of the files that are actually
    there, after dropping the page cache, because a loop device serves the image
    through it and will otherwise hand back a directory listing from before the
    car's latest writes. The directory listing tells the truth once the cache is
    dropped; only the free-space figure is unusable, on any mount, however
    fresh.
  * Order clips by FILENAME, never by mtime. The car writes FAT timestamps in
    its own timezone and the kernel reinterprets them, so a clip recorded at
    20:12 stats as 11:13. The name carries the truth.

Mode switches go through the web service rather than straight to the scripts.
It pauses the indexing and archive workers first (unmounting under a mid-file
copy corrupts it) and re-attaches the file watcher afterwards (its inotify
watches otherwise point at unmounted inodes, and SD plus cloud archiving stops
dead until someone reboots). A refusal there means a worker is busy, which is a
good reason to skip a sweep and come back in five minutes.

Only RecentClips is ever touched, and that is hard-coded rather than
configurable: it is Tesla's own throwaway ring, the car overwrites it anyway,
and no amount of misconfiguration should let a headroom sweep reach a Sentry
event. Evicting event folders needs proof they are safe elsewhere, which is a
separate job.

Getting the drive back to the car outranks everything else here. A sweep that
dies halfway leaves the car with no USB drive at all, so the marker file makes
that state recoverable by the next run, and SIGTERM is caught so systemd
stopping this unit still hands the drive back.
"""

import calendar
import fcntl
import logging
import os
import re
import signal
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
sys.path.insert(0, str(SCRIPT_DIR / 'web'))

from config import (
    GADGET_DIR,
    MNT_DIR,
    STATE_FILE,
    WEB_PORT,
    get_script_path,
)

GIB = 1024 ** 3

# Prune when free space drops below this. It covers a Sentry event folder
# (1.8 GB measured) landing all at once plus a timer interval of recording
# (~900 MB), with room left over for the estimate to be wrong.
FLOOR_BYTES = 4 * GIB

# Prune down to roughly this much free. Each sweep costs the car a ~15 second
# gap while the drive is detached, so the gap between sweeps wants to be as long
# as the disk allows.
#
# On a tight image this is usually unreachable and the sweep empties everything
# outside the keep window instead, which has a cost worth knowing: a driver who
# taps "save dashcam clip" just after a sweep gets the keep window rather than
# Tesla's usual hour. The fix for that is a larger cam image, not a smaller
# target — trimming less just means sweeping more often.
# 2026-08-10: was 8 GiB, chosen when the image was 12 GiB and this target was
# usually unreachable. At 48 GiB it governs. The sweep interval is set by the
# gap down to FLOOR_BYTES, not by this value alone: (16-4) GiB / 180 MB/min is
# ~72 min between sweeps, against ~24 min at the old 8 GiB.
#
# The trade this buys, stated because raising it is not free: each sweep now
# has to free ~12 GiB instead of ~4 GiB, and all of it comes from RecentClips
# outside KEEP_NEWEST_SECONDS. If Tesla's ring really is ~1 h (~10.5 GiB at
# this rate) then 12 GiB is the whole deletable pool, every sweep empties the
# ring to the keep window, and a driver who taps "save clip" just after one
# gets ten minutes instead of an hour — the exact cost described above.
# It rests on an open question: the ring has never been seen to rotate (largest
# observation 9.9 GiB over 76 min, still growing when the disk filled), and on
# a 48 GiB image the pool may be far larger than an hour, in which case a
# 12 GiB need trims only the oldest slice and ring depth is fine.
# To settle it, watch a sweep on the box: if `fell_short` is true or the sweep
# deletes the entire pool, the ring does rotate and this wants to come back
# down to somewhere between 8 and 12 GiB.
TARGET_BYTES = 16 * GIB

# Never touch the newest ten minutes: the car may still be writing them, and
# they are the footage most likely to matter. This also caps how much a sweep
# can reclaim, so it trades directly against sweep frequency.
KEEP_NEWEST_SECONDS = 10 * 60

# Tesla's ring holds about an hour. A folder claiming to span far more than a
# day has a corrupt filename in it, so the keep window anchors no further than
# this from the oldest clip. Without the clamp, one garbled directory entry on a
# car-yanked vfat dates the whole ring as ancient and the sweep deletes all of
# it, newest minutes included. It cannot be clamped against our own clock: the
# car names files in its timezone, which is not the Pi's.
MAX_RING_SPAN_SECONDS = 24 * 60 * 60

# After a sweep that could not reach the floor, wait this long before trying
# again. Without it, an image holding more event folders than the ring can
# offset detaches the drive every five minutes forever, each cycle costing a
# recording gap and a full fsck of both images.
COOLDOWN_SECONDS = 60 * 60

MODE_SWITCH_TIMEOUT = 180

LOCK_PATH = os.path.join(GADGET_DIR, 'cam_headroom.lock')
# Written while this process owns the edit session, so a run that dies with the
# drive detached can be told apart from a human editing over Samba.
MARKER_PATH = os.path.join(GADGET_DIR, 'cam_headroom.sweeping')
COOLDOWN_PATH = os.path.join(GADGET_DIR, 'cam_headroom.cooldown')

SETTINGS_URL = f'http://127.0.0.1:{WEB_PORT}/settings'
UDC_PATH = '/sys/kernel/config/usb_gadget/teslausb/UDC'

# 2026-08-08_20-12-32-front.mp4
CLIP_NAME = re.compile(r'^(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})-.+\.mp4$')

logger = logging.getLogger('cam_headroom')


def clip_seconds(name):
    """Seconds since the epoch parsed from a clip's filename, or None.

    A real epoch rather than a digit-packing scheme, because the value is used
    for durations as well as ordering: a naive `month * 31 + day` clock makes
    the 1st of October look a day away from the 30th of September, which would
    put the previous evening's footage outside the keep window and delete it.

    The fields are range-checked so a corrupt directory entry parses as None
    rather than as a date in the year 9999.
    """
    match = CLIP_NAME.match(name)
    if match is None:
        return None
    year, month, day, hour, minute, second = (int(part) for part in match.groups())
    if not (1 <= month <= 12 and 1 <= day <= 31):
        return None
    if not (hour <= 23 and minute <= 59 and second <= 59):
        return None
    try:
        return calendar.timegm((year, month, day, hour, minute, second, 0, 0, 0))
    except (ValueError, OverflowError):
        return None


def used_bytes(mount, cluster_bytes):
    """Bytes occupied by everything under `mount`, rounded up to whole clusters.

    Rounding up matters in the safe direction: a file always costs whole
    clusters on disk, and under-counting usage would leave the guard thinking it
    has room it does not have. Unreadable entries are skipped rather than fatal,
    because the car is writing to this filesystem as we walk it.
    """
    total = 0
    for root, _, names in os.walk(mount):
        for name in names:
            try:
                size = os.path.getsize(os.path.join(root, name))
            except OSError:
                continue
            total += -(-size // cluster_bytes) * cluster_bytes
    return total


def drop_page_cache():
    """Force the next read of the image to come off the disk.

    A loop device serves the image through the page cache, so even a brand new
    mount can hand back directory blocks from before the car's latest writes.
    Measured 2026-08-08: a fresh probe mount reported the same 5.06 GB free for
    thirteen minutes running, byte for byte, while the true figure fell to
    2.09 GB. Without this the guard is blind twice over.

    The `sudo tee` shape is the one the repo standardised on in issue #152.
    """
    subprocess.run(['sync'], check=False)
    result = subprocess.run(
        ['sudo', 'tee', '/proc/sys/vm/drop_caches'],
        input='3\n', capture_output=True, text=True,
    )
    if result.returncode != 0:
        logger.error('could not drop page cache, readings may be stale: %s',
                     result.stderr.strip())


def free_bytes_at(mount):
    """Free bytes under a mounted cam image, counted from the files themselves.

    See the module docstring for why the filesystem's own answer is not used.
    """
    drop_page_cache()
    stat = os.statvfs(mount)
    capacity = stat.f_blocks * stat.f_frsize
    return max(0, capacity - used_bytes(mount, stat.f_frsize or 4096))


def free_bytes():
    """Free bytes on the cam image, read through the standing read-only mount.

    No private mount of its own: once usage is counted from the files rather
    than from the FSINFO sector, the mount TeslaUSB already keeps is as good a
    window onto the image as a fresh one, and asking for another is what breaks.
    `mount -o loop` reuses any loop device already bound to the same backing
    file, so a stale one left at the wrong capacity fails the mount outright
    ("Can't open blockdev"), and every probe risks leaking a loop device and a
    temp directory 288 times a day.
    """
    return free_bytes_at(Path(MNT_DIR) / 'part1-ro')


def clips_to_delete(recent_dir, need_bytes):
    """Oldest clips whose combined size covers `need_bytes`, newest ones spared.

    Returns (paths, bytes_freed). Falls short rather than eating into the keep
    window, so the caller must report a shortfall instead of assuming success.
    """
    clips = []
    for entry in os.scandir(recent_dir):
        at = clip_seconds(entry.name) if entry.is_file() else None
        if at is not None:
            clips.append((at, entry.path, entry.stat().st_size))
    if not clips:
        return [], 0

    # Anything dated more than a ring's span after the oldest clip is a corrupt
    # entry, not footage. Such a clip is neither trusted to place the keep
    # window nor deleted: both directions of nonsense then fail safe, because a
    # date far in the past leaves the anchor behind every real clip and nothing
    # is deleted at all.
    horizon = min(at for at, _, _ in clips) + MAX_RING_SPAN_SECONDS
    datable = [clip for clip in clips if clip[0] <= horizon]
    if not datable:
        return [], 0
    cutoff = max(at for at, _, _ in datable) - KEEP_NEWEST_SECONDS

    chosen, freed = [], 0
    for at, path, size in sorted(datable):
        if freed >= need_bytes or at >= cutoff:
            break
        chosen.append(path)
        freed += size
    return chosen, freed


def run_script(name):
    result = subprocess.run(
        ['bash', get_script_path(name)], capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f'{name} failed ({result.returncode}): {result.stderr[-400:]}')


def current_mode():
    try:
        return Path(STATE_FILE).read_text().strip()
    except OSError:
        return ''


def gadget_bound():
    """Whether the car can actually see a drive right now."""
    try:
        return Path(UDC_PATH).read_text().strip() != ''
    except OSError:
        return False


def post_settings(action):
    try:
        request = urllib.request.Request(
            f'{SETTINGS_URL}/{action}', data=b'', method='POST',
        )
        with urllib.request.urlopen(request, timeout=MODE_SWITCH_TIMEOUT) as response:
            response.read()
        return True
    except Exception as error:
        logger.warning('POST %s failed: %s', action, error)
        return False


def enter_edit_mode():
    """Ask the web service for the drive. False means do not sweep now.

    Deliberately no fallback to the script: a refusal here means a worker is
    mid-file, and going around it is how a clip gets corrupted.
    """
    if current_mode() != 'present':
        logger.info('mode is now %r, leaving this sweep alone', current_mode())
        return False
    post_settings('edit_usb')
    if current_mode() == 'edit':
        return True
    logger.warning('web service would not hand over the drive, skipping this sweep')
    return False


def leave_edit_mode():
    """Give the drive back. Falls back to the script, because a car with no
    drive records nothing and that outranks the worker contract."""
    if current_mode() == 'present' and gadget_bound():
        return True
    post_settings('present_usb')
    if current_mode() == 'present' and gadget_bound():
        return True
    logger.error('web service did not restore the drive, running present_usb.sh')
    try:
        run_script('present_usb.sh')
    except Exception as error:
        logger.error('present_usb.sh failed, the car has no drive: %s', error)
        return False
    return current_mode() == 'present' and gadget_bound()


def sweep(target_bytes):
    """Detach the drive, delete the oldest clips, hand it back.

    Returns (bytes_freed, fell_short). The amount to delete is re-measured here
    rather than passed in, so a sweep acts on what the disk holds at the moment
    it is detached.
    """
    Path(MARKER_PATH).write_text(f'{os.getpid()}\n')
    try:
        if not enter_edit_mode():
            return 0, False
        mount = Path(MNT_DIR) / 'part1'
        free = free_bytes_at(mount)
        need = target_bytes - free
        logger.info('%.1f GB free with the drive detached', free / GIB)
        if need <= 0:
            logger.info('already above target once flushed, deleting nothing')
            return 0, False

        recent_dir = mount / 'TeslaCam' / 'RecentClips'
        if not recent_dir.is_dir():
            logger.warning('no RecentClips at %s, nothing to reclaim', recent_dir)
            return 0, True
        paths, freed = clips_to_delete(recent_dir, need)
        for path in paths:
            os.unlink(path)
        os.sync()
        logger.info('deleted %d clips, %.1f GB', len(paths), freed / GIB)
        if freed < need:
            logger.warning(
                'wanted %.1f GB, reclaimed %.1f GB: RecentClips outside the newest '
                '%d minutes is exhausted. The rest of the disk is footage too new to '
                'touch and event folders this sweep never touches.',
                need / GIB, freed / GIB, KEEP_NEWEST_SECONDS // 60,
            )
        return freed, freed < need
    finally:
        if leave_edit_mode():
            Path(MARKER_PATH).unlink(missing_ok=True)


def in_cooldown(now):
    try:
        until = float(Path(COOLDOWN_PATH).read_text().strip())
    except (OSError, ValueError):
        return False
    return now < until


def start_cooldown(now):
    Path(COOLDOWN_PATH).write_text(f'{now + COOLDOWN_SECONDS}\n')
    logger.info(
        'the disk cannot reach the floor by trimming RecentClips alone, '
        'holding off for %d minutes', COOLDOWN_SECONDS // 60,
    )


def recover_interrupted_sweep():
    """Hand the drive back if a previous run died holding it.

    The marker is what separates our own half-finished sweep from a person
    editing over Samba, whose session must not be yanked out from under them.
    """
    if not os.path.exists(MARKER_PATH):
        return
    logger.error('a previous sweep did not finish, restoring the drive to the car')
    if leave_edit_mode():
        Path(MARKER_PATH).unlink(missing_ok=True)


def _on_terminate(signum, _frame):
    # Raising lets the sweep's `finally` re-present the drive. Without this,
    # systemd stopping the unit mid-sweep leaves the car with no drive and the
    # state file stuck on 'edit'.
    raise SystemExit(f'terminated by signal {signum}')


def main():
    logging.basicConfig(level=logging.INFO, format='%(message)s')
    signal.signal(signal.SIGTERM, _on_terminate)

    lock = open(LOCK_PATH, 'w')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        logger.info('another sweep is running')
        return 0

    recover_interrupted_sweep()

    mode = current_mode()
    if mode != 'present':
        logger.info('mode is %r, leaving the drive alone', mode or 'unset')
        return 0
    if not gadget_bound():
        logger.error('mode says present but no gadget is bound, re-presenting')
        leave_edit_mode()
        return 0

    now = time.time()
    if in_cooldown(now):
        logger.info('holding off after a sweep that could not reach the floor')
        return 0

    free = free_bytes()
    # Logged every run, not just when acting: this trend is what diagnosed both
    # of the measurement bugs above, and it costs one journal line per tick.
    logger.info('%.1f GB free, floor is %.1f GB', free / GIB, FLOOR_BYTES / GIB)
    if free >= FLOOR_BYTES:
        return 0

    logger.info('below the floor, reclaiming toward %.1f GB', TARGET_BYTES / GIB)
    _, fell_short = sweep(TARGET_BYTES)
    after = free_bytes()
    logger.info('%.1f GB free after sweep', after / GIB)
    if fell_short and after < FLOOR_BYTES:
        start_cooldown(now)
    return 0


if __name__ == '__main__':
    sys.exit(main())
