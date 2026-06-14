#!/bin/sh
set -eu

iface=
for path in /sys/class/net/*; do
    name=${path##*/}
    [ "$name" = lo ] && continue
    iface=$name
    break
done

if [ -z "$iface" ]; then
    echo "no network interface found" >&2
    exit 2
fi

echo "using network interface $iface"
ip link set "$iface" up
udhcpc -i "$iface" -q -n
ip addr show "$iface"

exec python3 -m http.server 8000 --bind 0.0.0.0
