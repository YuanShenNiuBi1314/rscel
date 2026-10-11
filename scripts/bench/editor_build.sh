#!/usr/bin/env bash
# 编辑器 `/api/build` 的构建侧计时台（docs/05 P2-③a 的入库台）。
#
# # 为什么另建一台，而不是复用 buildtime.sh
# `buildtime.sh` 量的是**命令行 `rscel build`**，一条命令一个进程、产物名由调用方给定，
# 所以它天然就在命中路径上。这台量的是**编辑器里点一次 build**：走 HTTP、由 `api.rs`
# 自己决定产物名，而 P2-③a 登记的缺陷正是「名字每次递增 ⇒ 缓存键每次不同 ⇒ 永远 miss」。
# 两台的判定量不同（一个是命令行耗时，一个是端到端点一次按钮的耗时），
# 但**统计口径沿用同一套规矩**（不许自造第 4 套）：每轮交错、按轮次奇偶换序、
# min 与中位数都报、判定值用中位数，「最坏/最快」摆动 >15% 只当负载信号并作废本窗。
#
# # 口径（读绝对值前必须知道）
# 与 buildtime.sh 同一条：MSYS 的 bash 每次 fork/exec 一个外部进程都比原生启动贵
# （`rscel --version` 本台 24–25ms / node 台 9–10ms），所以
# **本台的绝对 ms 与 node 台不可混比**；台内部的 first/second 差值是同口径，可以读。
# 这里的每个读数还包含一次 `curl` 的启动与一条 HTTP 往返，两边的计时区**完全同形**
# （同一个 curl、同一个端点、同一个 body），所以比值与差值有效，绝对值偏高是预期的。
#
# 用法：
#   RSC_BIN=target/scratch/rscel_probe.exe bash scripts/bench/editor_build.sh
#   RSC_EB_REPS=9 RSC_EB_DOC=bench/read_share.rscel bash scripts/bench/editor_build.sh
#   RSC_EB_MODE=debug bash scripts/bench/editor_build.sh
#
# 环境变量：
#   RSC_BIN     被测编译器（默认 target/release/rscel.exe）。**建议指到 scratch 里的副本**：
#               服务进程会占住那个 exe（docs/05「环境坑」：Windows 不允许替换运行中的 exe，
#               后果是后面 `cargo build --release` 整批失败，而门禁三项全都不受影响）。
#   RSC_EB_PORT 端口（默认 8439；被占就自己换一个，别杀别人的进程）。
#   RSC_EB_DOC  要构建的 DSL（默认 examples/multi_branch_die.rscel）。
#   RSC_EB_MODE release|debug（默认 release）。
#   RSC_EB_REPS 轮数（默认 7；判定要跨窗同号就多跑几遍本脚本）。
set -uo pipefail
cd "$(dirname "$0")/../.."

RSC="${RSC_BIN:-target/release/rscel.exe}"
PORT="${RSC_EB_PORT:-8439}"
DOC="${RSC_EB_DOC:-examples/multi_branch_die.rscel}"
MODE="${RSC_EB_MODE:-release}"
REPS="${RSC_EB_REPS:-7}"
OUT="target/scratch/editor_build"
BASE="http://127.0.0.1:$PORT"

[[ -f "$RSC" ]] || { echo "找不到编译器：$RSC"; exit 1; }
[[ -f "$DOC" ]] || { echo "缺少 DSL：$DOC"; exit 1; }
case "$MODE" in release|debug) ;; *) echo "RSC_EB_MODE 只认 release|debug，收到 $MODE"; exit 1 ;; esac

