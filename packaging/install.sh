#!/bin/bash
# Collaplex RT —— deb 安装后的用户级启停脚本
# 把配置渲染到 ~/.config/pipewire/pipewire.conf.d 并切默认输出。
set -eu

ROOT=/usr/share/collaplex-rt
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/pipewire/pipewire.conf.d"
STATE_DIR="$HOME/.local/state/collaplex-rt"
DSP=/usr/bin/collaplex-rt-dsp
GAP=/usr/bin/collaplex-rt-gap
GAP_R=/usr/bin/collaplex-rt-gap-r
BAL_L_DB="${BAL_L_DB:-3.01}"
BAL_R_DB="${BAL_R_DB:--3.01}"
MONO="${MONO:-auto}"
MONO_IN="collaplex_mono_in"

# 链路激活 / 待命共用函数
. "$ROOT/scripts/chain-lib.sh"

sink_channels() {
    /usr/bin/python3 - "$1" <<'PY'
import json, subprocess, sys
name = sys.argv[1]
try:
    d = json.loads(subprocess.check_output(["pw-dump"], stderr=subprocess.DEVNULL))
except Exception:
    print(""); raise SystemExit
for n in d:
    if n.get("type") != "PipeWire:Interface:Node":
        continue
    p = n.get("info", {}).get("props", {})
    if p.get("node.name") == name:
        print(p.get("audio.channels", ""))
        break
PY
}

linked() {
    /usr/bin/python3 - "$1" "$2" <<'PY'
import json, subprocess, sys
out_name, tgt_name = sys.argv[1], sys.argv[2]
try:
    d = json.loads(subprocess.check_output(["pw-dump"], stderr=subprocess.DEVNULL))
except Exception:
    print(0); raise SystemExit
ids = {}
links = []
for n in d:
    t = n.get("type")
    p = n.get("info", {}).get("props", {})
    if t == "PipeWire:Interface:Node":
        ids[p.get("node.name", "")] = n.get("id")
    elif t == "PipeWire:Interface:Link":
        links.append(p)
oid = ids.get(out_name)
tid = ids.get(tgt_name)
print(1 if oid and tid and any(l.get("link.output.node") == oid and l.get("link.input.node") == tid for l in links) else 0)
PY
}

rollback() {
    rm -f "$CONF_DIR/10-rt-hrtf.conf" "$CONF_DIR/20-rt-dsp.conf" "$CONF_DIR/30-rt-mono.conf" "$CONF_DIR/91-rt-clock.conf"
    for p in /proc/[0-9]*; do
        [ -r "$p/cmdline" ] || continue
        c=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null) || continue
        case "$c" in *collaplex-realtime*) kill -9 "${p#/proc/}" 2>/dev/null || true;; esac
    done
    rm -f /dev/shm/collaplex-rt-agc /dev/shm/collaplex-rt-agc.lock* /dev/shm/collaplex-rt-gap /dev/shm/collaplex-rt-ctl /dev/shm/collaplex-rt-scope
    systemctl --user restart pipewire pipewire-pulse wireplumber
    sleep 2
}

[ -x /usr/bin/collaplex-realtime ] || { echo "缺少 /usr/bin/collaplex-realtime" >&2; exit 1; }
mkdir -p "$CONF_DIR" "$STATE_DIR"

pick_target() {
    if [ -n "${TARGET:-}" ]; then printf '%s' "$TARGET"; return; fi
    if [ -s "$STATE_DIR/target" ]; then cat "$STATE_DIR/target"; return; fi
    /usr/bin/python3 - <<'PY'
import json, subprocess
try:
    data = json.loads(subprocess.check_output(["pw-dump"], stderr=subprocess.DEVNULL))
except Exception:
    print(""); raise SystemExit
sinks = []
for n in data:
    if n.get("type") != "PipeWire:Interface:Node":
        continue
    p = n.get("info", {}).get("props", {})
    if p.get("media.class") != "Audio/Sink":
        continue
    name = p.get("node.name", "")
    if "collaplex" in name:
        continue
    sinks.append(name)
pref = [s for s in sinks if "iec958" in s or "usb" in s.lower()]
print((pref or sinks or [""])[0])
PY
}

TARGET_NAME="$(pick_target)"
if [ -z "$TARGET_NAME" ]; then
    echo "找不到硬件输出 sink；软件照常启动，链路待命（接上设备后自动激活）" >&2
    mkdir -p "$STATE_DIR"
    printf 'waiting\n' > "$STATE_DIR/chain-state"
    exit 0
fi
echo "输出设备: $TARGET_NAME"
CH=$(sink_channels "$TARGET_NAME")
if [ -n "$CH" ]; then
    TARGET_PRESENT=1
else
    TARGET_PRESENT=0
fi
printf '%s' "$TARGET_NAME" > "$STATE_DIR/target"
if [ "$MONO" = "1" ]; then MODE=mono
elif [ "$MONO" = "0" ]; then MODE=stereo
elif [ "$CH" = "1" ]; then MODE=mono
else MODE=stereo
fi
EQ_TARGET="$TARGET_NAME"
[ "$MODE" = "mono" ] && EQ_TARGET="$MONO_IN"
if [ "$MODE" = "mono" ]; then
    echo "  输出模式: 单声道混合（模拟设备）"
