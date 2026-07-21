# Hermes Agent VM. Boot boilerplate (systemd, udev, networkd, init shim, fstab,
# getty mask, sshd, the agent user) lives in vm-base; this image adds only the
# Hermes workload. Build vm-base first:
#
#   buildah bud --network host --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base
#
# then build this image (see README):
#
#   hearthctl image build --name hermes-vm --dockerfile example/hermes-vm/Dockerfile \
#     --context example/hermes-vm --disk 16
FROM localhost/vm-base

ENV DEBIAN_FRONTEND=noninteractive

# Hermes 0.19.0. Keep the full source pin so the default build is repeatable;
# pass --build-arg HERMES_COMMIT=<sha> to test another revision.
ARG HERMES_COMMIT=3ef6bbd201263d354fd83ec55b3c306ded2eb72a

# Workload tooling on top of vm-base's boot essentials (which already ships
# curl + ca-certificates). ffmpeg/ripgrep are Hermes runtime deps.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg \
    git \
    jq \
    python3 \
    ripgrep \
  && apt-get clean \
  && rm -rf /var/lib/apt/lists/*

# Install Hermes Agent for the agent user, pinned to $HERMES_COMMIT. The
# interactive setup wizard and the Playwright/Chromium download are skipped;
# drop --skip-browser and rebuild if you want Hermes browser tools inside the VM.
RUN su - agent -c "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash -s -- --skip-setup --non-interactive --skip-browser --commit \"$HERMES_COMMIT\""

# hermes.env and authorized_keys are NOT baked in anymore. §3/§10 provisions the
# env through `--provision-file` and SSH through typed
# `--authorized-keys-file`; the .example files show their shape.
RUN mkdir -p /etc/systemd/system/hearth-guestd.service.d
COPY hermes.service /etc/systemd/system/hermes.service
COPY hermes-probe.service /etc/systemd/system/hermes-probe.service
COPY hermes-probe /usr/local/bin/hermes-probe
COPY netdiag.service /etc/systemd/system/netdiag.service
COPY netdiag /usr/local/bin/netdiag
COPY hearth-guestd-hermes.conf /etc/systemd/system/hearth-guestd.service.d/10-hermes.conf

RUN chmod 0755 /usr/local/bin/hermes-probe /usr/local/bin/netdiag \
  && ln -sf /etc/systemd/system/hermes.service /etc/systemd/system/multi-user.target.wants/hermes.service \
  && ln -sf /etc/systemd/system/hermes-probe.service /etc/systemd/system/multi-user.target.wants/hermes-probe.service \
  && ln -sf /etc/systemd/system/netdiag.service /etc/systemd/system/multi-user.target.wants/netdiag.service

# §8 verification: fail the build here if the launcher moved off ~/.local/bin,
# instead of discovering it from the serial console on first boot.
RUN test -x /home/agent/.local/bin/hermes \
  && su - agent -c 'hermes --version' \
  && su - agent -c 'hermes acp --check'
