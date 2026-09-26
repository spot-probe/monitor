#!/bin/sh
# monitor hub installer.
#
#   curl -fsSL https://raw.githubusercontent.com/spot-probe/monitor/main/install-hub.sh -o install-hub.sh
#   chmod +x install-hub.sh
#   sudo ./install-hub.sh
#
# Menu-driven when a terminal is available. A plain `curl ... | sh` has no
# terminal to read answers from, so it installs with the defaults rather than
# waiting on an invisible prompt.
set -eu
# useradd resides in sbin, which a root shell entered through `su` without `-`
# lacks on Debian: su keeps the caller's PATH unless ALWAYS_SET_PATH is set, and
# Debian does not set it.
PATH="$PATH:/usr/sbin:/sbin"

# This fork's own releases, not upstream's: the binary carries this fork's embedded
# theme and its node grouping, so installing upstream's build would be a different
# program wearing the same name.
REPO="spot-probe/monitor"
# The default theme ships from its own repository and its own release, which is
# what lets a theme change ship without a hub release. A fresh install fetches
# the current one into the themes directory; the copy embedded in the binary
# stays as the fallback for an install with no network.
THEME_REPO="spot-probe/monitor-theme-default"
SERVICE="monitor-hub"
UNIT="/etc/systemd/system/monitor-hub.service"
# Everything but the unit lives under one directory: the two binaries at the top,
# and everything the hub writes -- database and themes/ -- under data/. One path
# to back up, one to move to another host, and the same split the container image
# uses, where data/ is mounted at /data.
ROOT="/opt/monitor"
BIN="$ROOT/monitor-hub"
DATA="$ROOT/data"
# A fixed data directory requires a fixed owner: DynamicUser= selects its uid at
# start, and a recycled one would leave the database unreadable.
USER_NAME="monitor"
PORT="28080"
PORT_SET=""
SITE=""
# Empty means "whatever the theme repository's latest release is"; --theme-tag
# pins one. --no-theme skips the fetch entirely, which is what an air-gapped
# install wants: the embedded copy needs nothing.
THEME_TAG=""
THEME_SKIP=""
SITE_SET=""
YES=""
PURGE=""
ACTION=""

# ---- ui ----
# Colour only to a terminal, and never when NO_COLOR is set: the output of a
# piped run belongs in a log rather than in escape sequences.
if [ -t 1 ] && [ -z "${NO_COLOR-}" ]; then
	B="$(printf '\033[1m')" D="$(printf '\033[2m')" N="$(printf '\033[0m')"
	G="$(printf '\033[32m')" R="$(printf '\033[31m')" Y="$(printf '\033[33m')"
else
	B="" D="" N="" G="" R="" Y=""
fi

rule() { printf '  %s────────────────────────────────────────────%s\n' "$D" "$N"; }

banner() {
	if [ -t 1 ]; then printf '\033[H\033[2J'; fi
	printf '\n  %smonitor hub%s  %s·%s  安装器\n' "$B" "$N" "$D" "$N"
	rule
	printf '\n'
}

# Every label below is deliberately two CJK characters wide: printf pads by byte
# count, so any other width would break the column.
ok() { printf '  %s✓%s  %s    %s%s%s\n' "$G" "$N" "$1" "$D" "${2-}" "$N"; }
field() { printf '  %s%s%s    %s\n' "$D" "$1" "$N" "$2"; }
warn() { printf '  %s!%s  %s\n' "$Y" "$N" "$1"; }
die() { printf '  %s✗%s  %s\n' "$R" "$N" "$1" >&2; exit 1; }

# A default answer on Enter, and the same default when there is no terminal.
ask() {
	if [ ! -t 0 ]; then printf '%s' "$2"; return; fi
	printf '  %s?%s  %s %s[%s]%s ' "$Y" "$N" "$1" "$D" "$2" "$N" >&2
	read -r reply || reply=""
	printf '%s' "${reply:-$2}"
}

