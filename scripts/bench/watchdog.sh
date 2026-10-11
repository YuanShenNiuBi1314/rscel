#!/usr/bin/env bash
# 空闲窗测量看门狗：等（或直接清）掉 IDE 负载，再跑权威性能测量。
#
# 为什么需要它（实测依据）：
#   - 绑核 + 同轮配对比值 + 周期归一化之后，配对比值带宽仍停在 ±18.7%（归一化前 ±26.7%）。
#     频率漂移已被探针除掉，剩下的量级只能来自**抢占**：tasklist 里有 6+ 个
#     Qoder CN.exe（两个各 ~1GB）与每 2 小时一轮的自动批次（会起 cargo/rustc）。
#   - 这些负载在 IDE 里跑不掉：测量进程一旦被 IDE 的调度/索引/GC 抢走时间片，
#     任何统计量都救不回来。所以"关掉 IDE 再测"不是洁癖，是必要条件。
#
# 三种用法：
#   1) 手动（推荐）：先自己退出 IDE，再跑
#        bash scripts/bench/watchdog.sh --wait 0 --reps 15
#   2) 让它等：轮询到没有 Qoder 进程为止（最长 --timeout 秒），然后测
#        bash scripts/bench/watchdog.sh --wait 3600 --reps 15
#   3) 交给 Windows 任务计划程序，在 IDE 未启动的时段跑（例如凌晨），并让它
#      先把 IDE 杀掉再测 —— 这条会在你正在用机器时强杀窗口，务必自己确认：
#        schtasks /Create /TN rscel-idle-bench /SC WEEKLY /D SAT /ST 04:30 ^
#          /TR "'C:\Program Files\Git\bin\bash.exe' -lc 'cd <仓库路径> " && "bash scripts/bench/watchdog.sh --kill --reps 15'"
#
# 输出：target/bench/idle-run-<UTC时间戳>.md（不自动覆盖 docs/04，由批次读过后再决定是否采信）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

WAIT=0
TIMEOUT=7200
REPS=15
KILL=0
GATE="${RSC_GATE:-97}"
BAND_MAX="${RSC_SBX_BAND:-5}"
while [[ $# -0 -gt 0 ]]; do
  case "$1" in
    --wait) WAIT="${2:-0}"; shift 2 ;;
    --timeout) TIMEOUT="${2:-7200}"; shift 2 ;;
    --reps) REPS="${2:-15}"; shift 2 ;;
    --kill) KILL=1; shift ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done

IDE_RE='Qoder[ .A-Za-z]*\.exe'

ide_running() { tasklist 2>/dev/null | tr -d '\r' | grep -icE "$IDE_RE" || true; }

if [[ "$KILL" == "1" ]]; then
  echo "[watchdog] --kill：即将结束所有 IDE 进程（会同时干掉正在跑的自动批次与你的编辑器会话）"
  taskkill //F //IM "Qoder CN.exe" >/dev/null 2>&1 || taskkill //F //IM Qoder.exe >/dev/null 2>&1 || true
  sleep 5
fi

if [[ "$WAIT" == "0" ]]; then
  n=$(ide_running)
  if [[ "$n" -gt 0 ]]; then
    echo "[watchdog] 仍有 $n 个 IDE 进程在跑 —— 现在的测量结果只会是 INCONCLUSIVE。"
    echo "[watchdog] 用 --wait <秒> 让它空闲后再测，或 --kill 强制清场（会关掉你的编辑器）。"
    exit 3
  fi
else
  start=$(date +%s)
  while :; do
    n=$(ide_running)
    [[ "$n" -eq 0 ]] && break
    if (( $(date +%s) - start > TIMEOUT )); then
      echo "[watchdog] 等待 ${TIMEOUT}s 超时，仍有 $n 个 IDE 进程；放弃测量。"; exit 4
    fi
    sleep 20
  done
  echo "[watchdog] IDE 已退出，等 15s 让索引/杀软后台沉降"
  sleep 15
fi

[[ -x target/bench/rscel-sbx.exe ]] || rustc -A dead_code --edition=2021 -O \
  -o target/bench/rscel-sbx.exe scripts/bench/sandbox.rs

stamp=$(date -u +%Y%m%dT%H%M%SZ)
out="target/bench/idle-run-$stamp.md"
{
  echo "# 空闲窗权威测量（$stamp，reps=$REPS，gate=$GATE%，band_max=±$BAND_MAX%）"
  echo
  echo '```'
} > "$out"

# 场景清单与 scripts/bench/run.sh 保持一致；缺二进制的先构建。
for sc in loop_sum loop_sum_opaque pipeline_and read_share async_step; do
  gen="target/bench/${sc}_gen.exe"; fnh="target/bench/${sc}_fn_hand.exe"
  if [[ ! -x "$gen" || ! -x "$fnh" ]]; then
    echo "[watchdog] 构建场景 $sc"
    cargo build -q --release -p rscel-cli
    ./target/release/rscel.exe build "bench/$sc.rscel" -o "$gen" --release >/dev/null 2>&1 || true
    rustc --edition=2021 -C opt-level=3 -C codegen-units=1 \
      -o "$fnh" "bench/${sc}_fn_hand.rs" >/dev/null 2>&1 || true
  fi
  [[ -x "$gen" && -x "$fnh" ]] || { echo "跳过 $sc（二进制缺失）"; continue; }
  echo "## $sc" >> "$out"
  # 掩码 0x3 = 一整个物理核（两个超线程兄弟）：只绑单个逻辑核会放大噪声，
  # 因为同胞核仍在同一物理核上被别的负载共用（已实测）。
  ./target/bench/rscel-sbx.exe --mask 0x3 --rounds "$REPS" --boost \
      --band-max "$BAND_MAX" --gate "$GATE" \
      -- "fn_hand=$fnh" "gen=$gen" 2>&1 | tee -a "$out"
done

echo '```' >> "$out"
echo "[watchdog] 报告：$out"
# 结论不自动写进 docs/04：是否采信由批次/人读过带宽后决定，
# 否则就是在把噪声固化成"官方数字"。
