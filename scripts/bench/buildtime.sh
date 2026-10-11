#!/usr/bin/env bash
# 构建侧测量台：把 `rscel build` 的耗时拆成「进程启动 / parse+check / 前端总量 / rustc /
# 缓存命中」，就是 docs/05 P2 ③ 那两张表（阶段占比 + 5.4x–14.0x 加速）的出处。
#
# 为什么不复用 `run.sh`：那台的判定量是「生成物 vs 同构手写的**运行期**比值」，
# 这台量的是**构建**本身，两者没有共同的量，硬套会把分母读成噪声。
# 但统计口径沿用同一套规矩（不许自造第 4 套）：每轮**交错**跑各读数、按轮次奇偶换起始项、
# min 与中位数都报、**判定值用中位数**，而「最坏/最快」摆动只当机器忙不忙的负载信号
# （>15% 就重测，不作判定量）。要下结论仍需**跨窗同号**：多跑几遍本脚本。
#
# 读数以 ms 计（秒表是 bash 内建 `$EPOCHREALTIME`，理由见下面 `now_us` 的注释）。
# 这是**外部进程**耗时，量级 50–1100ms，
# 不受门禁台那条「6.5ms 场景别声称 1–3%」的地板限制 —— 但同一档位差 <10ms 仍然不读。
#
# **本台的口径（读绝对值前必须知道）**：MSYS 的 bash 每次 fork/exec 一个外部进程都比
# 原生进程启动贵，实测同一条命令行在 node 台与本台上差 ~15–25ms（`rscel --version`
# node 台 9–10ms、本台 24–25ms）。所以：
#   - **本台的绝对 ms 与 node 台的不可混比**，跨台只能比方向与量级；
#   - 本台内部的差值（`first − cold`、`build − rustc`）是**同一台同口径**，可以读；
#   - docs/05 P2 ③ 表里那批"5.4x–14.0x / 前端 0.3%–3.4%"是 node 台（`target/scratch/*.mjs`，
#     不入库）先量的，本台复现出来是"4.7x–11.2x / 前端 0.15%–3.5%"——同一结论、口径略高。
#
# 用法：
#   RSC_BIN=target/reldata2/release/rscel.exe bash scripts/bench/buildtime.sh
#   RSC_BT_REPS=9 RSC_BT_MODE=release bash scripts/bench/buildtime.sh      # 第二窗 / 换档
#   RSC_BT_DSLS="bench/pipeline_and.rscel" bash scripts/bench/buildtime.sh  # 只量一份
set -uo pipefail
cd "$(dirname "$0")/../.."

RSC="${RSC_BIN:-target/release/rscel.exe}"
MODE="${RSC_BT_MODE:-release}"
REPS="${RSC_BT_REPS:-7}"
DSLS="${RSC_BT_DSLS:-bench/loop_sum_opaque.rscel bench/pipeline_and.rscel bench/read_share.rscel examples/multi_branch_die.rscel}"
OUT="target/scratch/buildtime"

[[ -x "$RSC" || -f "$RSC" ]] || { echo "找不到编译器：$RSC"; exit 1; }
# rustc 的 flag 必须与 CLI 那套逐字一致（crates/rscel-cli/src/lib.rs 的 rustc_command_args），
# 否则「前端总量 = build − rustc」这个差值就把 flag 差算进了构建成本。
case "$MODE" in
  release) RFLAGS=(--edition=2021 -C opt-level=3 -C codegen-units=1 --error-format=json --crate-type=bin) ;;
  debug)   RFLAGS=(--edition=2021 -C opt-level=0 -g --error-format=json --crate-type=bin) ;;
  *) echo "RSC_BT_MODE 只认 release|debug，收到 $MODE"; exit 1 ;;
esac

# 一个读数列表（每行一个**微秒整数**）→ "min med max n"（ms）
stat_of() {
  sort -n "$1" | awk '
    { a[NR] = $1 }
    END {
      if (NR == 0) { print "nan nan nan 0"; exit }
      med = (NR % 2) ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2
      printf "%.1f %.1f %.1f %d", a[1] / 1000, med / 1000, a[NR] / 1000, NR
    }'
}

