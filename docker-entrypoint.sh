#!/bin/sh
# Entrypoint of the Docker image. With arguments, it runs hetzner-fw-updater
# with them. Without, it runs check on /config/config.json every
# CHECK_INTERVAL seconds (default 900, i.e. 15 minutes) until stopped.
set -eu

if [ $# -gt 0 ]; then
    exec hetzner-fw-updater "$@"
fi

interval="${CHECK_INTERVAL:-900}"
case "$interval" in
    '' | *[!0-9]* | 0)
        echo "CHECK_INTERVAL must be a positive number of seconds, not \"$interval\"" >&2
        exit 1
        ;;
esac

# As PID 1, sh ignores SIGTERM unless it is trapped. The sleep runs in the
# background so that docker stop ends the loop at once, not after the sleep.
trap 'exit 0' TERM INT

echo "Running check every $interval seconds"
while :; do
    # A failed check is logged, and the next one still runs.
    hetzner-fw-updater check config.json || echo "check failed with exit code $?" >&2
    sleep "$interval" &
    wait $!
done
