# Minimal Hearth VM fixture. Boot boilerplate (systemd, udev, networkd, init
# shim, fstab, getty mask, sshd, the agent user) lives in vm-base; this image
# adds only the acceptance probe. Build vm-base first:
#
#   buildah bud --network host --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base
FROM localhost/vm-base

ENV DEBIAN_FRONTEND=noninteractive

# Tools the probe exercises on top of vm-base's boot essentials.
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    jq \
    python3 \
  && apt-get clean \
  && rm -rf /var/lib/apt/lists/*

COPY hearth-agent-probe.service /etc/systemd/system/hearth-agent-probe.service
COPY hearth-agent-probe /usr/local/bin/hearth-agent-probe

RUN chmod 0755 /usr/local/bin/hearth-agent-probe \
  && mkdir -p /var/lib/hearth-agent-probe \
  && ln -sf /etc/systemd/system/hearth-agent-probe.service /etc/systemd/system/multi-user.target.wants/hearth-agent-probe.service
