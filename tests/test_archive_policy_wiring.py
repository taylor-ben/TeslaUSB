"""The archive policy where it meets the queue, the worker and the gate.

Three properties, each of which cost the live box something on
2026-08-16:

* the policy is applied at every entrance, so the producers and the
  worker cannot disagree about what this box keeps;
* a policy change retires the backlog behind it in one UPDATE rather
  than 2096 claim-and-mark cycles;
* no throttle may stop the archive indefinitely — the whole rewrite
  came from a gate that latched shut for days.
"""

import os
import time

import pytest

from services import archive_policy
from services import archive_producer
from services import archive_queue
from services import archive_worker
from services.archive_queue import (
    claim_next_for_worker,
    enqueue_for_archive,
    list_queue,
)
from services.mapping_service import _init_db


@pytest.fixture
def db(tmp_path):
    db_path = str(tmp_path / "geodata.db")
    _init_db(db_path).close()
    return db_path


@pytest.fixture
def archive_root(tmp_path):
    root = tmp_path / "ArchivedClips"
    root.mkdir()
    return str(root)


@pytest.fixture
def card(tmp_path):
    root = tmp_path / "TeslaCam"
    for group in archive_policy.GROUPS:
        (root / group).mkdir(parents=True)
    return root


@pytest.fixture
def evidence_only(monkeypatch):
    """Undo conftest's ``everything`` default for this module's subject."""
    import config
    monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'evidence-only')


def _write(path, body=b"x" * 64):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'wb') as handle:
        handle.write(body)
    old = time.time() - 600
    os.utime(path, (old, old))
    return path


def _an_event(card, stamp="2026-08-14_21-03-11"):
    """One SentryClips event as the car writes it: six clips + evidence."""
    folder = os.path.join(str(card), 'SentryClips', stamp)
    written = {
        'clips': [
            _write(os.path.join(folder, f'{stamp}-{angle}.mp4'))
            for angle in ('front', 'back', 'left_repeater', 'right_repeater')
        ],
        'evidence': [
            _write(os.path.join(folder, name))
            for name in archive_policy.evidence_names()
        ],
    }
    return folder, written


class TestProducerFilter:
    def test_evidence_only_collects_the_three_files_and_no_clips(
        self, card, evidence_only,
    ):
        _folder, written = _an_event(card)
        _write(os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'))

        found = archive_producer._iter_archive_candidates(str(card))

        assert sorted(found) == sorted(written['evidence'])

    def test_everything_still_collects_the_ring(self, card, monkeypatch):
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'everything')
        _folder, written = _an_event(card)
        ring = _write(
            os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'),
        )

        found = archive_producer._iter_archive_candidates(str(card))

        assert ring in found
        assert set(written['clips']) <= set(found)

    def test_events_keeps_the_event_whole_but_drops_the_ring(
        self, card, monkeypatch,
    ):
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'events')
        _folder, written = _an_event(card)
        ring = _write(
            os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'),
        )

        found = archive_producer._iter_archive_candidates(str(card))

        assert ring not in found
        assert set(written['clips'] + written['evidence']) == set(found)


class TestWorkerGate:
    def test_a_ring_clip_already_queued_is_retired_not_copied(
        self, db, card, archive_root, evidence_only,
    ):
        # The row predates the policy — enqueued while the box was
        # still archiving everything.
        ring = _write(
            os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'),
        )
        enqueue_for_archive(ring, db_path=db)
        row = claim_next_for_worker('w', db_path=db)

        outcome = archive_worker.process_one_claim(
            row, db, archive_root, str(card), chunk_size=4096, max_attempts=3,
        )

        assert outcome == 'skipped_policy'
        assert list_queue(db_path=db)[0]['status'] == 'skipped_policy'
        assert os.listdir(archive_root) == [], "nothing should have been copied"

    def test_the_reason_survives_into_the_row(
        self, db, card, archive_root, evidence_only,
    ):
        # Whoever finds this row in six months should not have to guess.
        ring = _write(
            os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'),
        )
        enqueue_for_archive(ring, db_path=db)
        row = claim_next_for_worker('w', db_path=db)
        archive_worker.process_one_claim(
            row, db, archive_root, str(card), chunk_size=4096, max_attempts=3,
        )

        stored = list_queue(db_path=db)[0]['last_error']
        assert 'RecentClips' in stored and 'evidence-only' in stored

    def test_evidence_is_copied_normally(
        self, db, card, archive_root, evidence_only,
    ):
        _folder, written = _an_event(card)
        target = [p for p in written['evidence'] if p.endswith('event.json')][0]
        enqueue_for_archive(target, db_path=db)
        row = claim_next_for_worker('w', db_path=db)

        outcome = archive_worker.process_one_claim(
            row, db, archive_root, str(card), chunk_size=4096, max_attempts=3,
        )

        assert outcome == 'copied'

    def test_the_gate_runs_before_the_stat(
        self, db, card, archive_root, evidence_only, monkeypatch,
    ):
        # Cheapest verdict first: a path the policy rejects must not
        # cost an SD read, which on this box is the scarce resource.
        ring = os.path.join(str(card), 'RecentClips', 'gone-back.mp4')
        enqueue_for_archive(ring, db_path=db)
        row = claim_next_for_worker('w', db_path=db)

        def _no_stat(*_args, **_kwargs):
            raise AssertionError("policy gate must decide before stat()")

        monkeypatch.setattr(archive_worker, '_safe_stat', _no_stat)
        outcome = archive_worker.process_one_claim(
            row, db, archive_root, str(card), chunk_size=4096, max_attempts=3,
        )

        assert outcome == 'skipped_policy'


