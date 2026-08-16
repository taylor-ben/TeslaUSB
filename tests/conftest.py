"""conftest.py — set up imports and generated artefacts before test collection.

* Adds ``scripts/web`` to ``sys.path`` so ``from services.* import ...``
  works without an editable install.
* Auto-compiles ``services/dashcam_pb2.py`` from its ``.proto`` source
  before any test module is *collected*. Two test modules
  (``test_mapping_service.py``, ``test_sei_parser.py``) do
  ``from services.dashcam_pb2 import SeiMetadata`` at import time, and
  that file is gitignored on purpose (it's a generated artefact). On a
  fresh checkout — or in CI — collection used to abort with
  ``ModuleNotFoundError`` before any test ran. We pre-compile here using
  the same path the runtime uses (``sei_parser._get_sei_metadata_class``),
  so failures (e.g. ``protoc`` missing) surface with that helper's clear
  error message instead of an opaque import error.

This file is loaded by pytest BEFORE collection, which is the correct
phase to do generation (a session-scoped fixture would run too late
because collection happens first).

Closes #84.
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'scripts', 'web'))


@pytest.fixture(autouse=True)
def archive_policy_everything(monkeypatch):
    """Run every test under ``everything`` unless it says otherwise.

    ``services/archive_policy.py`` arrived on 2026-08-16 and its default
    is ``evidence-only``, which declines to archive RecentClips at all.
    Several hundred existing tests use ``RecentClips/x-front.mp4`` as a
    convenient fixture path while testing something else entirely —
    dead-lettering, moov verification, disk guards, claim mechanics —
    and under the real default they stop at the policy gate before
    reaching the thing they assert on.

    Pinning the OLD behaviour here keeps those tests honest about their
    own subject. Anything that means to test the policy sets it
    explicitly (``monkeypatch.setattr(config, 'ARCHIVE_POLICY', ...)``),
    which is also what makes those tests readable: the policy under test
    is written down in the test rather than inherited from config.yaml.
    """
    try:
        import config
    except Exception:  # noqa: BLE001 — tests that never load the app
        return
    monkeypatch.setattr(config, 'ARCHIVE_POLICY', 'everything', raising=False)

# Eagerly compile the protobuf module before any test imports it.
# Wrapped in try/except so a missing protoc surfaces a single clear
# warning during collection rather than 100s of cascading collection
# errors. Tests that don't use dashcam_pb2 will still run.
try:
    from services.sei_parser import _get_sei_metadata_class as _compile_proto

    _compile_proto()
except Exception as exc:  # noqa: BLE001 — collection-time best-effort
    import warnings

    warnings.warn(
        "Could not pre-compile services/dashcam_pb2.py for tests: "
        f"{exc}. Tests that import dashcam_pb2 directly will fail to "
        "collect. Install 'protobuf-compiler' (Debian/Ubuntu: "
        "`sudo apt install -y protobuf-compiler`) and re-run pytest.",
        stacklevel=1,
    )
