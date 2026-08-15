"""Facts about the files Tesla writes into TeslaCam, in one place.

Two of them are load-bearing in more than one service, and both had
already been re-derived — or quietly ignored — somewhere else:

* **An event folder holds three evidence files** alongside the camera
  clips. ``event.json`` (~200 B: trigger time, reason, camera, GPS),
  ``event.mp4`` (~400 KB, the car's own clip of the triggering camera)
  and ``thumb.png`` (~20 KB). Measured on the live box 2026-08-16: the
  three together are ~435 KB per event, against 26-53 MB for one
  camera clip. Keeping every one of them forever is free next to a
  single clip, and they are the only part of an event that says WHY
  the car triggered — which is why the producer and the watcher admit
  them to the archive queue and why nothing prunes them.

* **A clip's recording time is in its FILENAME, never its mtime.** The
  car writes FAT timestamps in its own timezone and the kernel
  reinterprets them, so a 20:12 recording stats as 11:13; the archive
  copy then inherits that wrong value through ``shutil.copystat``.
  Ordering or cutting off by ``st_mtime`` is therefore not
  chronological on either tier, which matters because every prune in
  the repo deletes "oldest first".

Deliberately free of ``config`` and of any other service import, so
``cam_headroom.py`` (a standalone systemd script) and the web
services can both read these facts without dragging a dependency
tree behind them.
"""

import calendar
import os
import re

# The three small files Tesla drops next to the camera clips in a
# SentryClips / SavedClips event folder. Named here once so the queue
# producers, the archive worker, the prunes and the delete doorway all
# agree on what "evidence" means — before this constant existed the
# ``.mp4`` filter in the producer and the watcher silently dropped
# ``event.json`` and ``thumb.png`` on the floor, and the archive on the
# box held 0 of 22 available event.json files (measured 2026-08-16).
EVIDENCE_NAMES = ('event.json', 'event.mp4', 'thumb.png')

# 2026-08-08_20-12-32-front.mp4
CLIP_NAME = re.compile(r'^(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})-.+\.mp4$')


def is_evidence_file(path):
    """Whether ``path`` names one of an event folder's evidence files.

    Basename match only: the same three names appear once per event
    folder and nowhere else, so no directory context is needed.
    """
    return os.path.basename(path) in EVIDENCE_NAMES


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


def recorded_seconds(path, mtime):
    """When the car recorded ``path``, from its name, falling back to ``mtime``.

    The fallback is not a formality: an event folder also holds the
    evidence files and the indexer's ``.sei.json`` sidecars, none of
    which carry a timestamp in the name, and a hand-dropped test
    fixture may not either. Those have no better answer than the mtime
    they were written with, and mtime on a Pi-written file IS correct —
    only the car's own FAT timestamps are the ones that lie.
    """
    parsed = clip_seconds(os.path.basename(path))
    return mtime if parsed is None else parsed
