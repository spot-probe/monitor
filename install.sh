#!/bin/sh
# Installs monitor-agent as a systemd or OpenRC service.
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --token TOKEN [options]
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --server URL --register KEY [options]
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --upgrade
#   curl -fsSL https://hub.example.com/install.sh | sh -s -- --uninstall
set -eu
# useradd and rc-update reside in sbin, which a root shell entered through `su`
# without `-` lacks on Debian: su keeps the caller's PATH unless ALWAYS_SET_PATH
# is set, and Debian does not set it.
PATH="$PATH:/usr/sbin:/sbin"

# Binary and token in one directory, the same one the hub uses, giving a node a
# single path to inspect and a single path to remove.
ROOT="/opt/monitor"
BIN="$ROOT/monitor-agent"
ENV_FILE="$ROOT/agent.env"
UNIT_FILE="/etc/systemd/system/monitor-agent.service"
RC_FILE="/etc/init.d/monitor-agent"
LOG_FILE="/var/log/monitor-agent.log"
SERVER=""
TOKEN=""
REGISTER=""
IFACE=""
IFACE_SET=""
INTERVAL=""
INSECURE=""
UNINSTALL=""
UPGRADE=""

while [ $# -gt 0 ]; do
	# A flag with no argument: under set -u, `$2` aborts with the shell's own
	# message rather than the usage below, and `shift 2` cannot proceed.
	case "$1" in
	--server | --token | --register | --iface | --interval)
		[ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; } ;;
	esac
	case "$1" in
	--server) SERVER="$2"; shift 2 ;;
	--token) TOKEN="$2"; shift 2 ;;
	--register) REGISTER="$2"; shift 2 ;;
	--iface) IFACE="$2"; IFACE_SET=1; shift 2 ;;
	--interval) INTERVAL="$2"; shift 2 ;;
	--insecure) INSECURE=1; shift ;;
	# 打开"允许 hub 远程升级这台机器上的 agent"。**与 --insecure 不同，它不该被
	# 机队共用的那条批量命令带上**，所以下面像 --interval/--iface 一样从旧 unit 读回并保留，
	# 另给一个显式关闭的参数 —— 只保留而不能关闭的话，这个开关就永远关不掉了。
	--allow-remote-upgrade) ALLOW_UPGRADE=1; shift ;;
	# 关闭写**空**而不是 "0"：下面三处用的是 ${ALLOW_UPGRADE:+…}，它只判"变量有没有设置"，
	# 于是 "0" 会被当成"开了" —— 那会让 --no-allow-remote-upgrade **关不掉**（参数被加回去、
	# ReadWritePaths 也照样写出来）。空 = 关，非空 = 开，三处就全对了。
	--no-allow-remote-upgrade) ALLOW_UPGRADE=""; ALLOW_UPGRADE_SET=1; shift ;;
	--uninstall) UNINSTALL=1; shift ;;
	--upgrade) UPGRADE=1; shift ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }

# Removes exactly what an install writes and nothing else, for both init
# systems: the one present now need not be the one the install found, and
# systemctl fails outright where systemd is not PID 1 (WSL, containers) although
# the install left its files there. Each step therefore tolerates failure. The
# hub may be installed in $ROOT as well, so the directory is removed only once
# empty.
if [ -n "$UNINSTALL" ]; then
	rc-service monitor-agent stop 2>/dev/null || true
	rc-update del monitor-agent default >/dev/null 2>&1 || true
	systemctl disable --now monitor-agent 2>/dev/null || true
	rm -f "$UNIT_FILE" "$RC_FILE" "$LOG_FILE" "$BIN" "$BIN.old" "$ENV_FILE"
	systemctl daemon-reload 2>/dev/null || true
	userdel monitor-agent 2>/dev/null || true
	rmdir "$ROOT" 2>/dev/null || true
	echo "monitor-agent uninstalled"
	exit 0
fi