confirm() {
	if [ -n "$YES" ]; then return 0; fi
	if [ ! -t 0 ]; then die "$1（非交互运行时加 --yes 确认）"; fi
	printf '  %s?%s  %s  %s[y/N]%s ' "$Y" "$N" "$1" "$D" "$N"
	read -r reply || reply=""
	case "$reply" in y | Y | yes) return 0 ;; *) printf '  已取消\n'; return 1 ;; esac
}

press() {
	if [ ! -t 0 ]; then return 0; fi
	printf '\n  %s回车返回菜单%s ' "$D" "$N"
	read -r _ || true
}

check_port() {
	case "$1" in "" | *[!0-9]*) die "端口必须是 1-65535 的整数：$1" ;; esac
	[ "$1" -ge 1 ] && [ "$1" -le 65535 ] || die "端口必须是 1-65535 的整数：$1"
}

# The `--listen ...` tail of the installed unit's ExecStart, empty when nothing
# is installed. An upgrade rewrites the unit, so anything not supplied on the
# command line must be recovered from the old one, or re-running to upgrade
# would silently reset the port and drop --site.
old_exec() { sed -n 's/^ExecStart=.*--listen //p' "$UNIT" 2>/dev/null || true; }

# The port that unit listens on, empty when there is none. Read twice: once for
# the carry-over and once for the default the menu offers, since pressing Enter
# there must leave a running deployment unchanged.
old_port() {
	listen="$(old_exec)"
	case "$listen" in
	*:[0-9]*) listen="${listen%% *}"; printf '%s' "${listen##*:}" ;;
	esac
}

# ---- install ----
# The default theme's release, unpacked into <data>/themes/default/.
#
# That directory is exactly the layout the hub reads an installed theme from, so
# the public page serves it from the next request on -- and it is also what makes
# the theme updatable later from the panel.
#
# Every failure here is a warning rather than a stop. The binary already carries
# a theme, an installer that died over an optional download would be an installer
# that cannot run offline at all, and the operator would be left with no hub
# rather than an older theme.
fetch_theme() {
	command -v tar >/dev/null 2>&1 || { warn "没有 tar，跳过主题下载（内置主题照常工作）"; return 0; }
	base="https://github.com/$THEME_REPO/releases/latest/download"
	[ -z "$THEME_TAG" ] || base="https://github.com/$THEME_REPO/releases/download/$THEME_TAG"
	tmp="$(mktemp -d)"
	curl -fsSL --max-time 300 "$base/theme.tar.gz" -o "$tmp/theme.tar.gz" || {
		warn "主题下载失败，保留内置主题：$base/theme.tar.gz"
		rm -rf "$tmp"
		return 0
	}
	curl -fsSL --max-time 30 "$base/theme.tar.gz.sha256" -o "$tmp/theme.tar.gz.sha256" || {
		warn "主题校验文件下载失败，保留内置主题"
		rm -rf "$tmp"
		return 0
	}
	want="$(sed -n 's/^\([0-9a-f]\{64\}\).*/\1/p' "$tmp/theme.tar.gz.sha256" | head -n 1)"
	got="$(sha256sum "$tmp/theme.tar.gz" | cut -d' ' -f1)"
	if [ -z "$want" ] || [ "$got" != "$want" ]; then
		warn "主题校验不通过，保留内置主题（期望 ${want:-空}，实得 $got）"
		rm -rf "$tmp"
		return 0
	fi
	mkdir -p "$tmp/unpacked"
	if ! tar xzf "$tmp/theme.tar.gz" -C "$tmp/unpacked"; then
		warn "主题解包失败，保留内置主题"
		rm -rf "$tmp"
		return 0
	fi
	if [ ! -f "$tmp/unpacked/dist/index.html" ] || [ ! -f "$tmp/unpacked/theme.json" ]; then
		warn "主题包里没有 dist/index.html 和 theme.json，保留内置主题"
		rm -rf "$tmp"
		return 0
	fi
	# Staged inside the themes directory, so the move into place is a rename on
	# one filesystem rather than a copy that could be interrupted half-written.
	install -d -m 0755 -o "$USER_NAME" "$DATA/themes"
	stage="$DATA/themes/.stage-default"
	rm -rf "$stage"
	mv "$tmp/unpacked" "$stage"
	rm -rf "$tmp"
	# Replaced wholesale: a directory left over from an earlier install must not
	# contribute a stale asset to the new one.
	rm -rf "$DATA/themes/default"
	mv "$stage" "$DATA/themes/default"
	chown -R "$USER_NAME" "$DATA/themes/default"
	version="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$DATA/themes/default/theme.json" | head -n 1)"
	ok "主题" "${THEME_TAG:-latest}${version:+ · $version}"
}

