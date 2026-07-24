#!/usr/bin/env bash
#
# Test probe: report the exit code of `assert_not_lifeline <unit>` in an isolated
# process, so sourcing common.sh cannot perturb the caller's shell state. Used by
# installer.test.sh's A1c lifeline-guard assertions. Exits 4 (EX_STEP) when <unit>
# is a remote-access lifeline, 0 otherwise.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=setup-lib/common.sh
. "${HERE}/../../common.sh"
assert_not_lifeline "$1"
