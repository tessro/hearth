# Hermes Agent VM. Boot boilerplate (systemd, udev, networkd, init shim, fstab,
# getty mask, sshd, the agent user) lives in vm-base; this image adds only the
# Hermes workload. Build vm-base first:
#
#   buildah bud --network host --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base
#
# then build this image with a pinned Hermes commit (see README):
#
#   hearthctl image build --name hermes-vm --dockerfile example/hermes-vm/Dockerfile \
#     --context example/hermes-vm --disk 16 --build-arg HERMES_COMMIT=<sha>
FROM localhost/vm-base

ENV DEBIAN_FRONTEND=noninteractive

# The Hermes commit to install. §8: no default, so a build is reproducible only
# with an explicit pin — pass --build-arg HERMES_COMMIT=<sha>. Record the tested
# sha in the README and bump it deliberately, in a commit.
ARG HERMES_COMMIT

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
RUN test -n "$HERMES_COMMIT" || { echo 'build with --build-arg HERMES_COMMIT=<sha> (see README)'; exit 1; } \
  && su - agent -c "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash -s -- --skip-setup --non-interactive --skip-browser --commit \"$HERMES_COMMIT\""

# hermes.env and authorized_keys are NOT baked in anymore. §3/§10 provisions the
# env through `--provision-file` and SSH through typed
# `--authorized-keys-file`; the .example files show their shape.
COPY hermes.service /etc/systemd/system/hermes.service
COPY hermes-probe.service /etc/systemd/system/hermes-probe.service
COPY hermes-probe /usr/local/bin/hermes-probe
COPY netdiag.service /etc/systemd/system/netdiag.service
COPY netdiag /usr/local/bin/netdiag

RUN chmod 0755 /usr/local/bin/hermes-probe /usr/local/bin/netdiag \
  && ln -sf /etc/systemd/system/hermes.service /etc/systemd/system/multi-user.target.wants/hermes.service \
  && ln -sf /etc/systemd/system/hermes-probe.service /etc/systemd/system/multi-user.target.wants/hermes-probe.service \
  && ln -sf /etc/systemd/system/netdiag.service /etc/systemd/system/multi-user.target.wants/netdiag.service

# §8 verification: fail the build here if the launcher moved off ~/.local/bin,
# instead of discovering it from the serial console on first boot.
RUN test -x /home/agent/.local/bin/hermes && su - agent -c 'hermes --version'
