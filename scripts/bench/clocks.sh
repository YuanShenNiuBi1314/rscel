#!/usr/bin/env bash
# 测量用时钟隔离：临时把 CPU 睿频（boost）关掉、把主频钉在基频附近，测完无条件恢复。
#
# 为什么必须"复制方案再改"而不是直接 setacvalueindex 当前方案：
# 直接改会把你自己的电源设置写坏，且事后很难精确还原。复制一份改参数、测完
# `setactive` 回原方案 GUID，等于完全可逆（原方案一个字节都没动）。
#
#   bash scripts/bench/clocks.sh -- <要跑的命令...>     # 关睿频跑命令，退出时自动恢复
#   bash scripts/bench/clocks.sh status                  # 看当前方案与睿频策略
set -euo pipefail

snap="target/bench/power-snapshot.txt"
mkdir -p target/bench

cur_line=$(powercfg -getactivescheme | tr -d '\r' | head -1)
orig=$(printf '%s' "$cur_line" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -1)
[[ -z "$orig" ]] && { echo "拿不到当前电源方案 GUID，拒绝改动电源设置"; exit 1; }

if [[ "${1:-}" == "status" ]]; then
  echo "活动电源方案：$cur_line"
  powercfg -query "$orig" SUB_PROCESSOR PERFBOOSTMODE 2>/dev/null | tr -d '\r' | grep -iE "当前设置|Current Setting|设置索引|Setting" | head -4 || echo "（查不到 PERFBOOSTMODE 项）"
  exit 0
fi

printf 'ORIGINAL_SCHEME=%s\n' "$orig" > "$snap"
dup=$(powercfg -duplicate "$orig" "rscel-bench-clocks" | tr -d '\r')
bench=$(printf '%s' "$dup" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | tail -1)
[[ -z "$bench" ]] && { echo "复制电源方案失败：$dup"; exit 1; }
printf 'BENCH_SCHEME=%s\n' "$bench" >> "$snap"

restore() {
  powercfg -setactive "$orig" >/dev/null 2>&1 || echo "!! 恢复失败：请手动执行  powercfg -setactive $orig"
  powercfg -delete "$bench" >/dev/null 2>&1 || true
  echo "[clocks] 已恢复原电源方案 $orig（睿频/节能设置回到你原来的样子）"
}
trap restore EXIT

# PERFBOOSTMODE 0 = Disabled（关睿频）；PROCTHROTTLEMIN=MAX=99 让核心停在基频附近、
# 既不上睿频也不掉到更低档，代价是机器会变慢——正是"暂时"的意义。
for g in SUB_PROCESSOR; do
  powercfg -setacvalueindex "$bench" $g PERFBOOSTMODE 0  >/dev/null
  powercfg -setdcvalueindex "$bench" $g PERFBOOSTMODE 0  >/dev/null
  powercfg -setacvalueindex "$bench" $g PROCTHROTTLEMAX 99 >/dev/null
  powercfg -setacvalueindex "$bench" $g PROCTHROTTLEMIN 99 >/dev/null
  powercfg -setdcvalueindex "$bench" $g PROCTHROTTLEMAX 99 >/dev/null
  powercfg -setdcvalueindex "$bench" $g PROCTHROTTLEMIN 99 >/dev/null
done
powercfg -setactive "$bench" >/dev/null
echo "[clocks] 睿频已关闭（临时方案 $bench，AC/DC：boost=Disabled，处理器上限 99%）"
[[ "${1:-}" == "--" ]] && shift
"$@"