# Reinstalls the binary from what this machine already holds. This is the one
# command a whole fleet can be upgraded with, because it carries no credential
# and names no node: the token and the hub address come from the env file, which
# only an install writes. Registration is never reached, so it can neither add a
# node nor spend a key, and a machine with nothing installed is told to use the
# panel's command rather than quietly becoming a new node.
if [ -n "$UPGRADE" ]; then
	[ -z "$TOKEN$REGISTER" ] ||
		{ echo "--upgrade 不接受 --token 或 --register：它沿用这台机器已有的凭证" >&2; exit 2; }
	TOKEN=$(sed -n 's/^MONITOR_TOKEN=//p' "$ENV_FILE" 2>/dev/null | tail -n 1)
	[ -n "$SERVER" ] || SERVER=$(sed -n 's/^MONITOR_SERVER=//p' "$ENV_FILE" 2>/dev/null | tail -n 1)
	# 写成 if 而不是 `A && B || C`：后者读起来一样，其实不然——第三个命令在第二个
	# 失败时也会跑，包括第一个本来就成立的情况。本仓已有五处为此改过。
	if [ -z "$TOKEN" ] || [ -z "$SERVER" ]; then
		echo "$ENV_FILE 里没有 token 与 hub 地址：这台机器上没有装过 agent。" >&2
		echo "请改用面板里生成的安装命令" >&2
		exit 2
	fi
fi

# A server with neither a token nor a registration key has no way to join. Written
# as one test rather than `A && B || C`, which reads as the same thing but is not:
# the third command runs whenever the second fails, including when the first did.
if [ -z "$SERVER" ] || { [ -z "$TOKEN" ] && [ -z "$REGISTER" ]; }; then
	echo "usage: install.sh --server URL (--token TOKEN | --register KEY) [--interval SECONDS] [--iface LIST] [--insecure] [--allow-remote-upgrade]" >&2
	echo "       install.sh --upgrade [--iface LIST] [--interval SECONDS] [--allow-remote-upgrade | --no-allow-remote-upgrade]" >&2
	echo "       install.sh --uninstall" >&2
	exit 2
fi
# A setting of this machine, kept by a rerun without the flag for the reason
# given for --iface below: the batch command carries none. It is read back from
# the service definition the last install wrote; a first install takes 1.
# 是否允许被远程升级：**这台机器的决定**，与机队共用的批量命令无关，所以与 --interval
# 完全同一套做法 —— 没给参数就从上次写的 unit 里读回；给了就以此为准（含显式关闭）。
if [ -z "${ALLOW_UPGRADE_SET:-}" ]; then
	KEPT=$(cat "$UNIT_FILE" "$RC_FILE" 2>/dev/null | sed -n \
		-e 's/^ExecStart=.*--allow-remote-upgrade.*/1/p' \
		-e 's/^command_args=".*--allow-remote-upgrade.*/1/p' | tail -n 1)
	if [ -n "$KEPT" ]; then
		ALLOW_UPGRADE=1
		echo "keeping --allow-remote-upgrade from the previous install"
	fi
fi

if [ -z "$INTERVAL" ]; then
	INTERVAL=$(cat "$UNIT_FILE" "$RC_FILE" 2>/dev/null | sed -n \
		-e 's/^ExecStart=.* --interval \([0-9][0-9]*\).*/\1/p' \
		-e 's/^command_args="--interval \([0-9][0-9]*\).*/\1/p' | tail -n 1)
	if [ -n "$INTERVAL" ]; then echo "keeping --interval $INTERVAL from the previous install"; else INTERVAL=1; fi
fi
case "$INTERVAL" in "" | *[!0-9]*) echo "interval must be an integer from 1 to 3600" >&2; exit 2 ;; esac
if [ "$INTERVAL" -lt 1 ] || [ "$INTERVAL" -gt 3600 ]; then
	echo "interval must be from 1 to 3600" >&2
	exit 2
fi
# Which interfaces carry this machine's traffic is known only on the machine,
# and the batch command a fleet shares cannot carry one value per machine. A
# rerun without --iface, the documented upgrade, therefore keeps the value in
# the env file; --iface '' clears it.
#
# A kept value is written back as found, the last assignment being the one
# systemd and OpenRC apply: root wrote it, both already read it, and a hand edit
# with quotes must not block every later upgrade. A value given here is held to
# what the agent accepts -- full names separated by commas, each optionally led
# by one `-` -- since the agent refuses anything else at startup and would
# restart forever while this script reported success. The character set also
# keeps it inert where OpenRC sources the file as shell.
if [ -z "$IFACE_SET" ]; then
	IFACE=$(sed -n 's/^MONITOR_IFACE=//p' "$ENV_FILE" 2>/dev/null | tail -n 1)
	[ -z "$IFACE" ] || echo "keeping --iface $IFACE from the previous install"
