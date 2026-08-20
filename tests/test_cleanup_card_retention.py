"""The two ways card-side retention silently died, and their fixes.

Found together on 2026-08-20, diagnosing a car that reported its dashcam
drive full:

1. The Phase 3a.2 startup migration renames ``cleanup_config.json`` to
   ``.migrated`` after importing it into config.yaml — but
   ``CleanupService`` kept reading only the JSON, so boot cleanup loaded
   zero policies from the day the migration first ran.
2. Under ``archive_policy: evidence-only`` no camera clip ever gains an
   SD-archive copy, so the "never delete the only copy" guard in
   ``_is_protected`` vetoed every candidate forever and event folders
   accumulated until the image filled.
"""

from __future__ import annotations

import yaml

import config
import pytest

from services.cleanup_service import CleanupService


@pytest.fixture
def gadget_dir(tmp_path, monkeypatch):
    """A gadget dir whose config.yaml is also what ``config.CONFIG_YAML``
    resolves to, so the service's unified-config fallback reads the tmp
    file rather than the repo's own config."""
    monkeypatch.setattr(config, 'CONFIG_YAML', str(tmp_path / 'config.yaml'),
                        raising=False)
    return tmp_path


def _write_unified(gadget_dir, policies):
    with open(gadget_dir / 'config.yaml', 'w') as f:
        yaml.safe_dump({'cleanup': {'policies': policies}}, f)


# ---------------------------------------------------------------------------
# Unified-config fallback
# ---------------------------------------------------------------------------


def test_migrated_policies_still_drive_card_cleanup(gadget_dir):
    """Legacy JSON gone + cleanup.policies present = policies load."""
    _write_unified(gadget_dir, {
        'SentryClips': {'enabled': True, 'retention_days': 7},
        'SavedClips': {'enabled': False, 'retention_days': 14},
    })
    service = CleanupService(str(gadget_dir))
    assert service.policies['SentryClips']['enabled'] is True
    assert service.policies['SentryClips']['age_based'] == {
        'days': 7, 'enabled': True,
    }
    assert service.policies['SavedClips']['enabled'] is False


def test_legacy_json_still_wins_when_present(gadget_dir):
    """Pre-migration boxes keep their exact legacy behaviour."""
    (gadget_dir / 'cleanup_config.json').write_text(
        '{"SentryClips": {"enabled": true, '
        '"age_based": {"days": 3, "enabled": true}}}'
    )
    _write_unified(gadget_dir, {
        'SentryClips': {'enabled': True, 'retention_days': 99},
    })
    service = CleanupService(str(gadget_dir))
    assert service.policies['SentryClips']['age_based']['days'] == 3


def test_archivedclips_never_becomes_a_card_policy(gadget_dir):
    """ArchivedClips is archive_watchdog's folder, not a TeslaCam one."""
    _write_unified(gadget_dir, {
        'ArchivedClips': {'enabled': True, 'retention_days': 30},
        'SentryClips': {'enabled': True, 'retention_days': 7},
    })
    service = CleanupService(str(gadget_dir))
    assert 'ArchivedClips' not in service.policies
    assert 'SentryClips' in service.policies


def test_no_config_anywhere_means_no_policies(gadget_dir):
    assert CleanupService(str(gadget_dir)).policies == {}


# ---------------------------------------------------------------------------
# Policy-refused files have no second tier to wait for
# ---------------------------------------------------------------------------


def _clip_row(path):
    return {'path': path, 'teslacam_root': '/mnt/gadget/part1/TeslaCam'}


def test_evidence_only_does_not_immortalize_camera_clips(gadget_dir, monkeypatch):
    """A clip the policy will never archive counts as having its copy."""
    monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'evidence-only', raising=False)
    service = CleanupService(str(gadget_dir))
    row = _clip_row(
        '/mnt/gadget/part1/TeslaCam/SentryClips/2026-08-20_01-09-24/'
        '2026-08-20_01-09-24-front.mp4'
    )
    assert service._has_archive_copy(row) is True


def test_wanted_clips_still_wait_for_a_real_copy(gadget_dir, monkeypatch):
    """Under ``events`` the same clip IS archive-bound, so the guard holds
    until the copy exists (here: it never will, in an empty tmp archive)."""
    monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'events', raising=False)
    service = CleanupService(str(gadget_dir))
    row = _clip_row(
        '/mnt/gadget/part1/TeslaCam/SentryClips/2026-08-20_01-09-24/'
        '2026-08-20_01-09-24-front.mp4'
    )
    assert service._has_archive_copy(row) is False
