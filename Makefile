PREFIX      ?= /usr
BINDIR      ?= $(PREFIX)/bin
LIBDIR      ?= $(PREFIX)/lib
GUESTPAYLOADDIR ?= $(LIBDIR)/hearth/guest
UNITDIR     ?= $(LIBDIR)/systemd/system
CONFDIR     ?= /etc/hearth
BRIDGE      ?= hearth0

CARGO       ?= cargo
INSTALL     ?= install

BUILDAH     ?= buildah
NFPM        ?= nfpm

RELEASE_TARGET ?= x86_64-unknown-linux-musl
RELEASE_STAGE  ?= target/release-stage
PORTABLE_STAGE ?= target/portable-stage
RELEASE_DIST   ?= dist
PACKAGE_FORMAT ?=
RELEASE_VERSION := $(shell sed -n 's/^version = "\([^\"]*\)"/\1/p' Cargo.toml | head -1)

.PHONY: build host-bins agentd-bin guest-bin agent-plane-artifacts release-bins release-portable-bins release-kernel release-stage release-portable-stage release-archive release-packages release-check check agui-conformance fmt test clippy dev dev-restart dev-restart-agent-plane dev-reset install install-bin install-guest-payload install-agentd uninstall vm-base reload enable start stop restart status logs ping clean

build: host-bins

# Host-side release executables. hearth-guestd is intentionally excluded: its
# deployable artifact is always built for musl by `guest-bin`.
host-bins:
	$(CARGO) build --release --locked -p hearthd -p hearthctl -p hearth-agentd

agentd-bin:
	$(CARGO) build --release --locked -p hearth-agentd

# The guest artifact is always a static musl binary so it can run in any VM
# image without inheriting the host's libc or Nix store interpreter.
GUEST_MUSL_TARGET ?= x86_64-unknown-linux-musl
GUEST_BIN         := target/$(GUEST_MUSL_TARGET)/release/hearth-guestd
STAGED_GUEST_BIN  := example/vm-base/hearth-guestd

guest-bin:
	$(CARGO) build --release --locked -p hearth-guestd --target $(GUEST_MUSL_TARGET)
	@if readelf -lW "$(GUEST_BIN)" | grep -q ' INTERP '; then \
		echo "error: $(GUEST_BIN) is dynamically linked; refusing to stage it" >&2; \
		exit 1; \
	fi
	@"$(GUEST_BIN)" --version
	$(INSTALL) -D -m 0755 "$(GUEST_BIN)" "$(STAGED_GUEST_BIN)"
	@cmp --silent "$(GUEST_BIN)" "$(STAGED_GUEST_BIN)"
	@file "$(GUEST_BIN)" "$(STAGED_GUEST_BIN)"

# Produce the two binaries that must be deployed together for the agent plane.
# Packaging/version metadata intentionally lives outside this target for now.
agent-plane-artifacts: agentd-bin guest-bin
	@sha256sum target/release/hearth-agentd "$(GUEST_BIN)" "$(STAGED_GUEST_BIN)"

# Native host binaries are for .deb/.rpm and source installs. The guest payload
# is always musl-static. HEARTH_RELEASE strips the local Git suffix.
release-bins:
	HEARTH_RELEASE=1 $(CARGO) build --release --locked \
		-p hearthd -p hearthctl -p hearth-agentd
	HEARTH_RELEASE=1 $(CARGO) build --release --locked --target $(RELEASE_TARGET) -p hearth-guestd
	@if readelf -lW "target/$(RELEASE_TARGET)/release/hearth-guestd" | grep -q ' INTERP '; then \
		echo "error: hearth-guestd is dynamically linked" >&2; exit 1; \
	fi

# The portable tarball has static host tools as well as the static guest.
release-portable-bins:
	HEARTH_RELEASE=1 $(CARGO) build --release --locked --target $(RELEASE_TARGET) \
		-p hearthd -p hearthctl -p hearth-agentd -p hearth-guestd
	@for binary in hearthd hearthctl hearth-agentd hearth-guestd; do \
		path="target/$(RELEASE_TARGET)/release/$$binary"; \
		if readelf -lW "$$path" | grep -q ' INTERP '; then \
			echo "error: $$path is dynamically linked" >&2; exit 1; \
		fi; \
	done

release-kernel: target/release-kernel/current/vmlinux

target/release-kernel/current/vmlinux:
	scripts/build-guest-kernel.sh --install-dir "$(CURDIR)/target/release-kernel"

release-stage: release-bins release-kernel
	HEARTH_STAGE_DIR="$(CURDIR)/$(RELEASE_STAGE)" \
		HEARTH_RELEASE_TARGET="$(RELEASE_TARGET)" \
		HEARTH_STAGE_FLAVOR=native scripts/stage-release.sh

