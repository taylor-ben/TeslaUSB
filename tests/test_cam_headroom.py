"""Headroom guard for the cam image.

A full cam image does not degrade gracefully: the car stops recording
altogether, so there is no dashcam footage and no SentryClips folder for a
Sentry event. Boot cleanup cannot prevent it, because its policy protects
everything younger than an hour and Tesla's rolling buffer is only about an
hour long. This guard trims that buffer mid-session instead.

Contract pinned here:

0. Usage is counted from the files themselves, never from the filesystem's own
   free-space answer. vfat serves that from the FSINFO hint sector, which the
   car does not maintain while it holds the drive: it wrote 2.7 GB and statvfs
   reported an unchanged 8.04 GB free against a true 5.06 GB (2026-08-08). A
   guard watching that number never fires.
1. Clips order by the timestamp in their FILENAME. The car writes FAT
   timestamps in its own timezone and the kernel reinterprets them, so mtime
   orders clips wrongly (a 20:12 recording stats as 11:13).
2. The newest ``KEEP_NEWEST_SECONDS`` are never deleted, even when that means
   failing to reach the requested figure.
3. Deletion stops as soon as enough bytes are covered.
4. Anything that is not a Tesla clip filename is left alone.
5. Filename timestamps are real epoch seconds, so the keep window is a real
   duration. A digit-packing scheme orders correctly but subtracts wrongly
   across a month end, which would delete the previous evening's footage.
6. A corrupt filename cannot drag the keep window off the end of the ring and
   take the newest minutes with it.
"""
from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'scripts'))

import cam_headroom
from cam_headroom import (
    KEEP_NEWEST_SECONDS,
    clip_seconds,
    clips_to_delete,
    used_bytes,
)

CAMERAS = ('front', 'back', 'left_pillar')


def write_clip(folder, stamp, camera, size):
    path = folder / f'{stamp}-{camera}.mp4'
    path.write_bytes(b'\0' * size)
    return path


def test_clip_seconds_orders_by_name_not_mtime(tmp_path):
    older = write_clip(tmp_path, '2026-08-08_16-50-00', 'front', 10)
    newer = write_clip(tmp_path, '2026-08-08_20-12-32', 'front', 10)
    # Backwards on disk: this is exactly what the vfat timestamps do.
    os.utime(older, (2_000_000_000, 2_000_000_000))
    os.utime(newer, (1_000_000_000, 1_000_000_000))

    assert clip_seconds(older.name) < clip_seconds(newer.name)


def test_clip_seconds_spans_midnight_and_month_end():
    assert clip_seconds('2026-08-08_23-59-59-front.mp4') < clip_seconds(
        '2026-08-09_00-00-01-front.mp4'
    )
    assert clip_seconds('2026-08-31_23-00-00-front.mp4') < clip_seconds(
        '2026-09-01_01-00-00-front.mp4'
    )


def test_clip_seconds_ignores_other_files():
    assert clip_seconds('thumb.png') is None
    assert clip_seconds('event.json') is None
    assert clip_seconds('2026-08-08_16-50-00-front.mp4.tmp') is None


def test_clip_seconds_measures_real_durations_across_a_month_end():
    # The bug this pins: a `month * 31 + day` clock puts these a day apart, so
    # every clip from the 30th falls outside the keep window on the 1st and the
    # sweep deletes footage the car wrote eight minutes ago.
    late = clip_seconds('2026-09-30_23-55-00-front.mp4')
    early = clip_seconds('2026-10-01_00-03-00-front.mp4')
    assert early - late == 8 * 60


def test_clip_seconds_measures_real_durations_across_a_year_end():
    assert (
        clip_seconds('2027-01-01_00-01-00-front.mp4')
        - clip_seconds('2026-12-31_23-59-00-front.mp4')
    ) == 2 * 60


def test_clip_seconds_rejects_impossible_dates():
    # A car-yanked vfat can leave garbage in a directory entry. Parsing it as a
    # date in the year 9999 is what drags the keep window off the ring.
    assert clip_seconds('9999-99-99_99-99-99-front.mp4') is None
    assert clip_seconds('2026-13-01_00-00-00-front.mp4') is None
    assert clip_seconds('2026-08-08_24-00-00-front.mp4') is None
    assert clip_seconds('2026-08-00_10-00-00-front.mp4') is None


def test_deletes_oldest_first_and_stops_once_covered(tmp_path):
    for minute in range(0, 60, 10):
        for camera in CAMERAS:
            write_clip(tmp_path, f'2026-08-08_16-{minute:02d}-00', camera, 100)

    paths, freed = clips_to_delete(tmp_path, 250)

    assert freed >= 250
    # Three 100-byte clips cover 250, and they are the 16:00 group.
    assert len(paths) == 3
    assert all('16-00-00' in os.path.basename(path) for path in paths)