else
	case ",$IFACE," in
	*[!A-Za-z0-9._,-]* | *,-,* | *,--*)
		echo "--iface takes full interface names separated by commas, each optionally led by -, not: $IFACE" >&2
		exit 2
		;;
	esac
fi
# A bare host implies TLS, matching the upgrade the agent's ws_url() performs,
# and the same reversal under --insecure where the hub has no TLS to upgrade to.
# Without this the two diverge: the agent would dial wss:// while curl below
# defaults a scheme-less URL to http://, fetching over plaintext the binary about
# to run as root.
if [ -n "$INSECURE" ]; then SCHEME=http; else SCHEME=https; fi
case "$SERVER" in *://*) ;; *) SERVER="$SCHEME://$SERVER" ;; esac
# The agent already refuses plaintext ws:// to a remote hub, since the token
# would travel in the clear. The same address fetches the binary about to run as
# root here, so the same rule applies: over plain HTTP anyone on the path can
# substitute a binary of their own.
#
# --insecure overrides both halves for a hub reached at ip:port with no TLS in
# front, and says so explicitly: this is the one step of the install that cannot
# be corrected afterwards, because a substituted binary is already running as
# root by then.
#
# The test applies to the host alone, with scheme, port and path removed, and
# matches an address rather than a prefix: `127.evil.com` is a registered name
# resolving wherever its owner points it, and reading it as loopback would hand
# this plaintext channel to that owner.
HOST="${SERVER#*://}"
HOST="${HOST%%/*}"
# RFC 3986 places userinfo before the host, so `127.0.0.1:28080@evil.example.com`
# leaves a loopback address where the test below looks while curl, which parses
# the URL correctly, fetches from the owner of that name -- over plain HTTP, with
# the bytes installed 0755 and started as root a few lines below. A hub address
# never requires userinfo; the agent's own ws_url rejects it as well.
case "$HOST" in
*@*) echo "server URL must not contain '@': the host is whatever follows it" >&2; exit 2 ;;
esac
case "$HOST" in
"["*) HOST="${HOST#\[}"; HOST="${HOST%%]*}" ;;
*) HOST="${HOST%%:*}" ;;
esac
# A full dotted quad in 127/8 and nothing shorter, matching what the agent's
# is_loopback() accepts, since Rust's IpAddr parser accepts nothing shorter
# either -- `127.1` is a name to it, not an address. The two must agree, or this
# installs over plaintext against a hub the agent then refuses to dial: the unit
# is written, the service started, and it crash-loops on RestartSec while this
# script has reported success.
#
# The first arm excludes anything containing a letter, which is a registered name
# however it begins, and anything with more than four components, which is not an
# address.
case "$HOST" in
localhost | ::1) LOCAL=1 ;;
*[!0-9.]* | *.*.*.*.*) LOCAL="" ;;
127.[0-9]*.[0-9]*.[0-9]*) LOCAL=1 ;;
*) LOCAL="" ;;
esac
case "$SERVER" in
http://*)
	if [ -z "$LOCAL" ]; then
		[ -n "$INSECURE" ] || {
			echo "refusing plaintext http:// to a remote hub; use https://, or --insecure if it has no TLS" >&2
			exit 2
		}
		echo "warning: --insecure over plain HTTP to $SERVER" >&2
		echo "         the token and every report travel in the clear, and the binary" >&2
		echo "         installed below is fetched over the same unverified channel" >&2
	fi
	;;
esac
if command -v systemctl >/dev/null; then
	INIT=systemd
elif command -v rc-update >/dev/null; then
	INIT=openrc
else
	echo "this installer needs systemd or OpenRC" >&2
	exit 1
fi

# The service user the unit below runs as, created before the download and the
# registration, so a host where this fails keeps the agent it already runs and
# spends no registration key. OpenRC has no equivalent and Alpine ships no
# useradd, which is why this is confined to systemd.
if [ "$INIT" = systemd ]; then
	id -u monitor-agent >/dev/null 2>&1 ||
		useradd --system --no-create-home --shell /usr/sbin/nologin monitor-agent ||
		{ echo "cannot create the system user monitor-agent" >&2; exit 1; }
fi

case "$(uname -m)" in
x86_64 | amd64) ARCH=x86_64 ;;
aarch64 | arm64) ARCH=aarch64 ;;
*) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# The hub relays the binary, so a node need only reach the hub it already talks
# to: an IPv6-only or blocked machine cannot resolve github.com. A hub unable to
# fetch releases itself is configured with a GitHub proxy in its own settings,
# which is why none is requested here.
URL="${SERVER%/}/agent/$ARCH"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

