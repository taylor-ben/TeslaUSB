"""What this box keeps out of everything the car records.

The archive is a second copy on the same SD card the camera image lives
on, so its size is a decision, not an accident. The numbers that forced
one, measured on the box 2026-08-16:

    117 GB card:  49 GB camera image + 49 GB archive + 7.8 GB free
    archive queue: 2148 rows pending, 2096 of them RecentClips clips
    one Sentry event, six angles:      ~1.8 GB
    the same event's evidence files:   ~435 KB

At 1.8 GB an event the card holds a fortnight and then the prune starts
eating the oldest thing it has. At 435 KB it holds years of *why the car
triggered* — ``event.json``'s reason, camera and GPS, the car's own
``event.mp4`` of the triggering angle, and ``thumb.png``. Ben's call the
same day: evidence only.

``RecentClips`` has no evidence files in it at all — it is the rolling
driving ring, and under ``evidence-only`` none of it is archived. That
is the whole 2096 rows, and it is the point rather than a side effect,
but it does mean drive footage lives only as long as Tesla's own ~60
minute rotation. ``events`` is the setting that keeps full six-angle
Sentry footage; ``everything`` is the pre-2026-08-16 behaviour.

Deliberately pure and import-light: the queue producer, the inotify
watcher and the worker all have to agree on the answer, and a
disagreement between them shows up as files that get copied and then
pruned forever.
"""

import os

from services import tesla_clips

# The three TeslaCam folders the car writes into. Order is irrelevant
# here; ``archive_producer`` owns the scan order.
GROUPS = ('RecentClips', 'SentryClips', 'SavedClips')

# The folders whose subdirectories are events. RecentClips is flat.
EVENT_GROUPS = ('SentryClips', 'SavedClips')

EVIDENCE_ONLY = 'evidence-only'
EVENTS = 'events'
EVERYTHING = 'everything'
POLICIES = (EVIDENCE_ONLY, EVENTS, EVERYTHING)

DEFAULT_POLICY = EVIDENCE_ONLY


def group_of(path):
    """Which TeslaCam folder ``path`` sits under, or None.

    Matched case-insensitively on a path component, never on a
    substring: a clip named ``...-SentryClips.mp4`` in RecentClips is a
    filename, not a group.
    """
    parts = (path or '').replace('\\', '/').split('/')
    lowered = [part.lower() for part in parts]
    for group in GROUPS:
        try:
            index = lowered.index(group.lower())
        except ValueError:
            continue
        return (group, len(parts) - index - 1)
    return None


def in_event_folder(path):
    """Whether ``path`` is a file inside a SentryClips/SavedClips event.

    True only two levels down (``SentryClips/<event>/<file>``). A file
    dropped directly in ``SentryClips/`` is not part of any event and
    carries no evidence, so no policy wants it.
    """
    found = group_of(path)
    if found is None:
        return False
    group, depth = found
    return group in EVENT_GROUPS and depth >= 2


def is_media(path):
    """Whether ``path`` is something the archive would ever hold."""
    return path.lower().endswith('.mp4') or tesla_clips.is_evidence_file(path)


def wanted(path, policy=DEFAULT_POLICY):
    """Whether ``policy`` says to archive ``path``.

    Paths outside the three TeslaCam groups return True: this function
    governs what comes OFF the card, and the callers also walk
    ``ArchivedClips`` itself for back-fill, which is already a keep
    decision somebody made.
    """
    if not is_media(path):
        return False
    if group_of(path) is None:
        return True
    if not in_event_folder(path):
        # Flat: ``RecentClips/*.mp4``, or a stray file sitting at a
        # group's root. Only ``everything`` wants any of it, and only
        # the clips — an ``event.json`` outside an event folder belongs
        # to no event and describes nothing.
        return policy == EVERYTHING and path.lower().endswith('.mp4')
    if policy == EVIDENCE_ONLY:
        return tesla_clips.is_evidence_file(path)
    return True


def resolve():
    """The configured policy, falling back to :data:`DEFAULT_POLICY`.

    An unrecognised value in ``config.yaml`` falls back rather than
    raising: a typo must not stop the box archiving evidence, and the
    fallback is the conservative-on-space choice.
    """
    try:
        from config import ARCHIVE_POLICY
        policy = str(ARCHIVE_POLICY).strip().lower()
    except Exception:  # noqa: BLE001
        return DEFAULT_POLICY
    return policy if policy in POLICIES else DEFAULT_POLICY


def describe(policy=None):
    """One line for a log or a status endpoint."""
    policy = policy or resolve()
    if policy == EVERYTHING:
        return 'archiving every clip and every event file'
    if policy == EVENTS:
        return 'archiving Sentry/Saved events in full, not the driving ring'
    return ('archiving event evidence only (event.json, event.mp4, '
            'thumb.png) — no camera clips')


def evidence_names():
    """The evidence filenames, re-exported so callers need one import."""
    return tesla_clips.EVIDENCE_NAMES


def unwanted_reason(path, policy=None):
    """Why ``policy`` refuses ``path``, for the queue row's error text."""
    policy = policy or resolve()
    if not is_media(path):
        return 'not an archivable file'
    group = group_of(path)
    name = group[0] if group else os.path.dirname(path)
    if not in_event_folder(path):
        return f'{name} is not archived under policy {policy!r}'
    return f'{os.path.basename(path)} is not evidence (policy {policy!r})'
