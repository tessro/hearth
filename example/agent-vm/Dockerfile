FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    dbus \
    git \
    iproute2 \
    jq \
    openssh-server \
    procps \
    python3 \
    sudo \
    systemd \
    systemd-sysv \
    util-linux \
  && apt-get clean \
  && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/bash agent \
  && install -d -m 0755 /etc/sudoers.d \
  && printf '%s\n' 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent \
  && chmod 0440 /etc/sudoers.d/agent

COPY 20-dhcp.network /etc/systemd/network/20-dhcp.network
COPY hearth-agent-probe.service /etc/systemd/system/hearth-agent-probe.service
COPY hearth-agent-probe /usr/local/bin/hearth-agent-probe
COPY init /usr/local/bin/init

RUN chmod 0755 /usr/local/bin/init /usr/local/bin/hearth-agent-probe \
  && mkdir -p /etc/systemd/system/multi-user.target.wants /var/lib/hearth-agent-probe /run/sshd \
  && ln -sf /lib/systemd/system/systemd-networkd.service /etc/systemd/system/multi-user.target.wants/systemd-networkd.service \
  && ln -sf /etc/systemd/system/hearth-agent-probe.service /etc/systemd/system/multi-user.target.wants/hearth-agent-probe.service \
  && printf '%s\n' '/dev/vda / ext4 defaults,x-systemd.growfs 0 1' > /etc/fstab

STOPSIGNAL SIGRTMIN+3
CMD ["/usr/local/bin/init"]
