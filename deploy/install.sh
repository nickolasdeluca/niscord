#!/usr/bin/env bash
# Installs a Niscord server on Ubuntu/Debian:
#
#   * niscord-server  signaling, as a systemd service on 127.0.0.1:8080
#   * Caddy           TLS for wss://<domain> (Let's Encrypt), proxying to it
#   * coturn          STUN/TURN relay for friends who can't connect directly
#
# Run it from a copy of the source tree; it builds the server unless you pass
# a ready binary. Safe to re-run (e.g. to update): the password and TURN
# secret are kept unless you pass new ones.
#
#   sudo deploy/install.sh --domain niscord.example.com [--password PW] [--binary PATH]

set -euo pipefail

RELAY_MIN=49160
RELAY_MAX=49200
ENV_FILE=/etc/niscord/niscord.env

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
}
step() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
    printf '\033[1;31merror:\033[0m %s\n' "$*" >&2
    exit 1
}

DOMAIN=""
PASSWORD=""
BINARY=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --domain) DOMAIN="${2:-}"; shift 2 ;;
        --password) PASSWORD="${2:-}"; shift 2 ;;
        --binary) BINARY="${2:-}"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

[[ $EUID -eq 0 ]] || die "run this with sudo"
[[ -n $DOMAIN ]] || die "--domain is required (see --help)"
[[ $DOMAIN =~ ^[A-Za-z0-9.-]+$ ]] || die "--domain should be a bare host name like niscord.example.com"
[[ $PASSWORD != *[\"\\\$\`]* && $PASSWORD != *$'\n'* ]] || die "the password can't contain quotes, \$, \` or backslashes"
command -v apt-get > /dev/null || die "this script supports Ubuntu/Debian (apt) only"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------------------------------------------------------------------------
step "Installing packages (coturn, Caddy)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q curl ca-certificates gnupg openssl coturn debian-keyring debian-archive-keyring apt-transport-https
if ! command -v caddy > /dev/null; then
    # Caddy's official repository (https://caddyserver.com/docs/install#debian-ubuntu-raspbian).
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' |
        gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
    apt-get update -q
    apt-get install -y -q caddy
fi

# ---------------------------------------------------------------------------
if [[ -n $BINARY ]]; then
    step "Installing $BINARY"
    [[ -f $BINARY ]] || die "no such file: $BINARY"
    install -m 0755 "$BINARY" /usr/local/bin/niscord-server
else
    step "Building niscord-server from $SRC"
    [[ -f $SRC/Cargo.toml ]] || die "run this from a Niscord source tree, or pass --binary"
    apt-get install -y -q build-essential pkg-config
    # Small VMs (1 GB) run out of memory linking the optimised build.
    MEM_MB=$(awk '/MemTotal/ {print int($2 / 1024)}' /proc/meminfo)
    if [[ $MEM_MB -lt 2000 && -z $(swapon --noheadings) ]]; then
        echo "Only ${MEM_MB} MB of memory: adding 2 GB of swap at /swapfile for the build"
        if [[ ! -f /swapfile ]]; then
            fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048
            chmod 600 /swapfile
            mkswap /swapfile > /dev/null
        fi
        swapon /swapfile
        grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
    fi
    # Build as the user who ran sudo, with their own rustup toolchain (the
    # distribution's cargo is usually too old for edition 2024).
    BUILD_USER="${SUDO_USER:-root}"
    run_as() { sudo -u "$BUILD_USER" -H bash -c "$1"; }
    if ! run_as 'test -x "$HOME/.cargo/bin/cargo"'; then
        run_as 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
    fi
    run_as '"$HOME/.cargo/bin/rustup" update stable --no-self-update'
    run_as "cd '$SRC' && \"\$HOME/.cargo/bin/cargo\" build --release --locked -p niscord-server"
    install -m 0755 "$SRC/target/release/niscord-server" /usr/local/bin/niscord-server
fi

# ---------------------------------------------------------------------------
step "Checking the network"
PUBLIC_IP="$(curl -4 -fsS --max-time 10 https://api.ipify.org || true)"
[[ -n $PUBLIC_IP ]] || die "couldn't find this server's public IPv4 address"
LOCAL_IP="$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}')"
EXTERNAL_IP_LINE=""
if [[ $PUBLIC_IP != "$LOCAL_IP" ]]; then
    # Cloud VPSes often sit behind 1:1 NAT; coturn must advertise the public address.
    EXTERNAL_IP_LINE="external-ip=$PUBLIC_IP/$LOCAL_IP"
    echo "Behind NAT: public $PUBLIC_IP, local $LOCAL_IP"
else
    echo "Public IP: $PUBLIC_IP"
fi
RESOLVED="$(getent ahostsv4 "$DOMAIN" | awk 'NR == 1 {print $1}' || true)"
if [[ $RESOLVED != "$PUBLIC_IP" ]]; then
    warn "$DOMAIN resolves to '${RESOLVED:-nothing}', not $PUBLIC_IP. Point its DNS A record here;"
    warn "Caddy can't get a TLS certificate until it does (it keeps retrying)."
fi

# ---------------------------------------------------------------------------
# Before Caddy starts: it needs ports 80/443 to get its certificate.
if command -v ufw > /dev/null && ufw status | grep -q 'Status: active'; then
    step "Opening firewall ports (ufw)"
    ufw allow 80/tcp > /dev/null
    ufw allow 443/tcp > /dev/null
    ufw allow 3478/udp > /dev/null
    ufw allow "$RELAY_MIN:$RELAY_MAX/udp" > /dev/null
elif command -v iptables > /dev/null && iptables -S INPUT | grep -q -- '-j REJECT'; then
    # Oracle Cloud's Ubuntu images reject everything but SSH with their own
    # iptables rules (saved by netfilter-persistent): accept our ports ahead
    # of the REJECT, and save so they survive a reboot.
    step "Opening firewall ports (iptables)"
    allow() {
        local reject
        reject=$(iptables -L INPUT --line-numbers -n | awk '$2 == "REJECT" {print $1; exit}')
        iptables -C INPUT -p "$1" --dport "$2" -m state --state NEW -j ACCEPT 2> /dev/null ||
            iptables -I INPUT "$reject" -p "$1" --dport "$2" -m state --state NEW -j ACCEPT
    }
    allow tcp 80
    allow tcp 443
    allow udp 3478
    allow udp "$RELAY_MIN:$RELAY_MAX"
    if command -v netfilter-persistent > /dev/null; then
        netfilter-persistent save > /dev/null 2>&1 || warn "couldn't save the iptables rules; they reset on reboot"
    else
        warn "netfilter-persistent isn't installed; the iptables rules reset on reboot"
    fi
fi

# ---------------------------------------------------------------------------
step "Writing $ENV_FILE"
OLD_PASSWORD=""
OLD_SECRET=""
if [[ -f $ENV_FILE ]]; then
    OLD_PASSWORD="$(sed -n 's/^NISCORD_PASSWORD="\(.*\)"$/\1/p' "$ENV_FILE")"
    OLD_SECRET="$(sed -n 's/^NISCORD_TURN_SECRET="\(.*\)"$/\1/p' "$ENV_FILE")"
fi
PASSWORD="${PASSWORD:-${OLD_PASSWORD:-$(openssl rand -base64 24 | tr -dc 'A-Za-z0-9' | cut -c1-16)}}"
TURN_SECRET="${OLD_SECRET:-$(openssl rand -hex 32)}"
install -d -m 0755 /etc/niscord
umask 077
cat > "$ENV_FILE" << EOF
# Niscord server settings (see the README). Restart after editing:
#   sudo systemctl restart niscord-server
NISCORD_BIND="127.0.0.1:8080"
NISCORD_PASSWORD="$PASSWORD"
NISCORD_STUN_URLS="stun:$DOMAIN:3478"
# UDP only: Niscord's WebRTC stack doesn't use TURN over TCP.
NISCORD_TURN_URLS="turn:$DOMAIN:3478?transport=udp"
NISCORD_TURN_SECRET="$TURN_SECRET"
RUST_LOG="info"
EOF
umask 022

# ---------------------------------------------------------------------------
step "Configuring coturn"
[[ -f /etc/turnserver.conf && ! -f /etc/turnserver.conf.orig ]] && cp /etc/turnserver.conf /etc/turnserver.conf.orig
cat > /etc/turnserver.conf << EOF
# Written by Niscord's deploy/install.sh (the original is in turnserver.conf.orig).
listening-port=3478
no-tcp
fingerprint
use-auth-secret
static-auth-secret=$TURN_SECRET
realm=$DOMAIN
min-port=$RELAY_MIN
max-port=$RELAY_MAX
$EXTERNAL_IP_LINE
# Plain TURN only: WebRTC encrypts the media itself.
no-tls
no-dtls
no-cli
no-multicast-peers
# Never relay into this machine or private networks.
denied-peer-ip=0.0.0.0-0.255.255.255
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=100.64.0.0-100.127.255.255
denied-peer-ip=127.0.0.0-127.255.255.255
denied-peer-ip=169.254.0.0-169.254.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
denied-peer-ip=::1
denied-peer-ip=fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
denied-peer-ip=fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff
simple-log
EOF
chmod 0640 /etc/turnserver.conf
chown root:turnserver /etc/turnserver.conf 2> /dev/null || true
# Older packages ship disabled.
[[ -f /etc/default/coturn ]] && sed -i 's/^#\?TURNSERVER_ENABLED=.*/TURNSERVER_ENABLED=1/' /etc/default/coturn
systemctl enable coturn > /dev/null 2>&1
systemctl restart coturn

# ---------------------------------------------------------------------------
step "Configuring Caddy"
SITE=/etc/caddy/niscord.caddy
cat > "$SITE" << EOF
# Niscord: TLS for wss://$DOMAIN, proxied to the signaling server.
$DOMAIN {
	reverse_proxy 127.0.0.1:8080
}
EOF
if grep -q 'The Caddyfile is an easy way' /etc/caddy/Caddyfile 2> /dev/null || [[ ! -s /etc/caddy/Caddyfile ]]; then
    # Still the stock welcome-page config: replace it.
    echo "import $SITE" > /etc/caddy/Caddyfile
elif ! grep -qF "import $SITE" /etc/caddy/Caddyfile; then
    printf '\nimport %s\n' "$SITE" >> /etc/caddy/Caddyfile
fi
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile > /dev/null
systemctl enable caddy > /dev/null 2>&1
systemctl reload-or-restart caddy

# ---------------------------------------------------------------------------
step "Starting niscord-server"
install -m 0644 "$SRC/deploy/niscord-server.service" /etc/systemd/system/niscord-server.service
systemctl daemon-reload
systemctl enable niscord-server > /dev/null 2>&1
systemctl restart niscord-server

# ---------------------------------------------------------------------------
step "Checking"
sleep 2
for unit in niscord-server coturn caddy; do
    if systemctl is-active --quiet "$unit"; then
        echo "  $unit: running"
    else
        warn "$unit is not running; see: journalctl -u $unit -n 50"
    fi
done

cat << EOF

Done. Friends connect with:

  Server:    wss://$DOMAIN
  Password:  $PASSWORD

Make sure your VPS provider's firewall (security group / Oracle security list) allows:
  TCP 80, 443         Caddy (TLS certificate + wss://)
  UDP 3478            STUN/TURN
  UDP $RELAY_MIN-$RELAY_MAX     TURN relay

Logs:      journalctl -u niscord-server -f
Settings:  $ENV_FILE
EOF
