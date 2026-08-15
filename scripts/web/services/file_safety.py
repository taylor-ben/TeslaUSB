"""
File safety utilities for TeslaUSB.

Provides guards to prevent accidental deletion or overwriting of critical
files — most importantly the USB disk images (*.img) that Tesla records to.

Every code path that deletes files MUST call is_protected_file() first.

Two rules the owner stated on 2026-08-15 are enforced at the
:func:`safe_delete_archive_video` doorway rather than at each caller,
so a prune that forgets to ask still cannot break them:

1. **An event's evidence files are never deleted.** ``event.json``,
   ``event.mp4`` and ``thumb.png`` total ~435 KB per event against
   26-53 MB for one camera clip, so an evidence-only archive is
   effectively free and there is no capacity argument for reaching
   them.
2. **Nothing is deleted before it has been archived somewhere else.**
   Callers that have a second tier pass ``archived_check``; the
   doorway refuses with :data:`DeleteOutcome.UNARCHIVED` when the
   second copy is not there yet.

Note both rules live here and NOT in :func:`is_protected_file`. That
predicate also gates :func:`safe_rmtree`, which is how the video panel
deletes a whole event folder at the owner's explicit request — putting
the evidence rule there would make that button fail on every event.
"""

import logging
import os
from enum import Enum
from typing import Callable, NamedTuple, Optional

from services import tesla_clips

logger = logging.getLogger(__name__)

# Lazy-load GADGET_DIR to avoid circular imports at module level
_gadget_dir = None


def _get_gadget_dir():
    global _gadget_dir
    if _gadget_dir is None:
        from config import GADGET_DIR
        _gadget_dir = os.path.realpath(GADGET_DIR)
    return _gadget_dir


def is_protected_file(path: str) -> bool:
    """Check whether a file path is protected from deletion.

    Protected files:
    - Any ``*.img`` file inside GADGET_DIR (the USB disk images)

    Args:
        path: Absolute or relative path to check.

    Returns:
        True if the file MUST NOT be deleted/overwritten.
    """
    try:
        real = os.path.realpath(path)
    except (OSError, ValueError):
        return False

    # Protect *.img files in the gadget directory
    if real.lower().endswith(".img"):
        gadget = _get_gadget_dir()
        if real.startswith(gadget + os.sep) or real == gadget:
            logger.warning(
                "BLOCKED: attempt to delete/overwrite protected IMG file: %s",
                real,
            )
            return True

    return False


def safe_remove(path: str) -> bool:
    """Remove a file only if it is not protected.

    Args:
        path: File to remove.

    Returns:
        True if the file was removed, False if it was protected or missing.

    Raises:
        OSError: If removal fails for a reason other than file-not-found.
    """
    if is_protected_file(path):
        logger.error(
            "REFUSED to delete protected file: %s — IMG files must never be deleted",
            path,
        )
        return False
    try:
        os.remove(path)
        return True
    except FileNotFoundError:
        return False


class DeleteOutcome(Enum):
    """Result of :func:`safe_delete_archive_video`.

    Distinguishes the semantically-different "did not delete" cases so
    callers can react appropriately without re-probing the filesystem
    (which would also double-log the BLOCKED warning).

    Callers matching on specific members MUST have a final ``is not
    DELETED`` catch rather than an ``else`` that assumes success — an
    ``elif``-chain written against four members silently counted the
    fifth as a delete when UNARCHIVED was added.
    """

    DELETED = "deleted"      # File was removed from disk.
    PROTECTED = "protected"  # is_protected_file, or an event's evidence.
    MISSING = "missing"      # File didn't exist (FileNotFoundError).
    ERROR = "error"          # Other OSError (permissions, I/O, etc.).
    # No second copy yet — ``archived_check`` said this file exists on
    # exactly one tier. Deliberately its own value rather than folded
    # into PROTECTED: PROTECTED means "never", UNARCHIVED means "not
    # yet", and a prune that reports the two as one number cannot tell
    # an operator whether waiting will help.
    UNARCHIVED = "unarchived"


class DeleteResult(NamedTuple):
    """Outcome + bytes freed for :func:`safe_delete_archive_video`.

    ``outcome`` carries the delete verdict; ``bytes_freed`` is the size
    of the deleted file (only meaningful when outcome is DELETED, but
    will be 0 for an actually-deleted 0-byte file too — callers should
    use ``outcome is DeleteOutcome.DELETED`` for the boolean test, not
    ``bytes_freed > 0``).
    """

    outcome: "DeleteOutcome"
    bytes_freed: int


def has_archive_copy(path: str, source_root: str, archive_root: str) -> bool:
    """Whether the SD archive already holds a same-size copy of ``path``.

    ``path`` is a file on the USB image (under ``source_root``, e.g.
    ``/mnt/gadget/part1/TeslaCam``); the answer is whether the archive
    worker has finished copying it to ``archive_root``.

    Size, not mtime: the archive copy inherits the car's FAT timestamp
    through ``shutil.copystat`` so the two mtimes agree even on a
    half-written copy, whereas the worker only ``os.replace``s a
    ``.partial`` into place once the byte count matched. Existence at
    the right size is therefore the strongest cheap statement that the
    copy completed.

    The source → destination mapping is
    :func:`services.archive_worker.compute_dest_path` — imported at
    call time so the delete doorway stays importable from
    ``cam_headroom.py`` without dragging the worker's module state in
    on every run, and so neither module can create an import cycle.

    Returns False on any error: the caller's next move is "keep the
    file", which is the safe direction.
    """
    try:
        from services.archive_worker import compute_dest_path
        dest = compute_dest_path(path, archive_root, source_root)
        return os.path.getsize(dest) == os.path.getsize(path)
    except OSError:
        return False
    except Exception as e:  # noqa: BLE001
        logger.warning(
            "has_archive_copy: could not resolve archive copy of %s: %s",
            path, e,
        )
        return False


