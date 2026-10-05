FROM --platform=linux/amd64 docker.io/library/alpine@sha256:1f3591b8a02ea153f41c5bba878ad477f63ab3d19349762cb77504db02a23e15 AS build

ARG SOURCE_DATE_EPOCH=1700000000
ARG ROOTFS_SIZE=128M

RUN apk add --no-cache e2fsprogs \
    && mkdir -p /rootfs \
    && apk --root /rootfs --initdb --keys-dir /etc/apk/keys \
        --repositories-file /etc/apk/repositories add --no-cache \
        alpine-base iperf3 socat iproute2 \
    && mkdir -p /rootfs/proc /rootfs/sys /rootfs/dev /rootfs/sbin \
    && rm -f /rootfs/sbin/init \
    && : > /rootfs/var/log/apk.log

COPY init /rootfs/sbin/init
COPY net-setup /rootfs/sbin/net-setup

RUN chmod 0755 /rootfs/sbin/init /rootfs/sbin/net-setup \
    && find /rootfs -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} + \
    && mkdir -p /out \
    && E2FSPROGS_FAKE_TIME="${SOURCE_DATE_EPOCH}" mkfs.ext4 -F -q -d /rootfs \
        -U 01234567-89ab-cdef-0123-456789abcdef \
        -E hash_seed=01234567-89ab-cdef-0123-456789abcdef,lazy_itable_init=0,lazy_journal_init=0 \
        /out/rootfs.ext4 "${ROOTFS_SIZE}"

FROM scratch
COPY --from=build /out/rootfs.ext4 /rootfs.ext4
