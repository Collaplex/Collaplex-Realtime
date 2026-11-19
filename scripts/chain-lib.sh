#!/bin/bash
# chain-lib.sh —— 链路激活 / 待命的共用函数
# 由 scripts/install.sh 与 scripts/target-watch.sh source。
#
# 状态文件：
#   ~/.local/state/collaplex-rt/chain-state   active | waiting
#   ~/.local/state/collaplex-rt/prev-default  激活前用户正在用的默认输出
#
# 规则：
#   目标设备在位 → active：按“模拟 → 数字（目标）→ 虚拟声卡”顺序切默认输出；
#   目标设备不在位 → waiting：默认输出留在/退回硬件，链路不接管声音。

STATE_DIR="${STATE_DIR:-$HOME/.local/state/collaplex-rt}"
CONF_DIR="${CONF_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/pipewire/pipewire.conf.d}"
CHAIN_STATE_FILE="$STATE_DIR/chain-state"
PREV_DEFAULT_FILE="$STATE_DIR/prev-default"
WATCH_LOG="$STATE_DIR/watch.log"
CYCLE_HOLD="${CYCLE_HOLD:-0.5}"     # 每“点”一次声卡后的停留时间（秒）
RESTART_GAP="${RESTART_GAP:-60}"    # 两次重启 PipeWire 的最小间隔（秒）
VIRTUAL_SINK="collaplex_rt_sink"

chain_log() {
    printf '%s %s\n' "$(date '+%F %T')" "$*" >>"$WATCH_LOG" 2>/dev/null || true
    sz=$(stat -c %s "$WATCH_LOG" 2>/dev/null || echo 0)
    if [ "${sz:-0}" -gt 262144 ]; then
        tail -n 200 "$WATCH_LOG" >"$WATCH_LOG.tmp" 2>/dev/null &&
            mv "$WATCH_LOG.tmp" "$WATCH_LOG" 2>/dev/null || true
    fi
}

# 当前记录的目标设备名（每次现读，支持运行中换目标）
target_name() {
    tr -d '[:space:]' <"$STATE_DIR/target" 2>/dev/null || true
}

# 所有 sink：name<TAB>id<TAB>analog<TAB>description
sinks_tsv() {
    /usr/bin/python3 - <<'PY' 2>/dev/null || true
import json, subprocess
try:
    data = json.loads(subprocess.check_output(["pw-dump"], stderr=subprocess.DEVNULL))
except Exception:
    raise SystemExit
for n in data:
    if n.get("type") != "PipeWire:Interface:Node":
        continue
    p = n.get("info", {}).get("props", {})
    if p.get("media.class") != "Audio/Sink":
        continue
    name = p.get("node.name", "") or ""
    desc = p.get("node.description", "") or p.get("node.nick", "") or name
    analog = 1 if ("analog" in name.lower() or "analog" in desc.lower() or "模拟" in desc) else 0
    print("%s\t%s\t%d\t%s" % (name, n.get("id", ""), analog, desc))
PY
}

sink_id() {
    sinks_tsv | awk -F'\t' -v n="$1" '$1 == n { print $2; exit }'
}

sink_present() {
    [ -n "$1" ] && [ -n "$(sink_id "$1")" ]
}

default_sink_name() {
    wpctl inspect @DEFAULT_SINK@ 2>/dev/null |
        sed -n 's/.*node.name = "\(.*\)"/\1/p' | head -1
}

# DSP 输出节点名（mono 模式下是 collaplex_mono_out）
out_node_name() {
    if [ -f "$CONF_DIR/30-rt-mono.conf" ]; then
        echo collaplex_mono_out
    else
        echo collaplex_dsp_out
    fi
}

# chain_linked <输出节点> <目标节点> —— 返回 0 表示已连接
chain_linked() {
    /usr/bin/python3 - "$1" "$2" <<'PY' 2>/dev/null
import json, subprocess, sys
out_name, tgt_name = sys.argv[1], sys.argv[2]
try:
    d = json.loads(subprocess.check_output(["pw-dump"], stderr=subprocess.DEVNULL))
except Exception:
    raise SystemExit(1)
ids, links = {}, []
for n in d:
    t = n.get("type")
    p = n.get("info", {}).get("props", {})
    if t == "PipeWire:Interface:Node":
        ids[p.get("node.name", "")] = n.get("id")
    elif t == "PipeWire:Interface:Link":
        links.append(p)
oid, tid = ids.get(out_name), ids.get(tgt_name)
ok = oid and tid and any(l.get("link.output.node") == oid and l.get("link.input.node") == tid for l in links)
raise SystemExit(0 if ok else 1)
PY
}

set_default_by_name() {
    local id
    id=$(sink_id "$1")
    [ -n "$id" ] || return 1
    wpctl set-default "$id" >/dev/null 2>&1
}

