#!/usr/bin/env sh
# Container init: prepare the one runtime root this add-on's owner plane meets
# on, before the vigil service starts.
#
# It has to happen HERE and not in the image build. /data is a volume the
# Supervisor mounts over whatever the image put at that path, so a directory
# created at build time is not present at run time; and the service itself is
# started as the vigil binary directly (a launcher between the Supervisor and
# the process would break signal delivery and is refused by this repository's
# packaging contract), so there is no wrapper to do it in.
#
# CONTEXTDB_OWNER_READ_RUNTIME_DIR is set in the image, so it is already in the
# environment of this script, of the daemon the service starts, and of any
# separate `vigil why`/`events`/`stats` exec'd into the container afterwards.
# Both sides of the owner plane read that one variable, which is what lets a
# command reach the running daemon: a packaged service has no per-user runtime
# directory to fall back on, so without a stated root the daemon binds where no
# command can find it and every live command answers about a store nobody is
# holding.
set -eu

runtime_root="${CONTEXTDB_OWNER_READ_RUNTIME_DIR:?the add-on image states the runtime root}"
mkdir -p "$runtime_root"
# The runtime drops to this user before it binds anything, and the substrate
# requires the stated root to be owned by whoever binds there and readable by
# nobody else. Applied here, as root, because after the drop it is too late.
chown "${VIGIL_RUN_UID:-1000}:${VIGIL_RUN_GID:-1000}" "$runtime_root"
chmod 700 "$runtime_root"
