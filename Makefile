PREFIX      ?= /usr/local
BINDIR      ?= $(PREFIX)/bin
UNITDIR     ?= /etc/systemd/system
BRIDGE      ?= hearth0

CARGO       ?= cargo
INSTALL     ?= install

BUILDAH     ?= buildah

.PHONY: build test clippy dev install install-bin install-agentd uninstall vm-base guest-bin guest-bin-musl reload enable start stop restart status logs ping clean

build:
	$(CARGO) build --release

# Guest target for hearth-guestd. musl gives a static binary that drops into any
# distro image regardless of libc (docs/agent-plane.md §2); requires the musl
# std (`rustup target add x86_64-unknown-linux-musl`). Override for other arches.
GUEST_MUSL_TARGET ?= x86_64-unknown-linux-musl
# Nix's musl cross wrapper defaults to a dynamic musl interpreter in the Nix
# store. Enforce the static contract this target promises on every toolchain.
GUEST_MUSL_RUSTFLAGS ?= -C target-feature=+crt-static -C link-arg=-static

# Build hearth-guestd and stage it into the vm-base build context. Defaults to a
# host-libc (glibc) build, correct for the ubuntu-based vm-base and buildable
# without the musl std installed. Use `guest-bin-musl` for the portable static
# binary that drops into any distro image.
guest-bin: build
	$(INSTALL) -D -m 0755 target/release/hearth-guestd example/vm-base/hearth-guestd

guest-bin-musl:
	RUSTFLAGS="$(GUEST_MUSL_RUSTFLAGS)" $(CARGO) build --release -p hearth-guestd --target $(GUEST_MUSL_TARGET)
	$(INSTALL) -D -m 0755 target/$(GUEST_MUSL_TARGET)/release/hearth-guestd example/vm-base/hearth-guestd

# Build the shared VM base layer as a plain local buildah image. Workload images
# (example/hermes-vm, example/agent-vm) are `FROM localhost/vm-base`, so build
# this first. --layers caches each step for cheap rebuilds. --network host runs
# RUN steps in the host netns: netavark races its own iptables chains between
# consecutive RUN steps and fails with "Chain already exists" — the same reason
# `hearthctl image build` defaults to host. VM-rootfs builds only need outbound.
# Depends on guest-bin so every vm-base image carries hearth-guestd (and thus
# declares guestd = true and can back agent-plane services).
vm-base: guest-bin
	$(BUILDAH) bud --network host --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base

test:
	$(CARGO) test --release

clippy:
	$(CARGO) clippy --release --all-targets -- -D warnings

# Run hearthd in the foreground out of target/release. Override BRIDGE for a
# different bridge name.
dev: build
	sudo HEARTH_BRIDGE=$(BRIDGE) \
		./target/release/hearthd

# Install the release binaries and the systemd unit. `install -D` creates parent
# dirs. When installing to the live system (no DESTDIR) and systemd is present,
# reload it and print the next steps; when staging under DESTDIR, print the
# commands to run on the target instead. See docs/operations.md.
DOCDIR ?= $(PREFIX)/share/doc/hearth

# Install just the binaries. Use this on NixOS (and anywhere else that manages
# systemd units declaratively): $(UNITDIR) is read-only there, so `install`'s
# unit copy fails, but updating hearthd/hearthctl is all a code deploy needs.
install-bin: build
	$(INSTALL) -D -m 0755 target/release/hearthd      $(DESTDIR)$(BINDIR)/hearthd
	$(INSTALL) -D -m 0755 target/release/hearthctl    $(DESTDIR)$(BINDIR)/hearthctl
	$(INSTALL) -D -m 0755 target/release/hearth-agentd $(DESTDIR)$(BINDIR)/hearth-agentd

# Install the agent-plane host daemon unit (opt-in — the machine plane runs
# without it). Requires a `hearth-agent` system user and the secret source files
# LoadCredential= points at; see docs/agent-plane.md §4. Installed separately so
# `make install` stays machine-plane-only.
install-agentd: install-bin
	$(INSTALL) -D -m 0644 systemd/hearth-agentd.service $(DESTDIR)$(UNITDIR)/hearth-agentd.service
	@echo "Installed hearth-agentd + hearth-agentd.service."
	@echo "Next: create the hearth-agent user, stage /etc/hearth/agent/{http-token,ref-key},"
	@echo "      then: systemctl enable --now hearth-agentd.service"

install: install-bin
	$(INSTALL) -D -m 0644 docs/operations.md $(DESTDIR)$(DOCDIR)/operations.md 2>/dev/null || true
	@if $(INSTALL) -D -m 0644 systemd/hearth.service $(DESTDIR)$(UNITDIR)/hearth.service 2>/dev/null; then \
		if [ -z "$(DESTDIR)" ] && command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload || true; fi; \
		echo "Installed hearthd + hearthctl + hearth.service."; \
		echo "Next: hearthctl host check   then   systemctl enable --now hearth.service"; \
		echo "Build the guest kernel first if you have not: scripts/build-guest-kernel.sh (see docs/operations.md)."; \
	else \
		echo "Installed hearthd + hearthctl to $(BINDIR)."; \
		echo "NOTE: $(UNITDIR) is not writable (read-only — NixOS?); skipped the systemd unit."; \
		echo "  Run hearthd from a runtime unit that survives until you manage it declaratively:"; \
		echo "    sudo cp systemd/hearth.service /run/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl restart hearth"; \
		echo "  or point configuration.nix at ExecStart=$(BINDIR)/hearthd. See docs/operations.md."; \
	fi

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/hearthd $(DESTDIR)$(BINDIR)/hearthctl $(DESTDIR)$(BINDIR)/hearth-agentd
	rm -f $(DESTDIR)$(UNITDIR)/hearth.service $(DESTDIR)$(UNITDIR)/hearth-agentd.service
	rm -f $(DESTDIR)$(DOCDIR)/operations.md
	@if [ -z "$(DESTDIR)" ] && command -v systemctl >/dev/null 2>&1; then \
		systemctl daemon-reload; \
	fi

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
