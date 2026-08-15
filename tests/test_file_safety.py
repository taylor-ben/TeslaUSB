"""Tests for services.file_safety — the single doorway for protected/safe deletes.

Phase 2.1 (issue #97) introduces ``safe_delete_archive_video`` as the only
sanctioned way to delete a clip from the local archive. These tests cover
both the existing helpers (``is_protected_file``, ``safe_remove``,
``safe_rmtree``) and the new helper.

The protection contract (must never break):

* A ``*.img`` file inside ``GADGET_DIR`` MUST NEVER be deleted by any
  helper in this module, regardless of its containing directory's mtime
  or any caller policy.
* ``safe_delete_archive_video`` returns a :class:`DeleteResult` whose
  ``outcome`` distinguishes DELETED / PROTECTED / MISSING / ERROR — the
  4-state contract callers rely on for accurate accounting and
  user-facing messages without re-probing the filesystem.
"""

from __future__ import annotations

import os

import pytest

from services import file_safety
from services.file_safety import DeleteOutcome


@pytest.fixture
def reset_gadget_dir(monkeypatch, tmp_path):
    """Force file_safety to use a tmp_path-based GADGET_DIR for the test."""
    fake_gadget = tmp_path / "gadget"
    fake_gadget.mkdir()
    # Reset the lazily-cached gadget dir and patch the lookup function.
    monkeypatch.setattr(file_safety, "_gadget_dir", None)
    monkeypatch.setattr(
        file_safety, "_get_gadget_dir",
        lambda: os.path.realpath(str(fake_gadget)),
    )
    return str(fake_gadget)


# ---------------------------------------------------------------------------
# is_protected_file
# ---------------------------------------------------------------------------


class TestIsProtectedFile:
    def test_img_in_gadget_is_protected(self, reset_gadget_dir):
        path = os.path.join(reset_gadget_dir, "usb_cam.img")
        # Create the file so realpath resolves correctly.
        open(path, "wb").close()
        assert file_safety.is_protected_file(path) is True

    def test_img_outside_gadget_is_NOT_protected(self, tmp_path, reset_gadget_dir):
        path = str(tmp_path / "stranger.img")
        open(path, "wb").close()
        assert file_safety.is_protected_file(path) is False

    def test_mp4_in_gadget_is_NOT_protected(self, reset_gadget_dir):
        path = os.path.join(reset_gadget_dir, "fake.mp4")
        open(path, "wb").close()
        assert file_safety.is_protected_file(path) is False

    def test_case_insensitive_extension(self, reset_gadget_dir):
        path = os.path.join(reset_gadget_dir, "USB_CAM.IMG")
        open(path, "wb").close()
        assert file_safety.is_protected_file(path) is True

    def test_nonexistent_path_does_not_crash(self, reset_gadget_dir):
        path = os.path.join(reset_gadget_dir, "ghost.img")
        # File never created — realpath still resolves; check should still
        # return True because the path syntactically points into GADGET_DIR.
        assert file_safety.is_protected_file(path) is True


# ---------------------------------------------------------------------------
# safe_delete_archive_video — the Phase 2.1 single doorway
# ---------------------------------------------------------------------------


class TestSafeDeleteArchiveVideo:
    def test_deletes_normal_archived_clip(self, tmp_path, reset_gadget_dir):
        path = str(tmp_path / "2026-05-12_06-00-00-front.mp4")
        with open(path, "wb") as f:
            f.write(b"X" * 1024)

        result = file_safety.safe_delete_archive_video(path)

        assert result.outcome is DeleteOutcome.DELETED
        assert result.bytes_freed == 1024
        assert not os.path.exists(path)

    def test_refuses_protected_img_in_gadget(self, reset_gadget_dir):
        path = os.path.join(reset_gadget_dir, "usb_cam.img")
        with open(path, "wb") as f:
            f.write(b"X" * 1024)

        result = file_safety.safe_delete_archive_video(path)

        assert result.outcome is DeleteOutcome.PROTECTED
        assert result.bytes_freed == 0
        assert os.path.exists(path), (
            "Protected IMG file was deleted — Phase 2.1 contract violated"
        )

    def test_missing_file_returns_missing(self, tmp_path, reset_gadget_dir):
        path = str(tmp_path / "ghost.mp4")
        result = file_safety.safe_delete_archive_video(path)
        assert result.outcome is DeleteOutcome.MISSING
        assert result.bytes_freed == 0

    def test_returns_size_before_deletion(self, tmp_path, reset_gadget_dir):
        path = str(tmp_path / "clip.mp4")
        size = 4096
        with open(path, "wb") as f:
            f.write(b"X" * size)

        result = file_safety.safe_delete_archive_video(path)
        assert result.outcome is DeleteOutcome.DELETED
        assert result.bytes_freed == size

    def test_zero_byte_file_is_deleted_with_zero_bytes_freed(
        self, tmp_path, reset_gadget_dir
    ):
        # The 4-state outcome enum disambiguates "0-byte deleted file"
        # (outcome=DELETED, bytes_freed=0) from "didn't delete" — callers
        # that use ``outcome is DELETED`` (per docstring) handle this
        # correctly. The ``bytes_freed > 0`` heuristic from the original
        # API is no longer necessary; tests pin the new contract.
        path = str(tmp_path / "empty.mp4")
        open(path, "wb").close()

        result = file_safety.safe_delete_archive_video(path)
        assert result.outcome is DeleteOutcome.DELETED
        assert result.bytes_freed == 0
        assert not os.path.exists(path)

    def test_oserror_on_remove_returns_error_outcome(
        self, tmp_path, reset_gadget_dir, monkeypatch
    ):
        path = str(tmp_path / "blocked.mp4")
        with open(path, "wb") as f:
            f.write(b"X" * 100)

        def boom(_):
            raise PermissionError("EACCES")

        monkeypatch.setattr(os, "remove", boom)
        result = file_safety.safe_delete_archive_video(path)
        assert result.outcome is DeleteOutcome.ERROR
        assert result.bytes_freed == 0
        # File still exists because remove was patched.
        assert os.path.exists(path)

    def test_oserror_on_stat_returns_error_outcome(
        self, tmp_path, reset_gadget_dir, monkeypatch
    ):
        path = str(tmp_path / "weird.mp4")
        with open(path, "wb") as f:
            f.write(b"X" * 100)

        def stat_boom(_):
            raise PermissionError("stat EACCES")

        monkeypatch.setattr(os.path, "getsize", stat_boom)
        result = file_safety.safe_delete_archive_video(path)
        assert result.outcome is DeleteOutcome.ERROR
        assert result.bytes_freed == 0