# 按“模拟 → 数字（目标）→ 虚拟声卡”点一遍默认输出。
# 开机后直接落到虚拟声卡时音量会很小；逐个设备切一遍让 WirePlumber
# 把各自的音量/路由恢复出来，最后停在虚拟声卡。
cycle_sinks() {
    local target="$1" n id
    local seq=()

    while IFS= read -r n; do
        [ -n "$n" ] || continue
        [ "$n" = "$target" ] && continue
        seq+=("$n")
    done < <(sinks_tsv | awk -F'\t' '$3 == 1 && $1 !~ /^collaplex/ { print $1 }')

    seq+=("$target")
    [ "$target" != "$VIRTUAL_SINK" ] && seq+=("$VIRTUAL_SINK")

    for n in "${seq[@]}"; do
        id=$(sink_id "$n")
        if [ -z "$id" ]; then
            chain_log "  跳过 $n（节点不存在）"
            continue
        fi
        wpctl set-default "$id" >/dev/null 2>&1 || true
        chain_log "  默认输出 → $n"
        sleep "$CYCLE_HOLD"
    done

    # 只有最后真的停在虚拟声卡上才算切成功
    [ "$(default_sink_name)" = "$VIRTUAL_SINK" ]
}

# 重启 PipeWire 让播放节点重新解析 node.target；60s 内最多一次。
maybe_restart_pipewire() {
    local now last
    now=$(date +%s)
    last=$(cat "$STATE_DIR/last-restart" 2>/dev/null || echo 0)
    case "$last" in ''|*[!0-9]*) last=0;; esac
    if [ $((now - last)) -lt "$RESTART_GAP" ]; then
        chain_log "跳过重启 PipeWire（${RESTART_GAP}s 内已重启过）"
        return 1
    fi
    chain_log "链路未连接：重启 PipeWire 重新解析 node.target"
    date +%s >"$STATE_DIR/last-restart"
    # filter-chain 的 DSP 子进程会拖住 pipewire.service 的停止，先清掉
    for p in /proc/[0-9]*; do
        [ -r "$p/cmdline" ] || continue
        c=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null) || continue
        case "$c" in
            *collaplex-realtime*)
                kill -9 "${p#/proc/}" 2>/dev/null || true;;
        esac
    done
    rm -f /dev/shm/collaplex-rt-agc /dev/shm/collaplex-rt-agc.lock* /dev/shm/collaplex-rt-gap
    timeout 25 systemctl --user restart pipewire pipewire-pulse wireplumber >/dev/null 2>&1 || true
    sleep 4
    return 0
}

# 目标设备在位：确保链路连通，再按顺序切默认输出。
activate_chain() {
    local target out prev
    target=$(target_name)
    if [ -z "$target" ]; then
        chain_log "没有目标设备名，无法激活"
        return 1
    fi
    if ! sink_present "$target"; then
        chain_log "目标设备不在位：$target"
        return 1
    fi

    out=$(out_node_name)
    prev=$(default_sink_name)
    case "$prev" in
        ''|collaplex*|"$target") ;;
        *) printf '%s' "$prev" >"$PREV_DEFAULT_FILE" ;;
    esac

    chain_log "目标设备在位：$target"
    if ! chain_linked "$out" "$target"; then
        maybe_restart_pipewire || true
        sleep 1
    fi
    if ! chain_linked "$out" "$target"; then
        chain_log "警告：$out 仍未连到 $target，稍后重试"
        return 1
    fi

    if ! cycle_sinks "$target"; then
        chain_log "默认输出未能停到 $VIRTUAL_SINK，稍后重试"
        return 1
    fi

    printf 'active\n' >"$CHAIN_STATE_FILE"
    chain_log "链路已激活：模拟 → 数字 → $VIRTUAL_SINK"
    return 0
}

# 目标设备不在位：把默认输出退回到硬件，链路待命（不卸载链路）。
deactivate_chain() {
    local target cur prev id
    target=$(target_name)
    cur=$(default_sink_name)

    case "$cur" in
        collaplex*|'')
            prev=$(cat "$PREV_DEFAULT_FILE" 2>/dev/null || true)
            sink_present "$prev" || prev=$(sinks_tsv |
                awk -F'\t' '$3 == 1 && $1 !~ /^collaplex/ { print $1; exit }')
            sink_present "$prev" || prev=$(sinks_tsv |
                awk -F'\t' '$1 !~ /^collaplex/ { print $1; exit }')
            if [ -n "$prev" ] && sink_present "$prev"; then
                set_default_by_name "$prev" || true
                chain_log "目标设备不在位：默认输出切回 $prev"
            else
                chain_log "目标设备不在位，且没有可用的硬件输出"
            fi
            ;;
    esac

    printf 'waiting\n' >"$CHAIN_STATE_FILE"
    chain_log "链路待命（目标设备：${target:-未记录}）"
}