echo "downloading monitor-agent ($ARCH)"
# The hub relays four downloads at once and queues the rest for 30 seconds. A
# batch run on more machines than drain in that time is turned away with 503,
# which is retried here rather than failing the machine. Any other refusal is
# final and shown with the hub's own reason, which --fail would discard.
TRIES=0
while :; do
	CODE=$(curl -sSL --max-time 300 -w '%{http_code}' "$URL" -o "$TMP") || exit 1
	# Anything other than a queue-full refusal is final; a 503 is retried five times.
	if [ "$CODE" != 503 ] || [ "$TRIES" -ge 5 ]; then
		break
	fi
	TRIES=$((TRIES + 1))
	echo "the hub is busy relaying to other machines; retrying in 5 seconds"
	sleep 5
done
[ "$CODE" = 200 ] ||
	{ printf 'download failed (HTTP %s): %s\n' "$CODE" "$(head -n 1 "$TMP" | cut -c1-500)" >&2; exit 1; }
# A relay can answer 200 with something other than the program, such as a
# mirror's error page. Checked before the running agent is stopped, so a batch
# run through such a relay leaves each machine on the agent it had, rather than
# on bytes that cannot start while this script reports success.
[ "$(head -c 4 "$TMP")" = "$(printf '\177ELF')" ] ||
	{ echo "the download is not a Linux executable: $(head -n 1 "$TMP" | tr -cd '[:print:]' | cut -c1-200)" >&2; exit 1; }

# Downloaded before the registration below, because that step spends a node: the
# key returns a token and the panel gains a row, while the env file recording it
# is only written once the binary is in place. A download that fails after
# registering therefore leaves an unusable node behind, and the rerun -- the
# documented way to recover -- registers a second one.
#
# --register exchanges a key for this node's own token, which is what allows one
# command to provision a batch of machines. The key is valid only within the
# window the panel opened and never becomes the credential the agent runs with.
if [ -z "$TOKEN" ]; then
	# Re-running the same command must not add a second node. This machine's
	# token is already present and outlives the window that issued it, so the env
	# file answers before the hub is consulted.
	#
	# Only for the same hub: a token issued by hub A means nothing to hub B, and
	# retaining it would leave the agent authenticating indefinitely against a
	# node that was never created, with this installer reporting success.
	CACHED=$(sed -n 's/^MONITOR_SERVER=//p' "$ENV_FILE" 2>/dev/null || true)
	if [ "${CACHED%/}" = "${SERVER%/}" ]; then
		TOKEN=$(sed -n 's/^MONITOR_TOKEN=//p' "$ENV_FILE" 2>/dev/null || true)
		if [ -n "$TOKEN" ]; then
			echo "this machine is already registered; keeping its token"
		fi
	fi
fi
if [ -z "$TOKEN" ]; then
	# The hub trims and bounds this as well; here it is restricted to characters
	# a hostname may contain, so nothing unexpected travels in the body.
	NAME=$(hostname 2>/dev/null | tr -cd 'A-Za-z0-9._-' | cut -c1-64)
	echo "registering $NAME with the hub"
	TOKEN=$(curl -fsS --max-time 30 -H "Authorization: Bearer $REGISTER" \
		--data-binary "$NAME" "${SERVER%/}/api/agent/register") || {
		echo "the hub refused the registration key: the window may have closed," >&2
		echo "the key may be wrong, or it has registered enough nodes already." >&2
		echo "open a new one from the panel's node list." >&2
		exit 1
	}
fi

# Stop an agent already running here before replacing its binary. The service
# name is fixed, so a reinstall could never start a second copy, but without this
# the new binary lands beneath a live process and only the restart at the end
# picks it up. Stopping first also means the copy does not depend on `install`
# unlinking rather than failing with ETXTBSY. Placed after the download, so a
# node that cannot fetch the binary keeps running.
if [ "$INIT" = openrc ]; then
	rc-service monitor-agent stop 2>/dev/null || true
else
	systemctl stop monitor-agent 2>/dev/null || true
fi
install -d -m 0755 "$ROOT"
# Kept until the new binary has proved it starts; see not_started. Never over
# an existing copy: a run that died before that check left an unproven binary
# in $BIN, and the copy is the one that ran before it.
[ ! -f "$BIN" ] || [ -f "$BIN.old" ] || cp "$BIN" "$BIN.old"
install -m 0755 "$TMP" "$BIN"

