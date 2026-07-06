FROM alpine:latest
RUN printf '%s\n' '#!/bin/sh' \
    'mount -t proc proc /proc' \
    'mount -t sysfs sysfs /sys' \
    'mount -t devtmpfs devtmpfs /dev 2>/dev/null || true' \
    'echo hello from OCI rootfs' \
    'uname -a' \
    'exec /bin/sh' > /init && chmod +x /init
CMD ["/init"]