# The new emergency password, straight from the binary's stdout.
#
# Not from the journal: some hosts keep none, and on a first install the password
# has to be in place before the service starts anyway, or the hub generates a
# second one. Empty output means the binary predates `--reset-password`, which the
# caller handles by reading the journal of that older release's start instead.
new_password() {
	"$BIN" --db "$DATA/monitor.db" --reset-password 2>/dev/null |
		sed -n 's/^Emergency password: //p' | tail -n 1
}

install_hub() {
	case "$(uname -m)" in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) arch=aarch64 ;;
	*) die "不支持的架构：$(uname -m)（发布的是 x86_64 与 aarch64）" ;;
	esac
	asset="monitor-hub-$arch-unknown-linux-musl"
	base="https://github.com/$REPO/releases/latest/download"
	ok "架构" "$arch"

	# Whatever the command line did not specify is recovered from the old unit;
	# see old_exec.
	if [ -z "$PORT_SET" ]; then
		carried="$(old_port)"
		[ -z "$carried" ] || PORT="$carried"
	fi
	if [ -z "$SITE_SET" ]; then
		carried="$(old_exec)"
		case "$carried" in
		*--site\ *) SITE="${carried##*--site }"; SITE="${SITE%% *}" ;;
		esac
	fi
	check_port "$PORT"

	# Before anything is stopped, replaced or downloaded: a port conflict must
	# leave the running hub untouched. The hub's own socket is never a conflict,
	# determined by pid rather than by the port written in the unit -- one this
	# script did not write, whether hand-edited or with ExecStart split across
	# lines, parses out empty, and the hub already on the port would be reported
	# as an unrelated process occupying it.
	if command -v ss >/dev/null 2>&1; then
		holder="$(ss -ltnpH "sport = :$PORT" 2>/dev/null |
			sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1)"
		if [ -n "$holder" ] &&
			[ "$holder" != "$(systemctl show -p MainPID --value "$SERVICE" 2>/dev/null)" ]; then
			die "端口 $PORT 已被其它程序占用，换一个：--port <n>"
		fi
	fi

	id -u "$USER_NAME" >/dev/null 2>&1 ||
		useradd --system --no-create-home --shell /usr/sbin/nologin "$USER_NAME" ||
		die "无法创建系统用户 $USER_NAME"
	install -d -m 0755 "$ROOT"
	# 0700 owned by the service user, so its group is irrelevant and nothing here
	# depends on useradd having created one.
	install -d -m 0700 -o "$USER_NAME" "$DATA"

	# A first run prints the one-time password, and only a missing database
	# constitutes one. Checked before anything is installed.
	first=""
	[ -f "$DATA/monitor.db" ] || first=1

	# The tag comes from GitHub's own redirect for "latest", so there is no API
	# call to be rate-limited and no JSON to parse. Only the first hop carries it
	# -- the chain ends on release-assets.githubusercontent.com, whose URL
	# contains no tag -- so this must not follow redirects. A missing asset still
	# redirects, so the download below is what catches an architecture that was
	# never published.
	tag="$(curl -fsSI -o /dev/null -w '%{redirect_url}' "$base/$asset" 2>/dev/null |
		sed -n 's#.*/download/\([^/]*\)/.*#\1#p')" || true
	[ -n "$tag" ] || die "查不到最新发布版；GitHub 不可达，或还没有任何发布"
	ok "版本" "$tag"

	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT
	curl -fsSL --max-time 300 "$base/$asset" -o "$tmp/$asset" || die "二进制下载失败：$base/$asset"
	ok "下载" "$(du -h "$tmp/$asset" | cut -f1)"

	# Verified against the release's own checksum file, so a truncated transfer or
	# a substituted asset is caught before anything lands in /opt/monitor.
	curl -fsSL --max-time 30 "$base/sha256sums.txt" -o "$tmp/sums" ||
		die "校验文件下载失败"
	want="$(sed -n "s/^\([0-9a-f]\{64\}\)  *$asset\$/\1/p" "$tmp/sums")"
	[ -n "$want" ] || die "sha256sums.txt 里没有 $asset 这一项"
	got="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
	[ "$got" = "$want" ] || die "校验不通过，已丢弃下载的文件。期望 ${want}，实得 $got"
	ok "校验" "sha256 一致"

	# Keep the old binary until the new one has proved it starts: a failed upgrade
	# must leave a running hub rather than a dead service.
	backup=""
	if [ -f "$BIN" ]; then
		backup="$BIN.old"
		# Never over an existing backup. A run that died between the install below
		# and the health check left $BIN holding a binary that never proved it
		# starts; copying that over the good backup would make the rollback restore
		# the same broken binary while reporting success.
		[ -f "$backup" ] || cp -f "$BIN" "$backup"
	fi
	# Stopped first so the copy does not land beneath a live process.
	systemctl stop "$SERVICE" 2>/dev/null || true
	install -m 0755 "$tmp/$asset" "$BIN"
	# The download is complete. Cleared here as well as on EXIT because the menu
	# calls this more than once per run and each call replaces the trap, leaving
	# the previous directory and its binary uncollected.
	rm -rf "$tmp"
	trap - EXIT

	# Only when there is nothing there yet: an upgrade must not silently replace
	# a theme the operator has since updated or replaced from the panel. The
	# panel's own update check is what tells them a newer one exists.
	if [ -n "$THEME_SKIP" ]; then
		ok "主题" "按 --no-theme 跳过，用二进制内置的那一份"
	elif [ -d "$DATA/themes/default" ]; then
		ok "主题" "沿用已安装的 $(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$DATA/themes/default/theme.json" 2>/dev/null | head -n 1)"
	else
		fetch_theme
	fi

	# Loopback only: the panel and the agent tokens never traverse a network in
	# the clear, and there is no port to firewall. Reaching it is the reverse
	# proxy's responsibility, and 127.0.0.1 rather than [::1] because that is
	# every proxy's default upstream; the hub binds one address, not both.
	args="--listen 127.0.0.1:$PORT --db $DATA/monitor.db"
	[ -z "$SITE" ] || args="$args --site $SITE"
	cat >"$UNIT" <<UNIT