else
    echo "  输出模式: 立体声直连"
fi
if [ "$TARGET_PRESENT" = 1 ]; then
    echo "  目标设备在位"
else
    echo "  目标设备不在位：链路待命，接上后自动激活"
fi
BAL_L_MULT=$(LC_ALL=C awk "BEGIN{printf \"%.6f\", 10^($BAL_L_DB/20)}")
BAL_R_MULT=$(LC_ALL=C awk "BEGIN{printf \"%.6f\", 10^($BAL_R_DB/20)}")

# 清掉旧版 collaplex 链的配置
rm -f "$CONF_DIR/10-collaplex-vsink.conf" "$CONF_DIR/20-collaplex-hrtf.conf" \
      "$CONF_DIR/30-collaplex-eq.conf" "$CONF_DIR/40-collaplex-mono.conf" "$CONF_DIR/30-rt-mono.conf" \
      "$CONF_DIR/91-clock.conf"

sed -e "s|__HRIR__|$ROOT/assets/hrir|g" -e "s|__REVERB__|$ROOT/assets/reverb/cinema-ir-96k.wav|g" \
    -e "s|__DSP_IN__|collaplex_dsp_in|g" -e "s|__GAP_L__|$GAP|g" -e "s|__GAP_R__|$GAP_R|g" \
    "$ROOT/pipewire/10-rt-hrtf.conf" > "$CONF_DIR/10-rt-hrtf.conf"
sed -e "s|__DSP__|$DSP|g" -e "s|__TARGET__|$EQ_TARGET|g" \
    -e "s|__BAL_L_MULT__|$BAL_L_MULT|g" -e "s|__BAL_R_MULT__|$BAL_R_MULT|g" \
    "$ROOT/pipewire/20-rt-dsp.conf" > "$CONF_DIR/20-rt-dsp.conf"
if [ "$MODE" = "mono" ]; then
    sed -e "s|__TARGET__|$TARGET_NAME|g" \
        "$ROOT/pipewire/30-rt-mono.conf" > "$CONF_DIR/30-rt-mono.conf"
else
    rm -f "$CONF_DIR/30-rt-mono.conf"
fi
cp "$ROOT/pipewire/91-rt-clock.conf" "$CONF_DIR/91-rt-clock.conf"
echo "已装配置: 10-rt-hrtf.conf / 20-rt-dsp.conf / 91-rt-clock.conf"

# 清残留 DSP
for p in /proc/[0-9]*; do
    [ -r "$p/cmdline" ] || continue
    c=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null) || continue
    case "$c" in
        *collaplex-realtime*) kill -9 "${p#/proc/}" 2>/dev/null || true;;
    esac
done
rm -f /dev/shm/collaplex-rt-agc /dev/shm/collaplex-rt-agc.lock* \
      /dev/shm/collaplex-rt-gap /dev/shm/collaplex-rt-ctl /dev/shm/collaplex-rt-scope

systemctl --user restart pipewire pipewire-pulse wireplumber
sleep 4
# 重启后链路需要重新判定一次；否则“active”状态会让监视进程跳过切默认输出
printf 'waiting\n' > "$STATE_DIR/chain-state"

# ---- 立即判定一次链路状态，再启动设备监视 ----
# 目标设备不在位时链路只是“待命”：配置和 DSP 都保留，默认输出不切到虚拟声卡；
# 设备接上后由 target-watch 按 模拟 → 数字 → 虚拟声卡 激活。
bash "$ROOT/scripts/target-watch.sh" --once || true

PIDFILE="$STATE_DIR/watch.pid"
OLD_PID=""
[ -f "$PIDFILE" ] && OLD_PID=$(cat "$PIDFILE" 2>/dev/null || true)
case "$OLD_PID" in ''|*[!0-9]*) OLD_PID=0;; esac
if [ "$OLD_PID" -gt 0 ] && kill -0 "$OLD_PID" 2>/dev/null; then
    echo "设备监视: 已在运行 (pid $OLD_PID)"
else
    setsid nohup bash "$ROOT/scripts/target-watch.sh" >/dev/null 2>&1 < /dev/null &
    echo "设备监视: 已启动"
fi

echo "=== 自检 ==="
echo -n "节点: "; pw-dump 2>/dev/null | grep -o '"node.name": "collaplex[^"]*"' | sort -u | tr '\n' ' '; echo
n=0
for p in /proc/[0-9]*; do
    [ -r "$p/cmdline" ] || continue
    c=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null) || continue
    case "$c" in *collaplex-realtime*) n=$((n+1));; esac
done
echo "DSP/间隙进程数: $n (应为 4：2 DSP + 2 干声响度锚点)"
echo -n "链路状态: "; cat "$STATE_DIR/chain-state" 2>/dev/null || echo "(未知)"
echo -n "默认输出: "; wpctl inspect @DEFAULT_SINK@ 2>/dev/null | sed -n 's/.*node.name = "\(.*\)"/\1/p' | head -1
[ "$TARGET_PRESENT" = 1 ] || echo "提示: 目标设备未接，链路处于待命；接上后自动激活"
