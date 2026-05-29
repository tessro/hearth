PREFIX      ?= /usr/local
BINDIR      ?= $(PREFIX)/bin
UNITDIR     ?= /etc/systemd/system
FIRMWAREDIR ?= /var/lib/hearth/firmware
FIRMWARE_URL ?= https://github.com/cloud-hypervisor/edk2/releases/download/ch-1e1b96f126/CLOUDHV.fd
BRIDGE      ?= hearth0

CARGO       ?= cargo
INSTALL     ?= install

.PHONY: build test clippy dev install uninstall firmware reload enable start stop restart status logs ping clean

build:
	$(CARGO) build --release

test:
	$(CARGO) test --release

clippy:
	$(CARGO) clippy --release --all-targets -- -D warnings

# Run hearthd in the foreground out of target/release. Override BRIDGE for a
# different bridge name.
dev: build
	sudo HEARTH_FIRMWARE=$(FIRMWAREDIR)/CLOUDHV.fd HEARTH_BRIDGE=$(BRIDGE) \
		./target/release/hearthd

install: build
	$(INSTALL) -d $(DESTDIR)$(BINDIR)
	$(INSTALL) -m 0755 target/release/hearthd   $(DESTDIR)$(BINDIR)/hearthd
	$(INSTALL) -m 0755 target/release/hearthctl $(DESTDIR)$(BINDIR)/hearthctl
	$(INSTALL) -d $(DESTDIR)$(UNITDIR)
	$(INSTALL) -m 0644 systemd/hearth.service $(DESTDIR)$(UNITDIR)/hearth.service
	systemctl daemon-reload

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/hearthd $(DESTDIR)$(BINDIR)/hearthctl
	rm -f $(DESTDIR)$(UNITDIR)/hearth.service
	systemctl daemon-reload

firmware:
	$(INSTALL) -d -o hearth -g hearth -m 0750 $(FIRMWAREDIR)
	curl -fsSL -o $(FIRMWAREDIR)/CLOUDHV.fd.tmp $(FIRMWARE_URL)
	$(INSTALL) -o hearth -g hearth -m 0640 $(FIRMWAREDIR)/CLOUDHV.fd.tmp $(FIRMWAREDIR)/CLOUDHV.fd
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
