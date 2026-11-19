#!/bin/sh
# Runs examples/demo against upstream ubusd in a container and checks it with
# the ubus CLI. Usage: tests/ubusd/run.sh [master|24.10|25.12] (default
# master), from anywhere, with Docker.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
# OpenWrt release revisions, from package/{system/ubus,libs/libubox}/Makefile.
case ${1:-master} in
    master) revs= ;;
    24.10) revs="--build-arg UBUS_REV=60e04048a0e2f3e33651c19e62861b41be4c290f --build-arg LIBUBOX_REV=49056d178f42da98048a5d4c23f83a6f6bc6dd80" ;;
    25.12) revs="--build-arg UBUS_REV=24864e7840b3a02a9ef76284a373f6b2f00b8a9b --build-arg LIBUBOX_REV=ff13b89e82e64cdd7853419504e51ec0c7e487e0" ;;
    *) echo "unknown ubus version $1" >&2; exit 2 ;;
esac
image=tokio-ubus-ubusd:${1:-master}
# shellcheck disable=SC2086
docker build -q $revs -t "$image" "$here" >/dev/null
exec docker run --rm \
    -v "$root:/work" \
    -v tokio-ubus-target:/work/target \
    -v tokio-ubus-cargo:/usr/local/cargo/registry \
    "$image" sh /work/tests/ubusd/check.sh
