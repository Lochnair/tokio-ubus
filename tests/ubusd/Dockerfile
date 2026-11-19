# Upstream ubusd and the ubus CLI, built from source, with a Rust toolchain.
# Used by tests/ubusd/run.sh; see that script for usage.
FROM rust:1-bookworm

ARG LIBUBOX_REV=a396f38a8568a1d670c240465cd8d24fdfdf6463
ARG UBUS_REV=db20eff5535fe3126c05dc0edc029caf0e82fe10

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake libjson-c-dev jq \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    for repo in libubox:$LIBUBOX_REV ubus:$UBUS_REV; do \
        name=${repo%%:*}; rev=${repo#*:}; \
        git init -q /src/$name; \
        git -C /src/$name fetch -q --depth 1 https://git.openwrt.org/project/$name.git $rev; \
        git -C /src/$name checkout -q FETCH_HEAD; \
        cmake -S /src/$name -B /src/$name/build -DBUILD_LUA=OFF -DBUILD_EXAMPLES=OFF; \
        make -C /src/$name/build -j"$(nproc)" install; \
    done; \
    ldconfig

WORKDIR /work
