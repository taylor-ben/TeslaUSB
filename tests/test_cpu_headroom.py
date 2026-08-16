"""The measurement that replaced loadavg as the archive worker's gate.

The bug these guard against is not an exception, it is a number that
means something other than what its user thought. So the tests are
mostly about the box's REAL readings: a machine at 4.5 loadavg and 97%
idle CPU, where the old gate read "too busy to work" and the new one
reads "idle". See services/cpu_headroom.py for the measurements.
"""

from services import cpu_headroom


def _stat(user=0, nice=0, system=0, idle=0, iowait=0, irq=0, softirq=0):
    return (
        "cpu  %d %d %d %d %d %d %d 0 0 0\n"
        "cpu0 1 1 1 1 1 1 1 0 0 0\n"
        "intr 12345\n"
    ) % (user, nice, system, idle, iowait, irq, softirq)


class TestParse:
    def test_reads_the_aggregate_cpu_line_not_a_core(self):
        parsed = cpu_headroom.parse(_stat(user=10, system=5, idle=85))
        assert parsed == (15, 85)

    def test_iowait_counts_as_idle(self):
        # THE point of the module. A CPU parked in iowait will run the
        # watchdog daemon the moment it asks; calling that "busy" is
        # what kept the archive worker asleep for 3.5 hours in 6.
        busy, idle = cpu_headroom.parse(_stat(user=1, idle=0, iowait=99))
        assert (busy, idle) == (1, 99)

    def test_missing_cpu_line_is_none(self):
        assert cpu_headroom.parse("intr 1\nctxt 2\n") is None

    def test_garbage_counters_are_none_not_an_exception(self):
        assert cpu_headroom.parse("cpu  a b c d e f g\n") is None

    def test_truncated_line_is_none(self):
        # A short read mid-write leaves fewer than the five fields the
        # idle/iowait indices need.
        assert cpu_headroom.parse("cpu  1 2 3\n") is None

    def test_empty_input_is_none(self):
        assert cpu_headroom.parse("") is None


class TestFreePct:
    def test_a_fully_idle_interval_is_a_hundred(self):
        before = (100, 1000)
        after = (100, 1100)
        assert cpu_headroom.free_pct(before, after) == 100.0

    def test_a_fully_busy_interval_is_zero(self):
        assert cpu_headroom.free_pct((100, 1000), (200, 1000)) == 0.0

    def test_the_live_box_reading(self):
        # Twelve samples over 30 s on the Pi while the archive worker
        # was refusing to run: loadavg 4.3-4.7, CPU 89-99% free. Over
        # one 2.5 s interval at HZ=100 that is ~250 jiffies, ~15 busy.
        free = cpu_headroom.free_pct((0, 0), (15, 235))
        assert 92.0 < free < 96.0
        assert not cpu_headroom.starved(free, 12.0)

    def test_no_previous_sample_is_none(self):
        assert cpu_headroom.free_pct(None, (1, 2)) is None

    def test_unreadable_current_sample_is_none(self):
        assert cpu_headroom.free_pct((1, 2), None) is None

    def test_counters_going_backwards_are_none(self):
        # A reboot, or a cgroup file swapped underneath us. Better to
        # say nothing than to report a wild ratio.
        assert cpu_headroom.free_pct((100, 100), (50, 100)) is None
        assert cpu_headroom.free_pct((100, 100), (100, 50)) is None

    def test_too_short_an_interval_is_none(self):
        # The worker can come round its loop in milliseconds while
        # draining small files; three jiffies is noise, not a reading.
        assert cpu_headroom.free_pct((0, 0), (1, 2)) is None

    def test_exactly_at_the_minimum_window_reads(self):
        free = cpu_headroom.free_pct((0, 0), (0, cpu_headroom.MIN_JIFFIES))
        assert free == 100.0


class TestStarved:
    def test_below_the_floor_is_starvation(self):
        assert cpu_headroom.starved(5.0, 12.0) is True

    def test_at_the_floor_is_not(self):
        assert cpu_headroom.starved(12.0, 12.0) is False

    def test_an_unknown_reading_is_never_starvation(self):
        # The property that keeps a blind instrument from stopping the
        # archive — which is exactly how the old gate failed, only it
        # was a WORKING instrument measuring the wrong thing.
        assert cpu_headroom.starved(None, 12.0) is False

    def test_a_zero_floor_disables_the_gate(self):
        assert cpu_headroom.starved(0.0, 0.0) is False
        assert cpu_headroom.starved(0.0, -1.0) is False


class TestSample:
    def test_reads_a_real_proc_stat_when_there_is_one(self, tmp_path):
        stat = tmp_path / "stat"
        stat.write_text(_stat(user=7, idle=93))
        assert cpu_headroom.sample(str(stat)) == (7, 93)

    def test_a_missing_file_is_none_not_an_exception(self, tmp_path):
        # macOS and container sandboxes have no /proc/stat, and the
        # test suite runs on one of them.
        assert cpu_headroom.sample(str(tmp_path / "nope")) is None

    def test_a_directory_is_none(self, tmp_path):
        assert cpu_headroom.sample(str(tmp_path)) is None