class TestBacklogSweep:
    def test_retires_the_ring_and_leaves_the_evidence(
        self, db, card, evidence_only,
    ):
        _folder, written = _an_event(card)
        ring = [
            _write(os.path.join(str(card), 'RecentClips', f'2026-08-14_20-{n:02d}-32-back.mp4'))
            for n in range(10, 15)
        ]
        for path in ring + written['clips'] + written['evidence']:
            enqueue_for_archive(path, db_path=db)

        retired = archive_worker._retire_backlog_the_policy_rejects(db)

        assert retired == len(ring) + len(written['clips'])
        survivors = [
            row['source_path'] for row in list_queue(db_path=db)
            if row['status'] == 'pending'
        ]
        assert sorted(survivors) == sorted(written['evidence'])

    def test_a_claimed_row_is_left_for_its_worker(self, db, card, evidence_only):
        # Another worker may be mid-copy on it; it will reach the same
        # verdict when it finishes.
        ring = _write(
            os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'),
        )
        enqueue_for_archive(ring, db_path=db)
        claim_next_for_worker('other-worker', db_path=db)

        assert archive_worker._retire_backlog_the_policy_rejects(db) == 0
        assert list_queue(db_path=db)[0]['status'] == 'claimed'

    def test_nothing_to_do_under_everything(self, db, card, monkeypatch):
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'everything')
        ring = _write(
            os.path.join(str(card), 'RecentClips', '2026-08-14_20-12-32-back.mp4'),
        )
        enqueue_for_archive(ring, db_path=db)

        assert archive_worker._retire_backlog_the_policy_rejects(db) == 0
        assert list_queue(db_path=db)[0]['status'] == 'pending'

    def test_a_broken_database_does_not_stop_the_worker_starting(
        self, card, evidence_only,
    ):
        # Non-fatal by construction: the per-row gate retires them one
        # at a time instead.
        assert archive_worker._retire_backlog_the_policy_rejects(
            '/nonexistent/dir/geodata.db',
        ) == 0


class TestForwardProgressFloor:
    """No gate may stop the archive indefinitely. This is the property
    the 2026-08-16 rewrite exists to guarantee: the old loadavg gate
    could latch shut, and on the live box it did, for days."""

    def teardown_method(self):
        archive_worker._clear_gate()

    def test_a_gate_that_never_opens_still_lets_work_through(self, monkeypatch):
        monkeypatch.setattr(archive_worker, '_max_stall_seconds', lambda: 60.0)
        archive_worker._clear_gate()
        archive_worker._note_gated()
        assert archive_worker._stalled_too_long() is False

        # 61 seconds of continuous gating later.
        archive_worker._gated_since = time.time() - 61.0
        assert archive_worker._stalled_too_long() is True

    def test_the_clock_only_runs_while_the_gate_holds(self, monkeypatch):
        monkeypatch.setattr(archive_worker, '_max_stall_seconds', lambda: 60.0)
        archive_worker._note_gated()
        archive_worker._gated_since = time.time() - 61.0
        archive_worker._clear_gate()

        # An idle queue is not a stall; only a gate that holds is.
        assert archive_worker._seconds_gated() == 0.0
        assert archive_worker._stalled_too_long() is False

    def test_noting_twice_does_not_restart_the_clock(self, monkeypatch):
        # Otherwise every iteration inside a sustained pause would reset
        # the timer and the floor could never be reached — the same
        # shape of bug in a different place.
        monkeypatch.setattr(archive_worker, '_max_stall_seconds', lambda: 60.0)
        archive_worker._note_gated()
        archive_worker._gated_since = time.time() - 61.0
        archive_worker._note_gated()

        assert archive_worker._stalled_too_long() is True

    def test_a_zero_limit_disables_the_floor(self, monkeypatch):
        monkeypatch.setattr(archive_worker, '_max_stall_seconds', lambda: 0.0)
        archive_worker._note_gated()
        archive_worker._gated_since = time.time() - 10_000.0

        assert archive_worker._stalled_too_long() is False

    def test_the_loop_forces_a_copy_through_a_permanently_shut_gate(
        self, db, archive_root, card, monkeypatch, caplog,
    ):
        # The end-to-end version: a box reporting 0% idle CPU forever.
        # Under the old gate this queue never drained. It must now.
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'everything')
        monkeypatch.setattr(archive_worker, '_sample_cpu_free', lambda: 0.0)
        monkeypatch.setattr(archive_worker, '_max_stall_seconds', lambda: 0.4)

        def fake_config(*_a, **_kw):
            return (4096, 3, 0.05, 0.05, 0.5, 0.2, 0.0, 0.0)
        monkeypatch.setattr(
            archive_worker, '_read_config_or_defaults', fake_config,
        )

        # A Sentry event clip, not a RecentClips one: the SEI
        # stationary peek only applies to the ring, and this test is
        # about the gate, not about that.
        clip = _write(
            os.path.join(str(card), 'SentryClips', '2026-08-14_21-03-11',
                         '2026-08-14_21-03-11-front.mp4'),
            _minimal_mp4(),
        )
        enqueue_for_archive(clip, db_path=db)

        with caplog.at_level('WARNING', logger='services.archive_worker'):
            archive_worker.start_worker(
                db, archive_root, teslacam_root=str(card),
            )
            deadline = time.time() + 8.0
            while time.time() < deadline:
                if list_queue(db_path=db)[0]['status'] == 'copied':
                    break
                time.sleep(0.1)
            archive_worker.stop_worker(timeout=5)

        assert list_queue(db_path=db)[0]['status'] == 'copied', (
            "a permanently-shut CPU gate stopped the archive dead — that "
            "is the bug the forward-progress floor exists to prevent"
        )
        assert any('forcing one copy through' in record.getMessage()
                   for record in caplog.records), (
            "the box must say out loud that it overrode its own throttle"
        )


