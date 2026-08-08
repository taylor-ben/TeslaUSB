#!/usr/bin/env python3
"""
Keep free space on the cam image above a floor while the car is recording.

Tesla writes about 130 MB per minute of RecentClips across six cameras, so a
12 GB image that starts an evening with 8 GB free is full inside an hour. A
full drive does not degrade gracefully: the car stops recording ENTIRELY, so
there is no dashcam footage, no SentryClips folder for a Sentry event, and
nothing for the uploader to send. That is an availability failure, which is
why this runs on a timer instead of at boot.

Boot cleanup cannot do this job. Its policy protects every file younger than
one hour, and Tesla's rolling buffer is only about an hour long, so the policy
can never reach the bytes that actually fill the disk.

Two facts learned the hard way on 2026-08-08, both encoded below:

  * Never ask the filesystem how much room is left. A vfat volume's free-cluster
    count comes from the FSINFO hint sector, and the car does not maintain it
    while it holds the drive: measured 2026-08-08, the car wrote 2.7 GB of clips
    and `statvfs` reported the same 8.04 GB free before and after, against a
    true 5.06 GB. That number is not merely stale, it is frozen, so a guard
    watching it would never fire while the disk filled underneath. The standing
    read-only mount is worse still: it reported 5.4 GB free against a real
    475 MB, which is how a full drive went unnoticed for three hours.
    Usage is therefore counted the only way that tracks reality here, by adding
    up the sizes of the files that are actually there — after dropping the page
    cache, because a loop device serves the image through it and will otherwise
    hand back a directory listing from before the car's latest writes.
  * Order clips by FILENAME, never by mtime. The car writes FAT timestamps in
    its own timezone and the kernel reinterprets them, so a clip recorded at
    20:12 stats as 11:13. The name carries the truth.

Only RecentClips is ever touched, and that is hard-coded rather than
configurable: it is Tesla's own throwaway ring, the car overwrites it anyway,
and no amount of misconfiguration should let a headroom sweep reach a Sentry
event. Evicting event folders needs proof they are safe elsewhere, which is a
separate job.
"""

import fcntl
import logging
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
sys.path.insert(0, str(SCRIPT_DIR / 'web'))

from config import GADGET_DIR, IMG_CAM_PATH, MNT_DIR, STATE_FILE, get_script_path

GIB = 1024 ** 3

# Prune when free space drops below this. It covers a Sentry event folder
# (1.8 GB measured) landing all at once plus a timer interval of recording
# (~800 MB), with room left over for the estimate to be wrong.
FLOOR_BYTES = 4 * GIB

# Prune down to roughly this much free, which buys about 25 minutes before the
# next sweep. Each sweep costs the car a ~15 second gap while the drive is
# detached, so the gap between sweeps wants to be as long as the disk allows.
#
# It cannot simply be raised: on a 12 GB image holding one 1.8 GB event, the
# keep window below is the ceiling, and asking for more than the disk can give
# just logs a shortfall every time. The real lever is a larger cam image.
TARGET_BYTES = 8 * GIB

# Never touch the newest ten minutes: the car may still be writing them, and
# they are the footage most likely to matter. This is also what caps how much a
# sweep can reclaim, so it trades directly against sweep frequency.
KEEP_NEWEST_SECONDS = 10 * 60

LOCK_PATH = os.path.join(GADGET_DIR, 'cam_headroom.lock')

# 2026-08-08_20-12-32-front.mp4
CLIP_NAME = re.compile(r'^(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})-.+\.mp4$')

logger = logging.getLogger('cam_headroom')


def clip_seconds(name):
    """Epoch-ish seconds parsed from a clip's filename, or None if it is not one.

    Absolute correctness does not matter here, only that the number orders the
    same way the car's clock does, so a naive day-count is enough.
    """
    match = CLIP_NAME.match(name)
    if match is None:
        return None
    year, month, day, hour, minute, second = (int(part) for part in match.groups())
    return ((((year * 12 + month) * 31 + day) * 24 + hour) * 60 + minute) * 60 + second