release-portable-stage: release-portable-bins release-kernel
	HEARTH_STAGE_DIR="$(CURDIR)/$(PORTABLE_STAGE)" \
		HEARTH_RELEASE_TARGET="$(RELEASE_TARGET)" \
		HEARTH_STAGE_FLAVOR=portable scripts/stage-release.sh

release-archive: release-portable-stage
	HEARTH_STAGE_DIR="$(CURDIR)/$(PORTABLE_STAGE)" \
		HEARTH_DIST_DIR="$(CURDIR)/$(RELEASE_DIST)" \
		HEARTH_VERSION="$(RELEASE_VERSION)" scripts/archive-release.sh

release-packages: release-stage
	@command -v $(NFPM) >/dev/null || { echo "error: nfpm is required" >&2; exit 1; }
	@if [ "$(PACKAGE_FORMAT)" != deb ] && [ "$(PACKAGE_FORMAT)" != rpm ]; then \
		echo "error: set PACKAGE_FORMAT=deb or PACKAGE_FORMAT=rpm; CI builds each on its target system" >&2; \
		exit 2; \
	fi
	@mkdir -p "$(RELEASE_DIST)"
	@if [ "$(PACKAGE_FORMAT)" = deb ]; then \
		HEARTH_VERSION="$(RELEASE_VERSION)" HEARTH_STAGE="$(CURDIR)/$(RELEASE_STAGE)" \
		scripts/build-package.sh deb "$(RELEASE_DIST)/hearth_$(RELEASE_VERSION)_amd64.deb"; \
	else \
		HEARTH_VERSION="$(RELEASE_VERSION)" HEARTH_STAGE="$(CURDIR)/$(RELEASE_STAGE)" \
		scripts/build-package.sh rpm "$(RELEASE_DIST)/hearth-$(RELEASE_VERSION)-1.x86_64.rpm"; \
	fi

release-check: release-stage release-portable-stage
	HEARTH_STAGE_DIR="$(CURDIR)/$(RELEASE_STAGE)" \
		HEARTH_VERSION="$(RELEASE_VERSION)" HEARTH_STAGE_FLAVOR=native scripts/verify-release.sh
	HEARTH_STAGE_DIR="$(CURDIR)/$(PORTABLE_STAGE)" \
		HEARTH_VERSION="$(RELEASE_VERSION)" HEARTH_STAGE_FLAVOR=portable scripts/verify-release.sh
	systemd-analyze verify "$(RELEASE_STAGE)/lib/systemd/system/hearth.service" \
		"$(RELEASE_STAGE)/lib/systemd/system/hearth-agentd.service"
	scripts/test-dev-restart.sh
	HEARTH_STAGE_DIR="$(CURDIR)/$(PORTABLE_STAGE)" scripts/test-reproducible-archive.sh

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

check: fmt clippy test

# The Rust suite has a dependency-free HTTP/SSE client. This extra conformance
# pass uses the pinned, unmodified TypeScript HttpAgent from the web workspace.
# Keep it opt-in because a fresh Rust-only checkout may not have run pnpm yet.
agui-conformance:
	@test -d web/node_modules/@ag-ui/client || { \
		echo "error: web dependencies are missing; run 'pnpm --dir web install' first" >&2; \
		exit 1; \
	}
	$(CARGO) test --release --locked -p hearth-e2e --test phase3_agui_http \
		unmodified_http_agent_interrupts_resumes_and_follows_up -- \
		--ignored --exact --nocapture

fmt:
	$(CARGO) fmt --all -- --check

test:
	$(CARGO) test --release --locked

clippy:
	$(CARGO) clippy --release --locked --all-targets -- -D warnings

# Run hearthd in the foreground out of target/release. Override BRIDGE for a
# different bridge name.
dev: build
	sudo HEARTH_BRIDGE=$(BRIDGE) \
		./target/release/hearthd

dev-restart:
	scripts/dev-restart.sh

dev-restart-agent-plane:
	HEARTH_DEV_AGENT_PLANE=1 scripts/dev-restart.sh

dev-reset:
	scripts/dev-restart.sh --reset

# Install the release binaries and the systemd unit. `install -D` creates parent
# dirs. When installing to the live system (no DESTDIR) and systemd is present,
# reload it and print the next steps; when staging under DESTDIR, print the
# commands to run on the target instead. See docs/operations.md.
DOCDIR ?= $(PREFIX)/share/doc/hearth

