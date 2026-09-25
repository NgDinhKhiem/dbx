#!/bin/bash
# Container health probe: GET <base path>/api/auth/check over loopback.
# That endpoint needs no session and is allowed during data migration.
# Uses bash's /dev/tcp so the image needs no curl/wget.
set -eu

port="${DBX_PORT:-4224}"
host="${DBX_HOST:-127.0.0.1}"
case "$host" in
    ''|0.0.0.0|::|'[::]') host=127.0.0.1 ;;
esac
host="${host#[}"
host="${host%]}"

base="$(printf '%s' "${DBX_PUBLIC_BASE_PATH:-/}" | sed -e 's#[?#].*$##' -e 's#^/*##' -e 's#/*$##')"
path="/${base:+$base/}api/auth/check"

exec 3<>"/dev/tcp/$host/$port"
printf 'GET %s HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n' "$path" >&3
IFS= read -r status <&3 || exit 1
case "$status" in
    "HTTP/1."?" 200"*) exit 0 ;;
    *) exit 1 ;;
esac