def used_bytes(mount, cluster_bytes):
    """Bytes occupied by everything under `mount`, rounded up to whole clusters.

    Rounding up matters in the safe direction: a file always costs whole
    clusters on disk, and under-counting usage would leave the guard thinking
    it has room it does not have. Unreadable entries are skipped rather than
    fatal, because the car is writing to this filesystem as we walk it.
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
    """
    subprocess.run(['sync'], check=False)
    try:
        with open('/proc/sys/vm/drop_caches', 'w') as caches:
            caches.write('3\n')
    except OSError as error:
        logger.error('could not drop page cache, readings may be stale: %s', error)


def free_bytes_at(mount):
    """Free bytes under a mounted cam image, counted from the files themselves.

    See the module docstring for why the filesystem's own answer is not used.
    """
    drop_page_cache()
    stat = os.statvfs(mount)
    capacity = stat.f_blocks * stat.f_frsize
    return max(0, capacity - used_bytes(mount, stat.f_frsize or 4096))


def free_bytes(image_path):
    """Free bytes on the cam image, measured through a throwaway mount."""
    probe = tempfile.mkdtemp(prefix='camfree-')
    subprocess.run(
        ['mount', '-o', 'ro,loop,noatime', image_path, probe],
        check=True, capture_output=True,
    )
    try:
        return free_bytes_at(probe)
    finally:
        # Loud on failure: this runs every five minutes, and a umount that
        # quietly fails leaks the loop device it created. A few hundred of
        # those a day is its own outage.
        result = subprocess.run(['umount', probe], capture_output=True, text=True)
        if result.returncode != 0:
            logger.error('could not unmount probe %s: %s', probe, result.stderr.strip())
        else:
            os.rmdir(probe)


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

    newest = max(at for at, _, _ in clips)
    cutoff = newest - KEEP_NEWEST_SECONDS

    chosen, freed = [], 0
    for at, path, size in sorted(clips):
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


def sweep(target_bytes):
    """Detach the drive, delete the oldest clips, hand it back. Returns bytes freed.

    The amount to delete is re-measured here rather than passed in, so a sweep
    acts on what the disk holds at the moment it is detached.
    """
    run_script('edit_usb.sh')
    try:
        mount = Path(MNT_DIR) / 'part1'
        free = free_bytes_at(mount)
        need = target_bytes - free
        logger.info('%.1f GB free with the drive detached', free / GIB)
        if need <= 0:
            logger.info('already above target once flushed, deleting nothing')
            return 0

        recent_dir = mount / 'TeslaCam' / 'RecentClips'
        if not recent_dir.is_dir():
            logger.warning('no RecentClips at %s, nothing to reclaim', recent_dir)
            return 0
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
        return freed
    finally:
        run_script('present_usb.sh')


def main():
    logging.basicConfig(level=logging.INFO, format='%(message)s')

    mode = Path(STATE_FILE).read_text().strip() if os.path.exists(STATE_FILE) else ''
    if mode != 'present':
        logger.info('mode is %r, not present: leaving the drive alone', mode or 'unset')
        return 0

    lock = open(LOCK_PATH, 'w')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        logger.info('another sweep is running')
        return 0

    free = free_bytes(IMG_CAM_PATH)
    if free >= FLOOR_BYTES:
        logger.debug('%.1f GB free, floor is %.1f GB', free / GIB, FLOOR_BYTES / GIB)
        return 0

    logger.info(
        '%.1f GB free is below the %.1f GB floor, reclaiming toward %.1f GB',
        free / GIB, FLOOR_BYTES / GIB, TARGET_BYTES / GIB,
    )
    sweep(TARGET_BYTES)
    logger.info('%.1f GB free after sweep', free_bytes(IMG_CAM_PATH) / GIB)
    return 0


if __name__ == '__main__':
    sys.exit(main())
