"""Metered-connection gate for the continuous cloud worker.

A car box often reaches the internet through a cellular dongle that
presents as ordinary WiFi — ``_is_wifi_connected()`` is True, yet every
byte costs money. The worker must hold automatic sync while
NetworkManager reports the active connection as metered, and resume the
moment an unmetered network takes over.

Contract pinned here:

1. ``_is_network_metered()`` parses nmcli's ``GENERAL.METERED`` output:
   ``yes`` and ``yes (guessed)`` are metered; ``no``, ``no (guessed)``,
   ``unknown``, empty output, and nmcli failure are not (fail-open — the
   gate is an economy measure, and a host without NetworkManager must
   keep syncing).
2. The worker skips drains while metered and ``pause_on_metered`` is on,
   and surfaces the hold via ``_sync_status["metered_paused"]``.
3. ``pause_on_metered: false`` disables the gate entirely.
"""
from __future__ import annotations

import subprocess
import time

import pytest

from services import cloud_archive_service as svc


# ---------------------------------------------------------------------------
# Fixtures (same harness as test_cloud_archive_continuous_worker)
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _reset_worker_state():
    if svc._worker_thread is not None and svc._worker_thread.is_alive():
        svc.stop(timeout=5.0)
    svc._worker_thread = None
    svc._sync_thread = None
    svc._worker_stop.clear()
    svc._wake.clear()
    svc._sync_cancel.clear()
    svc._drain_cancel.clear()
    svc._sync_status.update({
        "running": False,
        "worker_running": False,
        "wake_count": 0,
        "drain_count": 0,
        "error": None,
        "metered_paused": False,
    })
    yield
    if svc._worker_thread is not None and svc._worker_thread.is_alive():
        svc.stop(timeout=5.0)
    svc._worker_thread = None
    svc._sync_thread = None
    svc._worker_stop.clear()
    svc._wake.clear()


@pytest.fixture
def _enable_cloud(monkeypatch):
    monkeypatch.setattr(svc, 'CLOUD_ARCHIVE_ENABLED', True)
    monkeypatch.setattr(svc, 'CLOUD_ARCHIVE_PROVIDER', 'gdrive')


@pytest.fixture
def _stub_recover(monkeypatch):
    monkeypatch.setattr(svc, 'recover_interrupted_uploads', lambda _db: 0)


@pytest.fixture
def _stub_wifi_up(monkeypatch):
    monkeypatch.setattr(svc, '_is_wifi_connected', lambda: True)


@pytest.fixture
def _stub_no_archive_running(monkeypatch):
    fake_module = type('mod', (), {
        'get_archive_status': staticmethod(lambda: {"running": False}),
    })()
    import sys
    monkeypatch.setitem(
        sys.modules, 'services.cloud_rclone_service', fake_module,
    )


def _fake_nmcli(stdout: str):
    """A subprocess.run stand-in returning canned nmcli output."""
    def _run(cmd, **_kwargs):
        return subprocess.CompletedProcess(cmd, 0, stdout=stdout, stderr="")
    return _run


# ---------------------------------------------------------------------------
# 1. nmcli output parsing
# ---------------------------------------------------------------------------


class TestIsNetworkMetered:
    @pytest.mark.parametrize("stdout", [
        "GENERAL.METERED:yes\n",
        "GENERAL.METERED:yes (guessed)\n",
    ])
    def test_yes_variants_are_metered(self, monkeypatch, stdout):
        monkeypatch.setattr(svc.subprocess, 'run', _fake_nmcli(stdout))
        assert svc._is_network_metered() is True

    @pytest.mark.parametrize("stdout", [
        "GENERAL.METERED:no\n",
        "GENERAL.METERED:no (guessed)\n",
        "GENERAL.METERED:unknown\n",
        "",
        "garbage without a colon\n",
    ])
    def test_everything_else_is_not_metered(self, monkeypatch, stdout):
        monkeypatch.setattr(svc.subprocess, 'run', _fake_nmcli(stdout))
        assert svc._is_network_metered() is False

    def test_nmcli_failure_fails_open(self, monkeypatch):
        def _boom(*_a, **_k):
            raise FileNotFoundError("nmcli not installed")
        monkeypatch.setattr(svc.subprocess, 'run', _boom)
        assert svc._is_network_metered() is False


# ---------------------------------------------------------------------------
# 2. Worker holds while metered, surfaces the hold, resumes unmetered
# ---------------------------------------------------------------------------


class TestMeteredGate:
    def test_drain_skipped_and_status_surfaced_while_metered(
        self, monkeypatch, _enable_cloud, _stub_recover,
        _stub_wifi_up, _stub_no_archive_running,
    ):
        monkeypatch.setattr(svc, 'CLOUD_ARCHIVE_PAUSE_ON_METERED', True)
        monkeypatch.setattr(svc, '_is_network_metered', lambda: True)

        drain_calls = []
        monkeypatch.setattr(
            svc, '_drain_once',
            lambda *_a, **_k: (drain_calls.append(1), False)[1],
        )

        svc.start(teslacam_path="/x", db_path="/y")
        time.sleep(0.5)
        assert drain_calls == []
        assert svc._sync_status["metered_paused"] is True

    def test_gate_off_lets_metered_drain_run(
        self, monkeypatch, _enable_cloud, _stub_recover,
        _stub_wifi_up, _stub_no_archive_running,
    ):
        monkeypatch.setattr(svc, 'CLOUD_ARCHIVE_PAUSE_ON_METERED', False)
        monkeypatch.setattr(svc, '_is_network_metered', lambda: True)

        drain_calls = []
        monkeypatch.setattr(
            svc, '_drain_once',
            lambda *_a, **_k: (drain_calls.append(1), False)[1],
        )

        svc.start(teslacam_path="/x", db_path="/y")
        svc.wake()
        time.sleep(0.5)
        assert len(drain_calls) >= 1

    def test_unmetered_clears_the_paused_flag(
        self, monkeypatch, _enable_cloud, _stub_recover,
        _stub_wifi_up, _stub_no_archive_running,
    ):
        monkeypatch.setattr(svc, 'CLOUD_ARCHIVE_PAUSE_ON_METERED', True)
        monkeypatch.setattr(svc, '_is_network_metered', lambda: False)
        svc._sync_status["metered_paused"] = True

        monkeypatch.setattr(svc, '_drain_once', lambda *_a, **_k: False)

        svc.start(teslacam_path="/x", db_path="/y")
        svc.wake()
        time.sleep(0.5)
        assert svc._sync_status["metered_paused"] is False


# ---------------------------------------------------------------------------
# 3. The hold clears when there is no network at all
# ---------------------------------------------------------------------------


class TestMeteredFlagOnWifiDown:
    def test_wifi_down_clears_the_metered_hold(
        self, monkeypatch, _enable_cloud, _stub_recover, _stub_no_archive_running,
    ):
        # Unplugging a metered dongle must not leave the UI saying "waiting for
        # unmetered WiFi": the truth is no network at all, and the reset used
        # to sit below the WiFi gate where it could never run.
        monkeypatch.setattr(svc, 'CLOUD_ARCHIVE_PAUSE_ON_METERED', True)
        monkeypatch.setattr(svc, '_is_wifi_connected', lambda: False)
        monkeypatch.setattr(svc, '_is_network_metered', lambda: True)
        monkeypatch.setattr(svc, '_drain_once', lambda *_a, **_k: False)
        svc._sync_status["metered_paused"] = True

        svc.start(teslacam_path="/x", db_path="/y")
        time.sleep(0.5)
        assert svc._sync_status["metered_paused"] is False