[Unit]
Description=monitor hub
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN $args
Restart=always
RestartSec=5
# The database and the themes/ directory beside it live in data/, which is the
# only path this service may write to, excluding even the binary above it.
User=$USER_NAME
WorkingDirectory=$ROOT
ReadWritePaths=$DATA
NoNewPrivileges=yes
RestrictSUIDSGID=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=256M

[Install]
WantedBy=multi-user.target
UNIT

	# A reset refuses a database that does not exist, so a first install creates
	# the file first, under the service user: SQLite reads an empty file as a new
	# database and the hub then finds a password already in place. Quiet, because
	# a release predating the flag is handled after the start below.
	pw=""
	if [ -n "$first" ]; then
		install -m 0600 -o "$USER_NAME" /dev/null "$DATA/monitor.db"
		pw="$(new_password)"
	fi

	systemctl daemon-reload
	systemctl enable "$SERVICE" >/dev/null 2>&1 || true
	# Not left to set -e: a binary that cannot exec fails the job itself, which is
	# precisely the case the rollback below exists for. Unguarded, the script
	# would exit here with a raw systemd error and leave the hub down on the
	# binary that just failed.
	started="$(date '+%Y-%m-%d %H:%M:%S')"
	systemctl restart "$SERVICE" || true
	# is-active answers before a unit that exits immediately has done so, and the
	# first run also computes an argon2 hash. Wait, then query.
	sleep 3
	if ! systemctl is-active --quiet "$SERVICE"; then
		# A first install has nothing to keep serving, and the database it just
		# created holds a password that was never shown: leaving it would make the
		# next run an upgrade, which prints none. Removed and disabled, so a rerun
		# is a first install again.
		if [ -n "$first" ] && [ -z "$backup" ]; then
			systemctl disable --now "$SERVICE" >/dev/null 2>&1 || true
			rm -f "$DATA/monitor.db" "$DATA/monitor.db-wal" "$DATA/monitor.db-shm"
			die "服务启动失败。日志：journalctl -u $SERVICE -n 50"
		fi
		if [ -n "$backup" ]; then
			install -m 0755 "$backup" "$BIN"
			rm -f "$backup"
			systemctl restart "$SERVICE" 2>/dev/null || true
			die "新版本没能启动，已回滚到上一版。日志：journalctl -u $SERVICE -n 50"
		fi
		die "服务启动失败。日志：journalctl -u $SERVICE -n 50"
	fi
	rm -f "$BIN.old"
	ok "服务" "已启动并开机自启"

	# A release predating `--reset-password` refuses it and sets the password on
	# its first start instead, printing it to the journal. Only this run's lines:
	# an earlier failed attempt printed a password for a database since removed.
	if [ -n "$first" ] && [ -z "$pw" ]; then
		pw="$(journalctl -u "$SERVICE" --since "$started" --no-pager 2>/dev/null |
			sed -n 's/.*Emergency password: //p' | tail -n 1)"
	fi

	if [ -n "$first" ]; then done_title="安装完成"; else done_title="升级完成"; fi
	printf '\n  %s%s%s\n' "$B" "$done_title" "$N"
	rule
	printf '\n'
	field "面板" "${SITE:-http://127.0.0.1:$PORT}/admin"
	if [ -n "$first" ]; then
		if [ -n "$pw" ]; then
			field "密码" "$pw"
			field "    " "${D}只显示这一次，登录后到「安全」页的应急密码里改掉${N}"
		else
			field "密码" "没取到，重跑安装器选「重置密码」"
		fi
	fi
	field "数据" "$DATA/monitor.db"
	field "服务" "systemctl status $SERVICE"
	field "日志" "journalctl -u $SERVICE -f"
	printf '\n'

	# The hub is on loopback, so this is the remaining half of the install rather
	# than optional advice. It deliberately omits --site: the panel builds install
	# commands from the browser's own address, so once the domain works everything
	# downstream follows.
	if [ -z "$SITE" ]; then
		printf '  %s还差一步：配个反向代理%s\n' "$B" "$N"
		printf '     面板只监听本机，公网访问不到——这是故意的，凭证不会在链路上裸奔。\n'
		printf '     用 nginx / caddy / cf tunnel 任选一种，把 hub.example.com 换成你的域名，\n'
		printf '     配好之后用域名访问面板，我相信这难不倒你。\n\n'
		proxy_configs
		printf '\n     %s四点注意与完整说明见 README 的「反向代理」一节。%s\n' "$D" "$N"
	fi
}