# The token lives in a root-only environment file rather than the unit, keeping
# it out of `systemctl cat` and the world-readable journal. 0600 root is what
# keeps it private, since $ROOT itself is readable and holds the binaries. Set in
# a subshell, because the unit file written below is read by anyone debugging
# with `systemctl cat` and need not be 0600.
(
	umask 077
	cat >"$ENV_FILE" <<ENV
MONITOR_SERVER=$SERVER
MONITOR_TOKEN=$TOKEN
ENV
	[ -z "$IFACE" ] || printf 'MONITOR_IFACE=%s\n' "$IFACE" >>"$ENV_FILE"
)

# The new agent is not running. The binary it replaced is put back and started
# again, so a failed upgrade leaves the machine reporting as before; the unit
# and env file just written suit that binary as well, since an upgrade keeps the
# token and the settings. A first install has nothing to put back.
not_started() {
	echo "monitor-agent did not start; see: $1" >&2
	[ -f "$BIN.old" ] || exit 1
	mv -f "$BIN.old" "$BIN"
	if [ "$INIT" = openrc ]; then
		rc-service monitor-agent restart >/dev/null 2>&1 || true
	else
		systemctl restart monitor-agent || true
	fi
	echo "the previous monitor-agent binary is back in place and was restarted" >&2
	exit 1
}

# "Active" only means the process did not exit: an agent with a wrong token or
# address stays active while it redials forever, and the log says `connected to`
# only once it actually got through. So the process check above cannot tell
# "reporting" from "retrying" -- this waits for the first connection.
wait_for_connected() {
	wait_deadline=$(( $(date +%s) + 10 ))
	while [ "$(date +%s)" -lt "$wait_deadline" ]; do
		if [ "$INIT" = openrc ]; then
			tail -n +"$1" "$LOG_FILE" 2>/dev/null | grep -q 'connected to' && return 0
		else
			journalctl -u monitor-agent --since "$START_STAMP" --no-pager 2>/dev/null | grep -q 'connected to' && return 0
		fi
		sleep 1
	done
	return 1
}

# What to say at the end, and it says what was *checked*: the process, whether it
# reached the hub, and the command that proves ICMP works without the hub at all.
report_started() {
	report_base="$1"
	report_hint="$2"
	if [ "$INIT" = openrc ]; then
		report_pid=$(pidof monitor-agent 2>/dev/null || echo unknown)
	else
		report_pid=$(systemctl show -p MainPID --value monitor-agent 2>/dev/null || echo unknown)
	fi
	echo "monitor-agent is running (pid $report_pid)"
	if wait_for_connected "$report_base"; then
		echo "connected to $SERVER"
	else
		echo "warning: running, but it did not reach $SERVER within 10s -- see: $report_hint" >&2
	fi
	echo "check it by hand (no hub needed): $BIN --ping 1.1.1.1"
	# ICMP needs either this range to cover the agent's gid or CAP_NET_RAW on the
	# service. `1 0` is the kernel default and allows nobody -- which is what a
	# real machine turned out to have.
	report_gid=$(id -g monitor-agent 2>/dev/null || echo "")
	range=$(sysctl -n net.ipv4.ping_group_range 2>/dev/null || echo "")
	if [ -n "$report_gid" ] && [ -n "$range" ]; then
		# Deliberate: this is word splitting, not a missing quote -- `"1 0"` has to
		# become two words.
		# shellcheck disable=SC2086
		set -- $range
		report_low=$1
		report_high=$2
		if [ -n "$report_low" ] && [ -n "$report_high" ] && { [ "$report_gid" -lt "$report_low" ] || [ "$report_gid" -gt "$report_high" ]; }; then
			echo "warning: net.ipv4.ping_group_range is \"$range\" and does not cover monitor-agent's gid ($report_gid);" >&2
			echo "         ICMP probes will report a permission problem until it does (see the docs)" >&2
		fi
	fi
}

if [ "$INIT" = openrc ]; then
	cat >"$RC_FILE" <<RC
#!/sbin/openrc-run
description="monitor agent"
command="$BIN"
command_args="--interval $INTERVAL${INSECURE:+ --insecure}${ALLOW_UPGRADE:+ --allow-remote-upgrade}"
supervisor="supervise-daemon"
respawn_delay=5
output_log="$LOG_FILE"
error_log="$LOG_FILE"

