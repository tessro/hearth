PREFIX      ?= /usr/local
BINDIR      ?= $(PREFIX)/bin
UNITDIR     ?= /etc/systemd/system
FIRMWAREDIR ?= /var/lib/hearth/firmware
FIRMWARE_URL ?= https://github.com/cloud-hypervisor/edk2/releases/download/ch-1e1b96f126/CLOUDHV.fd
BRIDGE      ?= hearth0

CARGO       ?= cargo
INSTALL     ?= install

BUILDAH     ?= buildah

.PHONY: build test clippy dev install uninstall firmware vm-base reload enable start stop restart status logs ping clean

build:
	$(CARGO) build --release

# Build the shared VM base layer as a plain local buildah image. Workload images
# (example/hermes-vm, example/agent-vm) are `FROM localhost/vm-base`, so build
# this first. --layers caches each step for cheap rebuilds.
vm-base:
	$(BUILDAH) bud --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base

test:
	$(CARGO) test --release

clippy:
	$(CARGO) clippy --release --all-targets -- -D warnings

# Run hearthd in the foreground out of target/release. Override BRIDGE for a
# different bridge name.
dev: build
	sudo HEARTH_FIRMWARE=$(FIRMWAREDIR)/CLOUDHV.fd HEARTH_BRIDGE=$(BRIDGE) \
		./target/release/hearthd

# Install the release binaries and the systemd unit. `install -D` creates parent
# dirs. When installing to the live system (no DESTDIR) and systemd is present,
# reload it and print the next steps; when staging under DESTDIR, print the
# commands to run on the target instead. See docs/operations.md.
DOCDIR ?= $(PREFIX)/share/doc/hearth

install: build
	$(INSTALL) -D -m 0755 target/release/hearthd   $(DESTDIR)$(BINDIR)/hearthd
	$(INSTALL) -D -m 0755 target/release/hearthctl $(DESTDIR)$(BINDIR)/hearthctl
	$(INSTALL) -D -m 0644 systemd/hearth.service   $(DESTDIR)$(UNITDIR)/hearth.service
	$(INSTALL) -D -m 0644 docs/operations.md       $(DESTDIR)$(DOCDIR)/operations.md
	@if [ -z "$(DESTDIR)" ] && command -v systemctl >/dev/null 2>&1; then \
		systemctl daemon-reload; \
		echo "hearthd installed. Next: hearthctl host check   then   systemctl enable --now hearth.service"; \
		echo "Build the guest kernel first if you have not: scripts/build-guest-kernel.sh (see docs/operations.md)."; \
	else \
		echo "Staged under '$(DESTDIR)'. On the target: systemctl daemon-reload && systemctl enable --now hearth.service"; \
	fi

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/hearthd $(DESTDIR)$(BINDIR)/hearthctl
	rm -f $(DESTDIR)$(UNITDIR)/hearth.service
	rm -f $(DESTDIR)$(DOCDIR)/operations.md
	@if [ -z "$(DESTDIR)" ] && command -v systemctl >/dev/null 2>&1; then \
		systemctl daemon-reload; \
	fi

# hearthd runs as root (systemd unit sets no User=), and the shipped install
# path creates no `hearth` user, so install the firmware root:root — owning it to
# a nonexistent `hearth` user would abort with `invalid user 'hearth'` on a fresh
# host. 0750/0640 keeps it root-only.
firmware:
	$(INSTALL) -d -m 0750 $(FIRMWAREDIR)
	curl -fsSL -o $(FIRMWAREDIR)/CLOUDHV.fd.tmp $(FIRMWARE_URL)
	$(INSTALL) -m 0640 $(FIRMWAREDIR)/CLOUDHV.fd.tmp $(FIRMWAREDIR)/CLOUDHV.fd
	rm -f $(FIRMWAREDIR)/CLOUDHV.fd.tmp

reload:
	systemctl daemon-reload

enable:
	systemctl enable --now hearth.service

start:
	systemctl start hearth.service

stop:
	systemctl stop hearth.service

restart:
	systemctl restart hearth.service

status:
	systemctl status hearth.service

logs:
	journalctl -u hearth.service -f

ping:
	hearthctl ping

clean:
	$(CARGO) clean
