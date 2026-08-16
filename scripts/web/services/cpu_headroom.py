"""How much CPU this box has to spare, measured rather than guessed.

Written 2026-08-16 to replace ``os.getloadavg()`` as the archive
worker's back-off trigger. The two numbers are not interchangeable and
the difference had the worker asleep for 3.5 of every 6 hours.

**Loadavg counts uninterruptible sleep.** On Linux the run-queue
average includes tasks in ``D`` state — blocked on I/O — not just tasks
competing for CPU. On a Pi Zero 2 W with a car plugged into the USB
gadget, the permanent residents of ``D`` are ``file-storage`` (the
gadget serving the car's writes), ``jbd2/mmcblk0p2-8`` (the ext4
journal) and ``kworker/*flush-179:0`` (writeback to the SD card). Those
three are what "a car is recording" looks like, which is to say: always.

Measured on the live box, twelve samples over 30 s while the archive
worker was refusing to run:

    loadavg1   nr_running   D-state tasks              CPU free
    4.71       2/152        0                          97.5%
    4.42       1/150        4  file-storage, jbd2, ..  99.4%
    4.35       1/151        5  file-storage, flush, .. 96.3%

Load pinned above 4 the whole time; the CPU never dropped below 89%
free and one task was runnable out of a hundred and fifty. A gate set
at "loadavg > 3.0" can never open on that box, and the queue grew from
1995 to 2148 rows underneath it.

**What the gate was actually protecting.** The BCM2835 hardware
watchdog resets the Pi if the userspace ``watchdog`` daemon misses its
ping (90 s timeout), and the daemon misses it when it cannot get
*scheduled*. That is a CPU-availability question, and this module
answers exactly that. SDIO bus contention — the other half of the old
comment — is relieved by ``archive_worker``'s per-chunk pause, which
already fires on every chunk once the disk passes 80% full and needs no
load signal at all.

PSI (``/proc/pressure/*``) would be the better instrument and is what a
newer kernel should use; the Pi's kernel is built without ``CONFIG_PSI``
(checked on the box the same day), so this reads ``/proc/stat`` deltas
instead. Pure functions take the samples as arguments so the arithmetic
is testable without a kernel.
"""

PROC_STAT = '/proc/stat'

# Below this many jiffies between two samples the ratio is noise: at the
# usual HZ=100 this is a tenth of a second of wall clock, and the worker
# can come round its loop faster than that when it is draining small
# files. Callers get ``None`` and should hold their previous verdict
# rather than act on a reading taken over three jiffies.
MIN_JIFFIES = 25


def parse(text):
    """``(busy, idle)`` jiffies from ``/proc/stat`` contents, or None.

    ``idle`` deliberately includes ``iowait``. A CPU parked in iowait is
    a CPU that will run the watchdog daemon the moment it asks — the
    whole point here is to stop treating "the SD card is busy" as "the
    processor is busy".
    """
    for line in text.splitlines():
        fields = line.split()
        if not fields or fields[0] != 'cpu':
            continue
        try:
            times = [int(value) for value in fields[1:]]
        except ValueError:
            return None
        if len(times) < 5:
            return None
        # user nice system idle iowait irq softirq steal ...
        idle = times[3] + times[4]
        return (sum(times) - idle, idle)
    return None


def sample(path=PROC_STAT):
    """Read one ``(busy, idle)`` sample, or None when /proc is unreadable.

    Non-Linux hosts and container sandboxes without ``/proc/stat`` are a
    real case (the test suite runs on macOS): they get ``None``, and
    every caller treats an unknown reading as "do not throttle" so a
    missing file can never wedge the worker shut.
    """
    try:
        with open(path, 'r') as handle:
            return parse(handle.read(4096))
    except (OSError, ValueError):
        return None


def free_pct(before, after):
    """Percentage of CPU time idle between two samples, or None.

    ``None`` when either sample is missing, when the counters went
    backwards (a reboot or a cgroup re-read), or when too little time
    passed to say anything — see :data:`MIN_JIFFIES`.
    """
    if before is None or after is None:
        return None
    busy = after[0] - before[0]
    idle = after[1] - before[1]
    if busy < 0 or idle < 0:
        return None
    total = busy + idle
    if total < MIN_JIFFIES:
        return None
    return 100.0 * idle / total


def starved(free, floor):
    """Whether ``free`` percent of CPU is below ``floor``.

    An unknown reading is never starvation. This is the one place that
    decision is written down, because "unknown means do not throttle" is
    the property that keeps a broken instrument from stopping the work —
    the failure mode this module exists to fix.
    """
    if free is None or floor <= 0:
        return False
    return free < floor