def test_a_corrupt_filename_cannot_drag_the_keep_window_off_the_ring(tmp_path):
    # One entry dated years ahead used to make every real clip look ancient,
    # and the sweep took the whole ring including the minute just recorded.
    for minute in (0, 30, 50):
        for camera in CAMERAS:
            write_clip(tmp_path, f'2026-08-08_16-{minute:02d}-00', camera, 100)
    write_clip(tmp_path, '2031-01-01_00-00-00', 'front', 100)

    paths, _ = clips_to_delete(tmp_path, 10 ** 9)

    kept = set(os.listdir(tmp_path)) - {os.path.basename(p) for p in paths}
    assert any('16-50-00' in name for name in kept), 'newest real clips must survive'


def test_an_orphan_batch_behind_a_gap_cannot_brick_the_sweep(tmp_path):
    # The 2026-08-19 deadlock: a few clips survived the 08-17 SD repair, the
    # car came back nine days later, and with the horizon anchored on the
    # OLDEST clip everything the car wrote since looked corrupt. The sweep
    # deleted nothing, forever, while the drive filled. The orphans must be
    # the first thing reclaimed and the live ring must stay deletable.
    for minute in (27, 30, 33):
        for camera in CAMERAS:
            write_clip(tmp_path, f'2026-08-09_19-{minute:02d}-00', camera, 100)
    for minute in (0, 30, 50):
        for camera in CAMERAS:
            write_clip(tmp_path, f'2026-08-19_16-{minute:02d}-00', camera, 100)

    paths, freed = clips_to_delete(tmp_path, 10 ** 9)

    names = [os.path.basename(p) for p in paths]
    assert all(name.startswith('2026-08-09') for name in names[:9]), (
        'orphans go first'
    )
    assert any(name.startswith('2026-08-19_16-00') for name in names), (
        'the live ring outside the keep window must be deletable too'
    )
    assert freed == 1500
    kept = set(os.listdir(tmp_path)) - set(names)
    assert all('16-50-00' in name for name in kept), 'keep window survives'