depend() {
	need net
}

# The token stays in the root-only env file rather than the service script.
start_pre() {
	set -a
	. $ENV_FILE
	set +a
}
RC
	chmod 0755 "$RC_FILE"
	rc-update add monitor-agent default >/dev/null
LOG_BASE=$(( $(wc -l <"$LOG_FILE" 2>/dev/null || echo 0) + 1 ))
	rc-service monitor-agent restart
	# supervise-daemon reports the service started while it respawns an agent
	# that exits at once, so the process itself is what is looked for, inside
	# the respawn delay. pidof rather than pgrep -x, which BusyBox matches
	# against the full path.
	sleep 3
	pidof monitor-agent >/dev/null || not_started "$LOG_FILE"
	rm -f "$BIN.old"
report_started "$LOG_BASE" "$LOG_FILE"
	exit 0
fi

# 打开远程升级时，把**二进制所在目录**交给服务用户。
#
# 为什么需要：`apply.rs` 刻意把暂存文件放在二进制旁边（同一个文件系统，最后的 rename 才是
# 原子的；而 PrivateTmp=yes 又让 /tmp 不可用）。unit 里的 ProtectSystem=strict 让 /opt 只读
# —— 那个已经由 ReadWritePaths= 解决；但目录本身归 root，服务用户只有 r-x，于是**建不了文件**：
# 实测报错 "写 /opt/monitor/monitor-agent.new: Permission denied (os error 13)"。
#
# **rename 只需要目录的写权限**，所以只改目录，二进制仍归 root（最小改动）。
# 安全上不提权：agent 本来就以这个用户运行，这条唯一多给它的能力是"能替换自己的二进制"，
# 而那正是这个功能的定义 —— 且只在打开开关的机器上给。
[ -n "${ALLOW_UPGRADE:-}" ] && chown monitor-agent "$(dirname "$BIN")"

cat >"$UNIT_FILE" <<UNIT
[Unit]
Description=monitor agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$ENV_FILE
ExecStart=$BIN --interval $INTERVAL${INSECURE:+ --insecure}${ALLOW_UPGRADE:+ --allow-remote-upgrade}
Restart=always
RestartSec=5
# A fixed user rather than DynamicUser=: when the mount namespace cannot be
# created, as in an LXC container without nesting, systemd skips ProtectSystem=
# and the other mount sandboxing for a unit with a static User=, but refuses to
# start one with DynamicUser= and exits 226/NAMESPACE. DynamicUser= also implied
# RestrictSUIDSGID=, which is therefore stated below.
User=monitor-agent
NoNewPrivileges=yes
RestrictSUIDSGID=yes
ProtectSystem=strict
# 远程升级要**在二进制旁边**写一个暂存文件（同一个文件系统 → 最后的 rename 才是原子的；
# PrivateTmp=yes 让 /tmp 也不可用，所以不能写那儿）。而 ProtectSystem=strict 把 /opt 变成只读，
# 于是那一步会以 EROFS 失败 —— 实测到的报错就是它。
#
# 所以**只在打开这个开关时**给那个目录开一个写口：没开的机器保持严格只读，
# 也就是说**能力与写权限由同一个开关控制**，而这个开关只有本机能改（hub 读得到、写不了）。
${ALLOW_UPGRADE:+ReadWritePaths=$(dirname "$BIN")}
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
# AF_NETLINK is how getifaddrs(3) obtains this host's own addresses from the
# kernel; without it the agent reports none.
RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK
MemoryMax=64M

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable monitor-agent >/dev/null
START_STAMP=$(date '+%Y-%m-%d %H:%M:%S')
# restart rather than `enable --now`: --now leaves an already-running service
# untouched, so reinstalling over a live agent would keep the old binary
# running.
systemctl restart monitor-agent
# Type=simple counts the service started once it is forked, so `restart` above
# succeeds also for one that fails at once -- a user it cannot resolve
# (217/USER), a binary that exits -- and is then restarted every RestartSec.
# Checked inside that window, so a batch run shows the failure on the machine
# where it happened rather than a line reading "installed".
sleep 3
systemctl is-active --quiet monitor-agent || not_started "journalctl -u monitor-agent -n 20"
rm -f "$BIN.old"
report_started 1 "journalctl -u monitor-agent -n 20"