def safe_delete_archive_video(
    path: str,
    *,
    archived_check: Optional[Callable[[str], bool]] = None,
) -> DeleteResult:
    """The single doorway for deleting an archived video file.

    Every code path in TeslaUSB that deletes a clip from the local archive
    (retention prune, size trim, free-space trim, corrupt-file purge,
    non-driving prune, watchdog retention, manual cleanup, video-panel
    delete) MUST go through this function. Calling ``os.remove`` /
    ``os.unlink`` directly on archive files is a contract violation —
    past data-loss incidents were caused by a delete path that bypassed
    the protected-file check.

    The helper:

    * Refuses to delete any file flagged by :func:`is_protected_file`
      (currently: ``*.img`` files inside ``GADGET_DIR``) — returns
      ``DeleteOutcome.PROTECTED``.
    * Refuses to delete an event's evidence files
      (:data:`services.tesla_clips.EVIDENCE_NAMES`) — also
      ``DeleteOutcome.PROTECTED``. A backstop, not the primary
      mechanism: the prunes are supposed to leave those out of their
      candidate lists in the first place, and this is what catches
      the one that forgets.
    * Refuses when ``archived_check`` is supplied and returns False —
      ``DeleteOutcome.UNARCHIVED``. The predicate answers "does a copy
      of this file exist on the next tier down": ``has_archive_copy``
      for card → SD archive, ``archive_watchdog._is_synced_to_cloud``
      for SD archive → cloud. Callers with no second tier (the video
      panel's explicit per-file delete) pass nothing and keep the old
      behavior.
    * Reads the file size BEFORE removing so the caller can update its
      bytes-freed accounting.
    * Returns ``DeleteOutcome.MISSING`` for ``FileNotFoundError`` so
      loops over many candidate files don't blow up on transient races.
    * Returns ``DeleteOutcome.ERROR`` (with a WARNING log) for other
      ``OSError`` so callers can surface "skipped (unwritable)" without
      re-probing the filesystem.

    Geodata reconciliation (``mapping_service.purge_deleted_videos``)
    is intentionally NOT done here — it would create a circular-import
    risk and the May 7 contract requires the caller to control which
    rows get NULL'd. Callers that hold a list of
    successfully-deleted paths should call ``purge_deleted_videos``
    themselves after the loop finishes.

    Args:
        path: Absolute path to the archived video to delete.
        archived_check: Optional predicate answering "does a copy of
            this file exist on the next tier down". Omit when the
            caller has no second tier.

    Returns:
        :class:`DeleteResult` with ``outcome`` and ``bytes_freed``.
        ``bytes_freed`` is the file size at the moment of deletion;
        ``0`` for any non-DELETED outcome (and also for a real 0-byte
        delete, so use ``outcome is DeleteOutcome.DELETED`` for the
        boolean test).
    """
    if is_protected_file(path):
        return DeleteResult(DeleteOutcome.PROTECTED, 0)
    if tesla_clips.is_evidence_file(path):
        logger.warning(
            "REFUSED to delete event evidence file: %s — the three "
            "evidence files are ~435 KB per event and are the only "
            "record of why the car triggered",
            path,
        )
        return DeleteResult(DeleteOutcome.PROTECTED, 0)
    if archived_check is not None and not archived_check(path):
        logger.warning(
            "REFUSED to delete %s — it has not been archived to the "
            "next tier yet, so this is its only copy",
            path,
        )
        return DeleteResult(DeleteOutcome.UNARCHIVED, 0)
    try:
        size = os.path.getsize(path)
    except FileNotFoundError:
        return DeleteResult(DeleteOutcome.MISSING, 0)
    except OSError as e:
        logger.warning(
            "safe_delete_archive_video: stat failed for %s: %s", path, e,
        )
        return DeleteResult(DeleteOutcome.ERROR, 0)
    try:
        os.remove(path)
    except FileNotFoundError:
        return DeleteResult(DeleteOutcome.MISSING, 0)
    except OSError as e:
        logger.warning(
            "safe_delete_archive_video: failed to remove %s: %s", path, e,
        )
        return DeleteResult(DeleteOutcome.ERROR, 0)
    return DeleteResult(DeleteOutcome.DELETED, int(size))


def safe_rmtree(path: str) -> bool:
    """Remove a directory tree only if it contains no protected files.

    Scans the tree first; if ANY protected file is found the entire
    operation is refused.

    Args:
        path: Directory to remove.

    Returns:
        True if removed, False if refused or missing.
    """
    import shutil

    if not os.path.isdir(path):
        return False

    # Scan for protected files before removing anything
    for dirpath, _dirnames, filenames in os.walk(path):
        for fname in filenames:
            full = os.path.join(dirpath, fname)
            if is_protected_file(full):
                logger.error(
                    "REFUSED to rmtree %s — contains protected file: %s",
                    path,
                    full,
                )
                return False

    shutil.rmtree(path)
    return True