def test_never_deletes_inside_the_keep_window(tmp_path):
    newest = 20 * 3600
    for offset in (0, KEEP_NEWEST_SECONDS // 2, KEEP_NEWEST_SECONDS + 60):
        at = newest - offset
        stamp = f'2026-08-08_{at // 3600:02d}-{(at % 3600) // 60:02d}-00'
        for camera in CAMERAS:
            write_clip(tmp_path, stamp, camera, 100)

    # Ask for far more than exists, so only the keep window can hold it back.
    paths, freed = clips_to_delete(tmp_path, 10 ** 9)

    kept = {os.path.basename(p) for p in os.listdir(tmp_path)} - {
        os.path.basename(p) for p in paths
    }
    assert len(paths) == 3, 'only the group outside the keep window is deletable'
    assert freed == 300
    assert len(kept) == 6


def test_reports_shortfall_rather_than_eating_the_keep_window(tmp_path):
    for camera in CAMERAS:
        write_clip(tmp_path, '2026-08-08_16-00-00', camera, 100)
        write_clip(tmp_path, '2026-08-08_20-00-00', camera, 100)

    paths, freed = clips_to_delete(tmp_path, 10_000)

    assert freed == 300 and len(paths) == 3
    assert freed < 10_000, 'caller must see the shortfall, not a false success'


def test_leaves_non_clip_files_alone(tmp_path):
    write_clip(tmp_path, '2026-08-08_16-00-00', 'front', 100)
    (tmp_path / 'thumb.png').write_bytes(b'\0' * 5000)
    (tmp_path / 'event.json').write_text('{}')

    paths, _ = clips_to_delete(tmp_path, 10 ** 9)

    assert all(path.endswith('.mp4') for path in paths)
    assert (tmp_path / 'thumb.png').exists()


def test_empty_folder_is_not_an_error(tmp_path):
    assert clips_to_delete(tmp_path, 10 ** 9) == ([], 0)


def test_used_bytes_rounds_up_to_whole_clusters(tmp_path):
    # A file always costs whole clusters on disk. Rounding down would make the
    # guard believe in room that is not there.
    (tmp_path / 'a.mp4').write_bytes(b'\0' * 100)
    (tmp_path / 'b.mp4').write_bytes(b'\0' * 4097)

    assert used_bytes(tmp_path, 4096) == 4096 + 8192


def test_used_bytes_counts_nested_event_folders(tmp_path):
    (tmp_path / 'TeslaCam' / 'SentryClips' / '2026-08-08_16-21-54').mkdir(parents=True)
    (tmp_path / 'TeslaCam' / 'RecentClips').mkdir(parents=True)
    (tmp_path / 'TeslaCam' / 'SentryClips' / '2026-08-08_16-21-54' / 'f.mp4').write_bytes(
        b'\0' * 4096
    )
    (tmp_path / 'TeslaCam' / 'RecentClips' / 'g.mp4').write_bytes(b'\0' * 4096)

    assert used_bytes(tmp_path, 4096) == 8192


def test_used_bytes_survives_files_vanishing_mid_walk(tmp_path, monkeypatch):
    # The car is writing to this filesystem while we walk it.
    (tmp_path / 'a.mp4').write_bytes(b'\0' * 4096)
    (tmp_path / 'gone.mp4').write_bytes(b'\0' * 4096)

    real_getsize = os.path.getsize

    def flaky(path):
        if path.endswith('gone.mp4'):
            raise OSError('vanished')
        return real_getsize(path)

    monkeypatch.setattr(os.path, 'getsize', flaky)
    assert used_bytes(tmp_path, 4096) == 4096


def test_floor_leaves_room_for_an_event_landing_all_at_once():
    # A Sentry event folder measured 1.8 GB on 2026-08-08 and a timer interval
    # of recording is ~900 MB at the measured 180 MB/min. The floor has to
    # absorb both between two ticks, or the guard fires after the disk is full.
    assert cam_headroom.FLOOR_BYTES < cam_headroom.TARGET_BYTES
    assert cam_headroom.FLOOR_BYTES >= 2.7 * cam_headroom.GIB


# ---------------------------------------------------------------------------
# 7. A clip the SD archive already holds goes before one that exists nowhere
#    else, but the floor still wins: a full image stops the car recording
#    entirely, so an unarchived clip IS taken when the archived ones do not
#    cover the shortfall, and the log says how many and why.
# ---------------------------------------------------------------------------


def _tiers(tmp_path, monkeypatch):
    """Wire cam_headroom at a tmp card mount + tmp SD archive."""
    teslacam = tmp_path / 'part1' / 'TeslaCam'
    recent = teslacam / 'RecentClips'
    recent.mkdir(parents=True)
    archive = tmp_path / 'ArchivedClips'
    archive.mkdir()
    monkeypatch.setattr(cam_headroom, 'ARCHIVE_ENABLED', True)
    monkeypatch.setattr(cam_headroom, 'ARCHIVE_DIR', str(archive))
    return teslacam, recent, archive


def test_split_puts_clips_the_archive_holds_first(tmp_path, monkeypatch):
    teslacam, recent, archive = _tiers(tmp_path, monkeypatch)
    copied = write_clip(recent, '2026-08-08_16-50-00', 'front', 4096)
    only_here = write_clip(recent, '2026-08-08_16-51-00', 'front', 4096)
    (archive / 'RecentClips').mkdir()
    (archive / 'RecentClips' / copied.name).write_bytes(b'\0' * 4096)

    archived, pending = cam_headroom.split_by_archive_copy(
        [str(copied), str(only_here)], str(teslacam),
    )

    assert archived == [str(copied)]
    assert pending == [str(only_here)]


def test_split_treats_a_short_archive_copy_as_missing(tmp_path, monkeypatch):
    teslacam, recent, archive = _tiers(tmp_path, monkeypatch)
    clip = write_clip(recent, '2026-08-08_16-50-00', 'front', 4096)
    (archive / 'RecentClips').mkdir()
    (archive / 'RecentClips' / clip.name).write_bytes(b'\0' * 100)

    archived, pending = cam_headroom.split_by_archive_copy(
        [str(clip)], str(teslacam),
    )

    assert archived == []
    assert pending == [str(clip)]


def test_split_counts_everything_archived_when_archiving_is_off(
    tmp_path, monkeypatch,
):
    # With no second tier there is nothing to wait for, and waiting
    # forever would let the image fill.
    teslacam, recent, _archive = _tiers(tmp_path, monkeypatch)
    monkeypatch.setattr(cam_headroom, 'ARCHIVE_ENABLED', False)
    clip = write_clip(recent, '2026-08-08_16-50-00', 'front', 4096)

    archived, pending = cam_headroom.split_by_archive_copy(
        [str(clip)], str(teslacam),
    )

    assert archived == [str(clip)]
    assert pending == []


def test_delete_clips_stops_once_the_need_is_covered(tmp_path, monkeypatch):
    _teslacam, recent, _archive = _tiers(tmp_path, monkeypatch)
    first = write_clip(recent, '2026-08-08_16-50-00', 'front', 4096)
    second = write_clip(recent, '2026-08-08_16-51-00', 'front', 4096)

    count, freed = cam_headroom.delete_clips([str(first), str(second)], 4096)

    assert (count, freed) == (1, 4096)
    assert not first.exists()
    assert second.exists()


def test_delete_clips_refuses_an_evidence_file(tmp_path, monkeypatch):
    # RecentClips holds none, but the doorway is the doorway.
    _teslacam, recent, _archive = _tiers(tmp_path, monkeypatch)
    evidence = recent / 'event.json'
    evidence.write_bytes(b'{}')

    count, freed = cam_headroom.delete_clips([str(evidence)], 1024)

    assert (count, freed) == (0, 0)
    assert evidence.exists()