# The three configurations, printed where they are needed rather than described:
# the hub is unreachable until one of them is in place, so an installer that
# stops at "put a proxy in front of it" leaves the install half done.
#
# The heredocs are unquoted so $PORT lands in them; every variable belonging to
# the proxy is escaped, since nginx and caddy read those themselves.
proxy_configs() {
	printf '  %scaddy%s  Caddyfile\n' "$B" "$N"
	cat <<CADDY
hub.example.com {
    reverse_proxy 127.0.0.1:$PORT
}
CADDY
	printf '\n  %snginx%s  /etc/nginx/sites-available/hub.example.com\n' "$B" "$N"
	cat <<NGINX
map \$http_upgrade \$connection_upgrade { default upgrade; '' close; }

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name hub.example.com;
    ssl_certificate     /etc/letsencrypt/live/hub.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hub.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:$PORT;
        proxy_http_version 1.1;
        # 导入备份与上传主题是分片传的，单片 4 MiB；这个数不随数据库增长。
        client_max_body_size 8m;
        proxy_set_header Host              \$host;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_set_header X-Forwarded-For   \$proxy_add_x_forwarded_for;
        # /api/agent/ws 与 /api/ws 是长连接。
        proxy_set_header Upgrade    \$http_upgrade;
        proxy_set_header Connection \$connection_upgrade;
        proxy_buffering off;
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
    }
}
NGINX
	printf '\n  %scloudflare 隧道%s  config.yml（不用开任何入站端口）\n' "$B" "$N"
	cat <<CFD
