#!/bin/sh
set -eu
systemd-sysusers hearth.conf >/dev/null 2>&1 || systemd-sysusers >/dev/null 2>&1 || true
systemd-tmpfiles --create hearth.conf >/dev/null 2>&1 || systemd-tmpfiles --create >/dev/null 2>&1 || true
systemctl daemon-reload >/dev/null 2>&1 || true
