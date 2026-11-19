#!/bin/bash
# target-watch.sh —— 常驻监视目标声卡
#
#   在位 → activate_chain：模拟 → 数字 → 虚拟声卡，链路生效
#   不在位 → deactivate_chain：默认输出退回硬件，链路待命
#
# 用法：
#   target-watch.sh          常驻（单实例，PID 记录在 state/watch.pid）
#   target-watch.sh --once   立即判定一次，交给 install.sh 起停时用

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/chain-lib.sh
. "$ROOT/scripts/chain-lib.sh"

mkdir -p "$STATE_DIR"

one_shot() {
    local target state
    target=$(target_name)
    if [ -n "$target" ] && sink_present "$target"; then
        state=$(cat "$CHAIN_STATE_FILE" 2>/dev/null || echo stopped)
        if [ "$state" != active ] || ! chain_linked "$(out_node_name)" "$target"; then
            activate_chain
        fi
    else
        deactivate_chain
    fi
}

if [ "${1:-}" = "--once" ]; then
    exec 9>"$STATE_DIR/chain.lock"
    flock -w 30 9 2>/dev/null || true
    one_shot
    exit $?
fi

PIDFILE="$STATE_DIR/watch.pid"
if [ -f "$PIDFILE" ]; then
    old=$(cat "$PIDFILE" 2>/dev/null || true)
    if [ -n "$old" ] && kill -0 "$old" 2>/dev/null; then
        exit 0
    fi
fi
printf '%s\n' "$$" >"$PIDFILE"
trap 'rm -f "$PIDFILE"' EXIT

chain_log "设备监视启动 (pid $$)"

while :; do
    exec 9>"$STATE_DIR/chain.lock"
    if flock -w 30 9 2>/dev/null; then
        target=$(target_name)
        if [ -n "$target" ]; then
            state=$(cat "$CHAIN_STATE_FILE" 2>/dev/null || echo stopped)
            if sink_present "$target"; then
                if [ "$state" != active ] || ! chain_linked "$(out_node_name)" "$target"; then
                    activate_chain || true
                fi
            else
                if [ "$state" != waiting ]; then
                    deactivate_chain || true
                fi
            fi
        fi
        flock -u 9 2>/dev/null || true
    fi
    exec 9>&-
    sleep 2
done
