"""What the box takes off the card, under each of the three policies.

The numbers behind the default are in services/archive_policy.py: one
Sentry event is ~1.8 GB in full and ~435 KB as evidence, on a card with
7.8 GB free. Everything here is pure path arithmetic — no card, no DB.
"""

import pytest

from services import archive_policy

CARD = '/mnt/gadget/part1-ro/TeslaCam'
EVENT = f'{CARD}/SentryClips/2026-08-14_21-03-11'
SAVED = f'{CARD}/SavedClips/2026-08-14_09-00-00'
RING = f'{CARD}/RecentClips'

CLIP = f'{EVENT}/2026-08-14_21-03-11-front.mp4'
EVENT_JSON = f'{EVENT}/event.json'
EVENT_MP4 = f'{EVENT}/event.mp4'
THUMB = f'{EVENT}/thumb.png'
RING_CLIP = f'{RING}/2026-08-14_20-12-32-back.mp4'


class TestGroupOf:
    def test_finds_each_group(self):
        assert archive_policy.group_of(CLIP)[0] == 'SentryClips'
        assert archive_policy.group_of(f'{SAVED}/x.mp4')[0] == 'SavedClips'
        assert archive_policy.group_of(RING_CLIP)[0] == 'RecentClips'

    def test_reports_depth_below_the_group(self):
        assert archive_policy.group_of(CLIP)[1] == 2
        assert archive_policy.group_of(RING_CLIP)[1] == 1

    def test_a_path_outside_the_groups_is_none(self):
        assert archive_policy.group_of('/home/pi/ArchivedClips/a.mp4') is None
        assert archive_policy.group_of('') is None

    def test_matches_a_component_not_a_substring(self):
        # A clip NAMED after a group is a filename, not a location.
        # Substring matching here would archive it under the wrong rules.
        assert archive_policy.group_of('/home/pi/my-SentryClips-notes.mp4') is None

    def test_windows_separators_normalise(self):
        found = archive_policy.group_of(r'D:\TeslaCam\SentryClips\ev\a.mp4')
        assert found is not None and found[0] == 'SentryClips'


class TestInEventFolder:
    def test_a_clip_two_levels_down_is_in_an_event(self):
        assert archive_policy.in_event_folder(CLIP) is True
        assert archive_policy.in_event_folder(EVENT_JSON) is True

    def test_recentclips_is_never_an_event(self):
        assert archive_policy.in_event_folder(RING_CLIP) is False

    def test_a_file_at_the_group_root_is_not_in_an_event(self):
        assert archive_policy.in_event_folder(f'{CARD}/SentryClips/stray.mp4') is False


class TestEvidenceOnly:
    @pytest.mark.parametrize('path', [EVENT_JSON, EVENT_MP4, THUMB])
    def test_keeps_the_three_evidence_files(self, path):
        assert archive_policy.wanted(path, 'evidence-only') is True

    def test_drops_the_camera_clips(self):
        assert archive_policy.wanted(CLIP, 'evidence-only') is False

    def test_drops_the_whole_driving_ring(self):
        # 2096 of the 2148 queued rows on the box the day this landed.
        assert archive_policy.wanted(RING_CLIP, 'evidence-only') is False

    def test_keeps_saved_clip_evidence_too(self):
        assert archive_policy.wanted(f'{SAVED}/event.json', 'evidence-only') is True

    def test_an_event_json_outside_an_event_folder_is_not_evidence(self):
        # It describes no event; the name alone doesn't make it one.
        assert archive_policy.wanted(f'{RING}/event.json', 'evidence-only') is False


class TestEvents:
    def test_keeps_whole_event_folders(self):
        assert archive_policy.wanted(CLIP, 'events') is True
        assert archive_policy.wanted(EVENT_JSON, 'events') is True

    def test_still_drops_the_driving_ring(self):
        assert archive_policy.wanted(RING_CLIP, 'events') is False


class TestEverything:
    def test_keeps_the_ring_and_the_events(self):
        assert archive_policy.wanted(RING_CLIP, 'everything') is True
        assert archive_policy.wanted(CLIP, 'everything') is True
        assert archive_policy.wanted(EVENT_JSON, 'everything') is True

    def test_does_not_admit_stray_evidence_names_in_the_ring(self):
        # Parity with the pre-policy filter, which was ``.mp4``-only for
        # flat files. Admitting these would archive a file belonging to
        # no event.
        assert archive_policy.wanted(f'{RING}/thumb.png', 'everything') is False


class TestNonMedia:
    @pytest.mark.parametrize('policy', archive_policy.POLICIES)
    @pytest.mark.parametrize('name', ['clip.sei.json', 'notes.txt', 'a.MP4.part'])
    def test_nothing_else_is_ever_archived(self, policy, name):
        assert archive_policy.wanted(f'{EVENT}/{name}', policy) is False

    @pytest.mark.parametrize('policy', archive_policy.POLICIES)
    def test_uppercase_extensions_still_count(self, policy):
        assert archive_policy.wanted(f'{EVENT}/CLIP.MP4', policy) is (
            policy != 'evidence-only'
        )


class TestOutsideTheCard:
    @pytest.mark.parametrize('policy', archive_policy.POLICIES)
    def test_archived_clips_backfill_is_not_the_policys_business(self, policy):
        # The watcher also walks ArchivedClips itself for re-indexing.
        # Those files are already a keep decision somebody made; this
        # function only governs what comes OFF the card.
        assert archive_policy.wanted('/home/pi/ArchivedClips/x.mp4', policy) is True


class TestResolve:
    def test_reads_the_configured_value(self, monkeypatch):
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'events')
        assert archive_policy.resolve() == 'events'

    def test_case_and_whitespace_are_forgiven(self, monkeypatch):
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', '  EVENTS ')
        assert archive_policy.resolve() == 'events'

    def test_a_typo_falls_back_rather_than_raising(self, monkeypatch):
        # A misspelt policy must not stop the box archiving evidence,
        # and the fallback is the conservative-on-space choice.
        import config
        monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'evidince-only')
        assert archive_policy.resolve() == archive_policy.DEFAULT_POLICY

    def test_a_missing_config_falls_back(self, monkeypatch):
        import config
        monkeypatch.delattr(config, 'ARCHIVE_POLICY')
        assert archive_policy.resolve() == archive_policy.DEFAULT_POLICY

    def test_the_default_is_evidence_only(self):
        assert archive_policy.DEFAULT_POLICY == 'evidence-only'


class TestUnwantedReason:
    def test_names_the_group_for_a_ring_clip(self):
        reason = archive_policy.unwanted_reason(RING_CLIP, 'evidence-only')
        assert 'RecentClips' in reason and 'evidence-only' in reason

    def test_names_the_file_for_an_event_clip(self):
        reason = archive_policy.unwanted_reason(CLIP, 'evidence-only')
        assert '2026-08-14_21-03-11-front.mp4' in reason

    def test_says_so_when_it_was_never_archivable(self):
        assert 'not an archivable file' in archive_policy.unwanted_reason(
            f'{EVENT}/notes.txt', 'everything',
        )