# ---------------------------------------------------------------------------
# safe_remove (existing helper) — quick regression coverage
# ---------------------------------------------------------------------------


class TestSafeRemove:
    def test_removes_unprotected_file(self, tmp_path, reset_gadget_dir):
        path = str(tmp_path / "x.txt")
        open(path, "wb").close()
        assert file_safety.safe_remove(path) is True
        assert not os.path.exists(path)

    def test_refuses_protected_file(self, reset_gadget_dir):
        path = os.path.join(reset_gadget_dir, "u.img")
        open(path, "wb").close()
        assert file_safety.safe_remove(path) is False
        assert os.path.exists(path)

    def test_missing_file_returns_false(self, tmp_path, reset_gadget_dir):
        assert file_safety.safe_remove(str(tmp_path / "ghost")) is False


# ---------------------------------------------------------------------------
# safe_rmtree (existing helper) — quick regression coverage
# ---------------------------------------------------------------------------


class TestSafeRmtree:
    def test_removes_clean_tree(self, tmp_path, reset_gadget_dir):
        d = tmp_path / "tree"
        d.mkdir()
        (d / "a.txt").write_bytes(b"x")
        (d / "b.txt").write_bytes(b"y")
        assert file_safety.safe_rmtree(str(d)) is True
        assert not d.exists()

    def test_refuses_tree_containing_protected_file(self, reset_gadget_dir):
        # Create a subtree containing a protected .img file. The helper
        # MUST refuse to remove the parent — both the tree and the file
        # must still exist after the call.
        sub = os.path.join(reset_gadget_dir, "sub")
        os.makedirs(sub)
        img_path = os.path.join(sub, "u.img")
        open(img_path, "wb").close()
        assert file_safety.safe_rmtree(sub) is False
        assert os.path.exists(img_path)
        assert os.path.isdir(sub)

    def test_missing_dir_returns_false(self, tmp_path, reset_gadget_dir):
        assert file_safety.safe_rmtree(str(tmp_path / "ghost")) is False


# ---------------------------------------------------------------------------
# The two rules the owner stated on 2026-08-15, enforced at the doorway
# ---------------------------------------------------------------------------
# 1. An event's evidence files (event.json / event.mp4 / thumb.png) are
#    never deleted. ~435 KB per event against 26-53 MB for one camera
#    clip, and they are the only record of WHY the car triggered.
# 2. Nothing is deleted before it exists on a second tier. Callers that
#    have one pass ``archived_check``; the doorway answers UNARCHIVED
#    when the second copy is not there yet.
# ---------------------------------------------------------------------------