ingress:
  - hostname: hub.example.com
    service: http://127.0.0.1:$PORT
  - service: http_status:404
CFD
}

# ---- uninstall ----
uninstall_hub() {
	if [ ! -f "$BIN" ] && [ ! -f "$UNIT" ]; then
		# The data outlives the unit, so --purge still has work to do.
		if [ -n "$PURGE" ] && [ -e "$DATA" ]; then
			confirm "服务已经卸载了。删除 $DATA 下的数据库？不可撤销" || return 0
			rm -rf "$DATA"
			rmdir "$ROOT" 2>/dev/null || true
			ok "数据" "已删除"
			return 0
		fi
		[ ! -e "$DATA" ] || die "服务已经卸载了，数据还留在 ${DATA}；要一并删掉就加 --purge"
		die "这台机器上没有装 monitor hub"
	fi
	if [ -n "$PURGE" ]; then
		confirm "卸载 monitor hub，并删除 $DATA 下的数据库？不可撤销" || return 0
	else
		confirm "卸载 monitor hub？数据保留在 $DATA" || return 0
	fi
	systemctl disable --now "$SERVICE" 2>/dev/null || true
	rm -f "$UNIT" "$BIN" "$BIN.old"
	systemctl daemon-reload
	ok "服务" "已移除"
	if [ -n "$PURGE" ]; then
		rm -rf "$DATA"
		# Only when the agent is not installed alongside it.
		rmdir "$ROOT" 2>/dev/null || true
		ok "数据" "已删除"
	else
		field "数据" "保留在 ${DATA}，重新安装会直接接着用"
	fi
}

menu() {
	while :; do
		banner
		printf '    1  安装 / 升级\n'
		printf '    2  卸载\n'
		printf '    3  状态\n'
		printf '    4  日志\n'
		printf '    5  重置密码\n'
		printf '    q  退出\n\n'
		printf '  %s›%s ' "$B" "$N"
		read -r choice || exit 0
		printf '\n'
		case "$choice" in
		1)
			# The default offered is what the unit already listens on, so Enter
			# leaves a running deployment unchanged. PORT_SET marks the answer as
			# supplied: without it the carry-over in install_hub would read the port
			# back out of that same unit and discard the answer given here.
			carried="$(old_port)"
			PORT="$(ask "监听端口" "${carried:-$PORT}")"
			check_port "$PORT"
			PORT_SET=1
			printf '\n'
			install_hub
			press
			;;
		2) uninstall_hub; press ;;
		3) systemctl status "$SERVICE" --no-pager || true; press ;;
		4) journalctl -u "$SERVICE" -f --no-pager ;;
		5) reset_password; press ;;
		q | Q | exit | "") exit 0 ;;
		*) ;;
		esac
	done
}

usage() {
	cat <<TXT
monitor hub 安装器

  sudo ./install-hub.sh                有终端时给菜单，否则按默认安装
  sudo ./install-hub.sh --port 8443    指定端口安装
  sudo ./install-hub.sh --uninstall    卸载，保留数据
  sudo ./install-hub.sh --reset-password  重新生成应急密码并登出所有会话
  sudo ./install-hub.sh --purge        卸载并删除数据库

  --port <n>     本机监听端口，默认 $PORT
  --site <url>   一般不用填。面板拼安装命令用的是浏览器地址栏，配好反代
                 用域名访问就自动对了。只有两种情况要填：你进面板的地址
                 不是节点能用的地址（比如走 SSH 隧道），或反代不发
                 X-Forwarded-Proto
  --theme-tag <t>  装指定版本的主题，而不是该主题仓库的最新发布版
  --no-theme     不下载主题，公开页用二进制内置的那一份（离线装机用）
  --yes, -y      跳过确认
  --help, -h     显示这段

hub 只监听 127.0.0.1，公网访问不到，需要自己配 nginx / caddy / CF 隧道把
域名指过来。装完会打印具体怎么配。

重跑一次就是升级：校验通过后才替换二进制，起不来会自动回滚到上一版；
没写的参数沿用上次的，所以升级不会把端口和 --site 冲掉。

首次安装会顺带把主题仓库当前发布版的主题装到 ${DATA}/themes/default/（校验 sha256），
这样主题可以独立发版；升级不会动你已经装的主题。
二进制和数据都在 $ROOT 下（数据库和主题在 ${DATA}），卸载默认保留数据。
TXT
}