now_us() {
  local s="${EPOCHREALTIME%.*}" f="${EPOCHREALTIME#*.}"
  NOW_US=$(( s * 1000000 + 10#$f ))
}

# 读数列表（每行一个微秒整数）→ "min med max n"（ms）
stat_of() {
  sort -n "$1" | awk '
    { a[NR] = $1 }
    END {
      if (NR == 0) { print "nan nan nan 0"; exit }
      med = (NR % 2) ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2
      printf "%.1f %.1f %.1f %d", a[1] / 1000, med / 1000, a[NR] / 1000, NR
    }'
}

# 从响应里取一个字符串字段（这里只要 exePath）。
# **为什么不用 awk RS="\"" 那套**：本批第一版那么写，结果 `getline` 取到的是键值之间
# 那个 `: `，`exePath` 变成空串 → `CACHE_DIR` 也是空的 → `rm -rf` 清不掉缓存，
# 于是"清缓存后的冷构建"其实一直是命中，读数快 4 倍而形状判成 incrementing。
# 这类错不会让任何一行报错，只会让两张表都读错，所以现在按行取、并且**取不到就退出**。
json_str() {
  grep -o "\"$2\": \"[^\"]*\"" "$1" | head -1 | sed "s/^\"$2\": \"//; s/\"$//" | sed 's/\\\\/\\/g'
}

stop_server() {
  [[ -n "${SRV:-}" ]] || return 0
  kill "$SRV" 2>/dev/null
  # MSYS 的 kill 对某些 Windows 进程不生效，而端口留着会让下一批复用同一个服务
  # （读到的是**上一个批次**的行为）。按端口找真 pid 兜一次。
  if curl -s -o /dev/null --max-time 2 "$BASE/api/initial" 2>/dev/null; then
    local winpid
    winpid="$(netstat -ano 2>/dev/null | awk -v p=":$PORT" '$4=="LISTENING" && $2 ~ p"$" {print $5; exit}')"
    [[ -n "$winpid" ]] && taskkill //PID "$winpid" //F >/dev/null 2>&1
  fi
  SRV=""
}
trap stop_server EXIT

rm -rf "$OUT"
mkdir -p "$OUT"
echo "== 编辑器 /api/build 构建侧计时：MODE=$MODE REPS=$REPS 文档=$DOC 编译器=$RSC =="
echo "   端口 $PORT；每轮清空会话缓存目录后连点两次 build（first=清缓存后第一次，second=同一条源文本第二次）"
echo

"$RSC" edit "$DOC" --port "$PORT" >"$OUT/server.log" 2>&1 &
SRV=$!
for ((i = 0; i < 100; i++)); do
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$BASE/api/initial" 2>/dev/null)"
  [[ "$code" == "200" ]] && break
  sleep 0.2
done
[[ "${code:-}" == "200" ]] || { echo "服务没起来（端口 $PORT）："; cat "$OUT/server.log"; exit 1; }

: > "$OUT/first.txt"
: > "$OUT/second.txt"
names=""
hits=0
misses=0
retries=0
post_build() {
  local out="$1"
  curl -s -X POST --data-urlencode "source@$DOC" --data "mode=$MODE" \
    --max-time 120 "$BASE/api/build" > "$out" 2>"$out.curlerr"
}

for ((r = 1; r <= REPS; r++)); do
  # **这一对不换序**（与 buildtime.sh 的奇偶换序是唯一的差别，理由要写死）：
  # first 与 second 是**生产者/消费者**关系 —— first 把条目写进缓存，second 才有得命中。
  # 换序的后果不是"排队位置"而是**量的东西变了**：`second` 会吃到上一轮留下的条目，
  # 而 `first` 清完缓存必然 miss，两条读数就不再是同一对。
  # 交错与复现带在这里由「同一条读数重复 REPS 次 + min/med/摆动」承担。
  for k in first second; do
    if [[ "$k" == first ]]; then
      # 清缓存落在**计时区之外**（`rm`/`mkdir` 在 MSYS 上各是一次 fork，
      # 记进读数就成了"构建比实际贵"）。
      [[ -z "${CACHE_DIR:-}" ]] || rm -rf "$CACHE_DIR"
    fi
    now_us; t0="$NOW_US"
    post_build "$OUT/$k.$r.json"
    rc=$?
    now_us; t1="$NOW_US"
    if [[ $rc -ne 0 ]]; then
      # **分清两种后果**：服务死了后面每一条都会挂（本批实测在 8463 上撞到，
      # 发生在第 5 轮的 `second`，服务日志空白、curl 无 stderr —— 那就是连接被吞）；
      # 而服务还活着时单次连接挂掉只是传输层抖动，重试一次就能把这轮救回来。
      # 不重试的代价是本批实测的：三窗里两窗因此整个作废。
      if curl -s -o /dev/null --max-time 3 "$BASE/api/initial" 2>/dev/null; then
        retries=$((retries + 1))
        now_us; t0="$NOW_US"
        post_build "$OUT/$k.$r.json"
        rc=$?
        now_us; t1="$NOW_US"
        [[ $rc -eq 0 ]] || { echo "FAIL curl($k,$r) 重试仍 exit=$rc：$(tr -d '\r' < "$OUT/$k.$r.json.curlerr" | head -2)"; exit 1; }
      else
        echo "FAIL curl($k,$r) exit=$rc 且服务已无响应 —— 本窗作废（后面的读数全都不可用）。"
        echo "     服务端日志尾部：$(tr -d '\r' < "$OUT/server.log" | tail -2)"
        exit 1
      fi
    fi
    echo $(( t1 - t0 )) >> "$OUT/$k.txt"
    exe="$(json_str "$OUT/$k.$r.json" exePath)"
    [[ -n "$exe" ]] || { echo "FAIL 解析不出 exePath（读的是 $OUT/$k.$r.json）—— 台子的取字段坏了，读数作废"; exit 1; }
    [[ "$exe" == *"-last.exe" ]] && shape=stable || shape=incrementing
    # 累加而不是前置空格：`uniq -c` 会把那个空项数成一行，报告里就多出一个没来由的 "1"。
    names="${names:+$names }$shape"
    # **自检（本批踩过的那类错就靠它兜）**：`first` 是清完缓存之后的第一次构建，
    # 它含"增量缓存命中"只有一种解释 —— 上面那个 `rm -rf` 清的是个空路径（解析器坏了），
    # 或者缓存落在别处。那时整张表的"冷构建"其实全是命中，读数会**快 4 倍且毫无报错**。
    if [[ "$k" == first ]] && grep -q "增量缓存命中" "$OUT/$k.$r.json"; then
      echo "FAIL 清缓存后的第一次构建报了命中 —— 本台的缓存清理失效，读数作废（查 exePath 解析与 CACHE_DIR）"
      exit 1
    fi
    if [[ "$k" == second ]]; then
      if grep -q "增量缓存命中" "$OUT/$k.$r.json"; then hits=$((hits + 1)); else misses=$((misses + 1)); fi
    fi
    # 会话缓存根：`api.rs` 把源文件写在 artifact_dir，`build_cache::cache_root` 对
    # 临时目录里的源文件就落在**它自己的目录**，所以 `.rscel-cache` 与 exePath 同目录。
    CACHE_DIR="${exe%\\*}/.rscel-cache"
  done
done

read -r f_min f_med f_max f_n < <(stat_of "$OUT/first.txt")
read -r s_min s_med s_max s_n < <(stat_of "$OUT/second.txt")
swing_f="$(awk -v a="$f_min" -v b="$f_max" 'BEGIN{printf "%.1f", (b-a)/a*100}')"
swing_s="$(awk -v a="$s_min" -v b="$s_max" 'BEGIN{printf "%.1f", (b-a)/a*100}')"
shapes="$(echo "$names" | tr ' ' '\n' | sort | uniq -c | tr '\n' ' ')"

awk -v fm="$f_med" -v fmin="$f_min" -v sm="$s_med" -v smin="$s_min" -v hits="$hits" \
    -v reps="$REPS" -v misses="$misses" -v sf="$swing_f" -v ss="$swing_s" -v shapes="$shapes" \
    -v retries="$retries" -v spread="$(awk -v a="$s_min" -v b="$s_max" 'BEGIN{printf "%.1f", b-a}')" '
  END {
    printf "清缓存后第一次(miss) min=%.1f med=%.1f | 同文本第二次 min=%.1f med=%.1f | 第二次相对第一次省 %.1fms（%.2fx）\n",
           fmin, fm, smin, sm, fm - sm, fm / sm
    printf "第二次命中 %d/%d 窗，未命中 %d 次 | 产物名形状：%s | 连接被吞后重试成功 %d 次\n",
           hits, reps, misses, shapes, retries
    printf "摆动 first=%s%% second=%s%%（命中档绝对散布 %sms）%s\n\n", sf, ss, spread,
           (sf > 15 || ss > 15 ? "  ← 摆动 >15%：这一档读得出 4x 这个量级，读不了小数点后面" : "")
  }'

echo "判读：$shapes 里出现 incrementing 就是 P2-③a 那个缺陷本身（缓存键含产物名 ⇒ 每次 miss）；"
echo "     只有 stable 且第二次命中，才算修好了。命中数必须是 $REPS/$REPS，"
echo "     少一次就说明稳定名被某次退路换掉了（看 stderr 里那句「写不进去」）。"
echo "跨窗：判定要同号，再跑一遍本脚本（或 RSC_EB_REPS=9）。"