# 秒表必须是 bash **内建**的 `$EPOCHREALTIME`。两个已经踩过的坑：
#   ① 用 `date +%s%N` 或 `awk` 做差 —— 那两次 fork/exec 在 MSYS 上各要 40ms 量级，
#      量出来的"进程启动"会是 48.9ms（真实值 9–10ms），整张表被秒表自己污染；
#   ② 打印样本时漏换行 —— `sort -n` 把五个样本看成一行，min=med=max 全都"看着挺正常"，
#      这是最坏的一种错：数字合理但毫无意义。
# `$EPOCHREALTIME` 形如 `1791697445.479660`（小数部分固定 6 位），所以拆成整数微秒做差，
# 全程零子进程 —— 连 `$(now_us)` 这种命令替换都不要，MSYS 上每次 fork 一个子 shell 就是
# 毫秒级的系统性开销，会被算进"被测耗时"里。结果写进全局 `NOW_US`。
now_us() {
  local s="${EPOCHREALTIME%.*}" f="${EPOCHREALTIME#*.}"
  NOW_US=$(( s * 1000000 + 10#$f ))
}

rm -rf "$OUT"
echo "== 构建侧测量：MODE=$MODE REPS=$REPS 编译器=$RSC =="
echo "   （rustc flag: ${RFLAGS[*]}）"
echo

for dsl in $DSLS; do
  [[ -f "$dsl" ]] || { echo "缺少 DSL：$dsl"; exit 1; }
  tag="$(basename "$dsl" .rscel)"
  d="$OUT/$tag"
  mkdir -p "$d"
  cp "$dsl" "$d/$tag.rscel"
  mkdir -p "$d/.rscel-cache"          # 缓存根就近落这一级，各 DSL 互不淘汰
  exe="$d/$tag"
  local_dsl="$d/$tag.rscel"

  # 先产出一份 .rs 给"单跑 rustc"这一读数当输入。
  "$RSC" build "$local_dsl" -o "$exe" "--$MODE" --no-cache 2>"$d/prep.log" 1>/dev/null || {
    echo "FAIL $tag 预热构建失败"; cat "$d/prep.log"; exit 1; }
  rs="$exe.rs"

  for k in startup check cold rustc version first hit; do : > "$d/$k.txt"; done
  hit_cnt=0; miss_cnt=0
  for ((r = 1; r <= REPS; r++)); do
    # 奇偶轮换起始项：交错解决"谁先跑"，奇偶换序缓解"排在谁后面"。
    # first→hit 必须成对且保序（first 把条目写进去，hit 才有得命中）。
    if (( r % 2 == 0 )); then ORDER="startup check cold rustc version"
    else ORDER="cold rustc version check startup"; fi
    ORDER="$ORDER first hit"

    for k in $ORDER; do
      # 清缓存必须在**计时区之外**：`rm`/`mkdir` 在 MSYS 上各是一次 fork，
      # 放进计时区就把"台自己的开销"记成了"带缓存的冷构建比 --no-cache 贵"。
      if [[ "$k" == first ]]; then
        rm -rf "$d/.rscel-cache"; mkdir -p "$d/.rscel-cache"
      fi
      now_us; t0="$NOW_US"
      case "$k" in
        startup) "$RSC" --version >/dev/null 2>&1 ;;
        check)   "$RSC" check "$local_dsl" >/dev/null 2>&1 ;;
        cold)    "$RSC" build "$local_dsl" -o "$exe" "--$MODE" --no-cache >/dev/null 2>&1 ;;
        rustc)   rustc "${RFLAGS[@]}" "$rs" -o "$d/raw.exe" >/dev/null 2>&1 ;;
        # 缓存身份里那一格 `rustc -vV` 单独计时：它是"带缓存的冷构建比 --no-cache 贵"
        # 的主要构成，不单独量就会把它误记到"存条目"头上。
        version) rustc -vV >/dev/null 2>&1 ;;
        first)   "$RSC" build "$local_dsl" -o "$exe" "--$MODE" 2>"$d/first.log" >/dev/null ;;
        hit)     "$RSC" build "$local_dsl" -o "$exe" "--$MODE" 2>"$d/hit.log" >/dev/null ;;
      esac
      now_us; t1="$NOW_US"
      echo $(( t1 - t0 )) >> "$d/$k.txt"
    done
    # 命中性检查：读数只有在"该命中"时命中才有意义（否则 med(hit) 其实是冷构建）。
    grep -q "增量缓存命中" "$d/first.log" || miss_cnt=$((miss_cnt + 1))
    grep -q "增量缓存命中" "$d/hit.log" && hit_cnt=$((hit_cnt + 1))
  done

  read -r c_min c_med c_max c_n < <(stat_of "$d/cold.txt")
  read -r r_min r_med r_max r_n < <(stat_of "$d/rustc.txt")
  read -r h_min h_med h_max h_n < <(stat_of "$d/hit.txt")
  read -r f_min f_med f_max f_n < <(stat_of "$d/first.txt")
  read -r s_min s_med s_max s_n < <(stat_of "$d/startup.txt")
  read -r k_min k_med k_max k_n < <(stat_of "$d/check.txt")
  read -r v_min v_med v_max v_n < <(stat_of "$d/version.txt")
  swing_cold="$(awk -v a="$c_min" -v b="$c_max" 'BEGIN{printf "%.1f", (b-a)/a*100}')"
  swing_hit="$(awk -v a="$h_min" -v b="$h_max" 'BEGIN{printf "%.1f", (b-a)/a*100}')"
  awk -v c="$c_med" -v cmin="$c_min" -v r="$r_med" -v rmin="$r_min" -v h="$h_med" -v hmin="$h_min" \
      -v tag="$tag" \
      -v sc="$swing_cold" -v sh="$swing_hit" -v hc="$hit_cnt" -v reps="$REPS" \
      -v miss="$miss_cnt" -v s="$s_med" -v k="$k_med" -v fn="$f_med" -v v="$v_med" '
    END {
      fe = c - r
      printf "%-18s 冷构建(--no-cache) min=%.1f med=%.1f | 单跑 rustc min=%.1f med=%.1f | 前端=build-rustc med=%.1f (%.2f%% of build) | 进程启动 med=%.1f | parse+check(进程内) med=%.1f\n",
             tag, cmin, c, rmin, r, fe, fe / c * 100, s, k - s
      printf "%-18s 带缓存冷构建(miss+存) med=%.1f (比 --no-cache 贵 %.1fms，其中 rustc -vV 自己 med=%.1f) | 命中 min=%.1f med=%.1f | 加速(中位)=%.1fx | 命中 %d/%d、首次误命中 %d 次 | 摆动 cold=%s%% hit=%s%%%s\n\n",
             tag, fn, fn - c, v, hmin, h, c / h, hc, reps, reps - miss, sc, sh, (sc > 15 || sh > 15 ? "   ← 摆动 >15%，本窗按纪律作废重测" : "")
    }'
  rm -f "$d"/*.exe "$d"/*.pdb
done

echo "提示：判定要**跨窗同号**，再跑一遍本脚本（或直接 RSC_BT_REPS=9）对表；"
echo "     docs/05 P2 ③ 的两窗数字就是这么来的（N=7 与 N=9）。"