def _minimal_mp4(payload: bytes = b"\x00" * 32) -> bytes:
    """ftyp + moov + mdat, so the copy passes moov verification."""
    def box(kind: bytes, body: bytes) -> bytes:
        return (len(body) + 8).to_bytes(4, 'big') + kind + body

    return (
        box(b'ftyp', b'isom' + b'\x00\x00\x02\x00' + b'isomiso2avc1mp41')
        + box(b'moov', b'')
        + box(b'mdat', payload)
    )


class TestWatcherDispatch:
    """The policy applies to the card half of a batch, and only that.

    It was applied at discovery first, which was wrong in a way the unit
    tests could not see: an archive path repeats the card's
    ``SentryClips/<event>/`` shape, so filtering before
    ``_classify_paths`` judged already-archived clips by a rule about
    what to take OFF the card — and the indexer went blind to them.
    """

    @pytest.fixture
    def watcher(self, monkeypatch, tmp_path):
        from services import file_watcher_service as fws

        card = tmp_path / 'ro' / 'TeslaCam'
        archive = tmp_path / 'ArchivedClips'
        (card / 'SentryClips' / 'ev').mkdir(parents=True)
        (archive / 'SentryClips' / 'ev').mkdir(parents=True)

        monkeypatch.setattr(fws, '_ro_mount_prefixes',
                            lambda: [os.path.normpath(str(tmp_path / 'ro'))])
        monkeypatch.setattr(fws, '_archive_dir_prefix',
                            lambda: os.path.normpath(str(archive)))
        seen = {'archive': [], 'indexing': []}
        monkeypatch.setattr(fws, '_on_archive_callbacks',
                            [lambda paths: seen['archive'].extend(paths)])
        monkeypatch.setattr(fws, '_on_new_file_callbacks',
                            [lambda paths: seen['indexing'].extend(paths)])
        return fws, card, archive, seen

    def test_a_card_clip_is_declined_but_its_evidence_is_not(
        self, watcher, evidence_only,
    ):
        fws, card, _archive, seen = watcher
        clip = str(card / 'SentryClips' / 'ev' / '2026-08-14_21-03-11-front.mp4')
        evidence = str(card / 'SentryClips' / 'ev' / 'event.json')

        fws._notify_callbacks([clip, evidence], fws._watcher_generation)

        assert seen['archive'] == [evidence]

    def test_an_already_archived_clip_still_reaches_the_indexer(
        self, watcher, evidence_only,
    ):
        # The regression the move exists to prevent. This path is under
        # ArchivedClips and repeats the card's event-folder shape.
        fws, _card, archive, seen = watcher
        archived = str(archive / 'SentryClips' / 'ev' / '2026-08-14_21-03-11-front.mp4')

        fws._notify_callbacks([archived], fws._watcher_generation)

        assert seen['indexing'] == [archived]
        assert seen['archive'] == []

    def test_everything_declines_nothing_from_the_card(
        self, watcher, monkeypatch,
    ):
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'everything')
        fws, card, _archive, seen = watcher
        ring = str(card / 'RecentClips' / '2026-08-14_20-12-32-back.mp4')

        fws._notify_callbacks([ring], fws._watcher_generation)

        assert seen['archive'] == [ring]