class TestEvidenceIsNeverDeleted:
    @pytest.mark.parametrize(
        "name", ["event.json", "event.mp4", "thumb.png"],
    )
    def test_doorway_refuses_every_evidence_name(
        self, tmp_path, reset_gadget_dir, name,
    ):
        event_dir = tmp_path / "SentryClips" / "2026-08-14_19-18-38"
        event_dir.mkdir(parents=True)
        path = event_dir / name
        path.write_bytes(b"x" * 64)

        result = file_safety.safe_delete_archive_video(str(path))

        assert result.outcome is DeleteOutcome.PROTECTED
        assert result.bytes_freed == 0
        assert path.exists(), "evidence file was deleted"

    def test_a_camera_clip_in_the_same_folder_is_still_deletable(
        self, tmp_path, reset_gadget_dir,
    ):
        event_dir = tmp_path / "SentryClips" / "2026-08-14_19-18-38"
        event_dir.mkdir(parents=True)
        clip = event_dir / "2026-08-14_19-18-38-front.mp4"
        clip.write_bytes(b"x" * 64)

        result = file_safety.safe_delete_archive_video(str(clip))

        assert result.outcome is DeleteOutcome.DELETED
        assert not clip.exists()

    def test_rmtree_still_removes_an_event_folder(
        self, tmp_path, reset_gadget_dir,
    ):
        """The video panel's "delete this event" button must keep working.

        The evidence rule lives in the delete doorway and NOT in
        ``is_protected_file`` precisely because ``is_protected_file``
        also gates ``safe_rmtree``; putting it there would make the
        button fail on every event the owner explicitly asked to remove.
        """
        event_dir = tmp_path / "SentryClips" / "2026-08-14_19-18-38"
        event_dir.mkdir(parents=True)
        (event_dir / "event.json").write_bytes(b"{}")
        (event_dir / "thumb.png").write_bytes(b"x")
        (event_dir / "2026-08-14_19-18-38-front.mp4").write_bytes(b"x")

        assert file_safety.safe_rmtree(str(event_dir)) is True
        assert not event_dir.exists()


class TestArchivedCheckGate:
    def test_refuses_when_the_predicate_says_no(self, tmp_path, reset_gadget_dir):
        clip = tmp_path / "2026-08-14_19-18-38-front.mp4"
        clip.write_bytes(b"x" * 64)

        result = file_safety.safe_delete_archive_video(
            str(clip), archived_check=lambda _p: False,
        )

        assert result.outcome is DeleteOutcome.UNARCHIVED
        assert result.bytes_freed == 0
        assert clip.exists()

    def test_deletes_when_the_predicate_says_yes(self, tmp_path, reset_gadget_dir):
        clip = tmp_path / "2026-08-14_19-18-38-front.mp4"
        clip.write_bytes(b"x" * 64)

        result = file_safety.safe_delete_archive_video(
            str(clip), archived_check=lambda _p: True,
        )

        assert result.outcome is DeleteOutcome.DELETED
        assert not clip.exists()

    def test_no_predicate_keeps_the_old_behaviour(self, tmp_path, reset_gadget_dir):
        clip = tmp_path / "2026-08-14_19-18-38-front.mp4"
        clip.write_bytes(b"x" * 64)

        result = file_safety.safe_delete_archive_video(str(clip))

        assert result.outcome is DeleteOutcome.DELETED

    def test_unarchived_is_distinct_from_protected(self):
        """A prune reporting the two as one number cannot tell an
        operator whether waiting will help."""
        assert DeleteOutcome.UNARCHIVED is not DeleteOutcome.PROTECTED


class TestHasArchiveCopy:
    def _tiers(self, tmp_path):
        source_root = tmp_path / "part1" / "TeslaCam"
        archive_root = tmp_path / "ArchivedClips"
        (source_root / "SentryClips" / "2026-08-14_19-18-38").mkdir(parents=True)
        archive_root.mkdir()
        return source_root, archive_root

    def test_true_when_the_archive_holds_the_same_size(self, tmp_path):
        source_root, archive_root = self._tiers(tmp_path)
        rel = os.path.join("SentryClips", "2026-08-14_19-18-38", "front.mp4")
        src = source_root / rel
        src.write_bytes(b"x" * 4096)
        dest = archive_root / rel
        dest.parent.mkdir(parents=True)
        dest.write_bytes(b"x" * 4096)

        assert file_safety.has_archive_copy(
            str(src), str(source_root), str(archive_root),
        ) is True

    def test_false_when_the_archive_copy_is_missing(self, tmp_path):
        source_root, archive_root = self._tiers(tmp_path)
        src = source_root / "SentryClips" / "2026-08-14_19-18-38" / "front.mp4"
        src.write_bytes(b"x" * 4096)

        assert file_safety.has_archive_copy(
            str(src), str(source_root), str(archive_root),
        ) is False

    def test_false_when_the_sizes_disagree(self, tmp_path):
        """Size, not mtime: the archive copy inherits the car's FAT
        timestamp through ``shutil.copystat``, so the mtimes agree even
        on a half-written copy."""
        source_root, archive_root = self._tiers(tmp_path)
        rel = os.path.join("SentryClips", "2026-08-14_19-18-38", "front.mp4")
        src = source_root / rel
        src.write_bytes(b"x" * 4096)
        dest = archive_root / rel
        dest.parent.mkdir(parents=True)
        dest.write_bytes(b"x" * 100)
        os.utime(dest, (os.stat(src).st_atime, os.stat(src).st_mtime))

        assert file_safety.has_archive_copy(
            str(src), str(source_root), str(archive_root),
        ) is False