# Install just the already-built binaries. Build as the invoking user first;
# privileged install targets must never invoke Cargo and leave root-owned
# artifacts in target/. Use this on NixOS (and anywhere else that manages
# systemd units declaratively): $(UNITDIR) is read-only there, so `install`'s
# unit copy fails, but updating hearthd/hearthctl is all a code deploy needs.
install-bin:
	@for binary in target/release/hearthd target/release/hearthctl target/release/hearth-agentd; do \
		test -x "$$binary" || { \
			echo "error: $$binary is missing; run 'make build' without sudo first" >&2; \
			exit 1; \
		}; \
	done
	$(INSTALL) -D -m 0755 target/release/hearthd      $(DESTDIR)$(BINDIR)/hearthd
	$(INSTALL) -D -m 0755 target/release/hearthctl    $(DESTDIR)$(BINDIR)/hearthctl
	$(INSTALL) -D -m 0755 target/release/hearth-agentd $(DESTDIR)$(BINDIR)/hearth-agentd

# Install the portable guest-only payload outside PATH. hearthctl derives this
# location from its own PREFIX and uses it as the default `upgrade --from`.
install-guest-payload:
	@test -x "$(GUEST_BIN)" || { \
		echo "error: guest payload is missing; run 'make guest-bin' without sudo first" >&2; \
		exit 1; \
	}
	$(INSTALL) -D -m 0755 "$(GUEST_BIN)" "$(DESTDIR)$(GUESTPAYLOADDIR)/hearth-guestd"

# Install the agent-plane host daemon unit (opt-in — the machine plane runs
# without it). Requires a `hearth-agent` system user and the secret source files
# LoadCredential= points at; see docs/agent-plane.md §4. Installed separately so
# `make install` stays machine-plane-only.
install-agentd: install-bin
	$(INSTALL) -D -m 0644 systemd/hearth-agentd.service $(DESTDIR)$(UNITDIR)/hearth-agentd.service
	@if [ -e "$(DESTDIR)$(CONFDIR)/verb-policy.toml" ]; then \
		echo "Preserved existing $(CONFDIR)/verb-policy.toml."; \
	else \
		$(INSTALL) -D -m 0644 systemd/hearth-agentd-verb-policy.toml "$(DESTDIR)$(CONFDIR)/verb-policy.toml"; \
	fi
	@echo "Installed hearth-agentd and its unit; ensured a verb policy file exists."
	@echo "If the policy already existed, confirm it has the hearth-agent rule from"
	@echo "systemd/hearth-agentd-verb-policy.toml before restarting hearthd."
	@echo "Next: create the hearth-agent user, stage $(CONFDIR)/agent/{http-token,ref-key},"
	@echo "      then: systemctl enable --now hearth-agentd.service"

install:
	@test -d "$(RELEASE_STAGE)/bin" || { \
		echo "error: release stage is missing; run 'devenv shell -- make release-stage' first" >&2; \
		exit 1; \
	}
	$(INSTALL) -D -m 0755 $(RELEASE_STAGE)/bin/hearthd $(DESTDIR)$(BINDIR)/hearthd
	$(INSTALL) -D -m 0755 $(RELEASE_STAGE)/bin/hearthctl $(DESTDIR)$(BINDIR)/hearthctl
	$(INSTALL) -D -m 0755 $(RELEASE_STAGE)/bin/hearth-agentd $(DESTDIR)$(BINDIR)/hearth-agentd
	$(INSTALL) -D -m 0755 $(RELEASE_STAGE)/lib/hearth/guest/hearth-guestd $(DESTDIR)$(GUESTPAYLOADDIR)/hearth-guestd
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/lib/hearth/kernel/vmlinux $(DESTDIR)$(LIBDIR)/hearth/kernel/vmlinux
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/lib/hearth/kernel/contract $(DESTDIR)$(LIBDIR)/hearth/kernel/contract
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/lib/systemd/system/hearth.service $(DESTDIR)$(UNITDIR)/hearth.service
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/lib/systemd/system/hearth-agentd.service $(DESTDIR)$(UNITDIR)/hearth-agentd.service
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/lib/sysusers.d/hearth.conf $(DESTDIR)$(LIBDIR)/sysusers.d/hearth.conf
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/lib/tmpfiles.d/hearth.conf $(DESTDIR)$(LIBDIR)/tmpfiles.d/hearth.conf
	$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/share/doc/hearth/operations.md $(DESTDIR)$(DOCDIR)/operations.md
	@if [ ! -e "$(DESTDIR)$(CONFDIR)/verb-policy.toml" ]; then \
		$(INSTALL) -D -m 0644 $(RELEASE_STAGE)/etc/hearth/verb-policy.toml "$(DESTDIR)$(CONFDIR)/verb-policy.toml"; \
	fi
	@if [ -z "$(DESTDIR)" ]; then \
		systemd-sysusers hearth.conf || true; systemd-tmpfiles --create hearth.conf || true; \
		systemctl daemon-reload || true; \
	fi
	@echo "Installed the staged Hearth tree. Units remain disabled."

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/hearthd $(DESTDIR)$(BINDIR)/hearthctl $(DESTDIR)$(BINDIR)/hearth-agentd
	rm -f $(DESTDIR)$(GUESTPAYLOADDIR)/hearth-guestd
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
