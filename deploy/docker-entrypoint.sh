#!/bin/sh
# DBX container entrypoint.
#
# The image starts as root only so that volumes created by older images (which
# ran dbx-web as root) keep working: the data and backup directories are handed
# to the unprivileged "dbx" user, then privileges are dropped with setpriv
# before dbx-web starts. When the container is started as a non-root user
# (e.g. `docker run --user 10001:10001` or a Kubernetes securityContext), the
# command is executed directly.
#
# DBX_RUN_AS_ROOT=1 keeps the old behaviour (run as root). Only use it when a
# mounted file (for example a 0600 root-owned secret) cannot be made readable
# by uid 10001.
set -eu

DBX_USER=dbx

if [ "$(id -u)" != "0" ]; then
    exec "$@"
fi

case "${DBX_RUN_AS_ROOT:-}" in
    1|true|TRUE|yes|YES)
        echo "dbx-entrypoint: DBX_RUN_AS_ROOT is set; running as root" >&2
        exec "$@"
        ;;
esac

uid="$(id -u "$DBX_USER")"
gid="$(id -g "$DBX_USER")"

for dir in "${DBX_DATA_DIR:-/app/data}" "${DBX_BACKUP_ROOT:-/app/backups}" ${DBX_AGENT_DIR:+"$DBX_AGENT_DIR"}; do
    [ -n "$dir" ] || continue
    mkdir -p "$dir"
    # Only touch entries that are not already ours, so restarts stay fast.
    # -xdev keeps the walk on the volume itself (no nested bind mounts).
    if find "$dir" -xdev \( ! -user "$uid" -o ! -group "$gid" \) -print -quit | grep -q .; then
        echo "dbx-entrypoint: handing $dir to $DBX_USER ($uid:$gid)" >&2
        find "$dir" -xdev \( ! -user "$uid" -o ! -group "$gid" \) -exec chown -h "$uid:$gid" {} +
    fi
done

export HOME=/home/dbx USER="$DBX_USER" LOGNAME="$DBX_USER"
exec setpriv --reuid="$uid" --regid="$gid" --init-groups --inh-caps=-all --no-new-privs -- "$@"