while [ $# -gt 0 ]; do
	case "$1" in
	# An explicit guard rather than `${2-}`: `shift 2` with nothing to shift is
	# fatal in dash, and the output would be the shell's diagnostic rather than
	# this message.
	--port) [ $# -ge 2 ] || die "--port 后面要跟端口号"; PORT="$2"; PORT_SET=1; shift 2 ;;
	--site) [ $# -ge 2 ] || die "--site 后面要跟地址"; SITE="$2"; SITE_SET=1; shift 2 ;;
	--uninstall) ACTION=uninstall; shift ;;
	--reset-password) ACTION=reset-password; shift ;;
	--purge) ACTION=uninstall; PURGE=1; shift ;;
	--theme-tag) [ $# -ge 2 ] || die "--theme-tag 后面要跟标签，例如 v1.7.0"; THEME_TAG="$2"; shift 2 ;;
	--no-theme) THEME_SKIP=1; shift ;;
	--yes | -y) YES=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) die "未知参数：$1（--help 看用法）" ;;
	esac
done

check_port "$PORT"
# The same form `api::https_domain` measures --site against on the hub, checked
# here because this is where the value is entered. A hub started with a value it
# refuses starts normally and then declines to add or install any node, while the
# message the panel prints names the browser and the reverse proxy, neither of
# which is at fault.
SITE="${SITE%/}"
if [ -n "$SITE" ]; then
	case "$SITE" in
	https://*) ;;
	*) die "--site 必须以 https:// 开头：$SITE" ;;
	esac
	rest="${SITE#https://}"
	case "$rest" in
	*/*) die "--site 后面不能带路径，只要 https://域名[:端口]：$SITE" ;;
	*@*) die "--site 里不能带用户名：$SITE" ;;
	"["*) die "--site 必须是域名，不能是 IP 地址：$SITE" ;;
	esac
	# A port is permitted; what precedes it must be a name rather than an
	# address.
	case "${rest%%:*}" in
	"" | localhost | *.localhost) die "--site 必须是一个域名：$SITE" ;;
	*[!0-9.]*) ;;
	*) die "--site 必须是域名而不是 IP 地址：$SITE" ;;
	esac
fi
[ "$(id -u)" = 0 ] || die "需要 root：sudo sh $0"
command -v curl >/dev/null 2>&1 || die "需要 curl"
command -v sha256sum >/dev/null 2>&1 || die "需要 sha256sum（装 coreutils）"
command -v systemctl >/dev/null 2>&1 ||
	die "这个安装器只装 systemd 服务。手动运行：$BIN --listen 127.0.0.1:$PORT --db $DATA/monitor.db"

# Resets the emergency password and reports it. Its own function because the menu
# and `--reset-password` both reach it, and because the failure to explain (no
# database yet, or a binary too old to know the flag) is the same one.
reset_password() {
	if [ ! -f "$DATA/monitor.db" ]; then
		warn "还没有数据库：先安装一次"
		return 0
	fi
	pw="$(new_password)"
	printf '\n'
	if [ -n "$pw" ]; then
		field "新密码" "$pw"
		field "    " "${D}已登出所有会话，登录后到「安全」页里改掉${N}"
	else
		warn "重置失败：二进制不支持 --reset-password，或数据库不可写"
	fi
	printf '\n'
}

case "$ACTION" in
uninstall) banner; uninstall_hub ;;
reset-password) banner; reset_password ;;
*)
	if [ -t 0 ]; then
		menu
	else
		banner
		install_hub
	fi
	;;
esac
