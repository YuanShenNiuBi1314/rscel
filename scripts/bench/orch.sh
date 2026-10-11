#!/usr/bin/env bash
# 编排形状的控制变量探针（docs/05 P0-5 第 1 项 + 第 4 项）。
#
# 它回答两个只能用数据回答的问题，两个问题共用同一个时间窗：
#
# · **第 4 项**（已归因，本脚本是它的取证工具）：`pipeline_and` 的生成物曾经测出比
#   **同构手写基线**快 4%–6%，方向反常。`bench/pipeline_and_fn_hand.rs` 声明的第 ③ 处有意差异
#   （supervisor 交接：手写侧 `join()` vs 生成物 `Arc<__RscGroup>`+`Condvar`+`catch_unwind`）
#   从未单独隔离测过。A0/A4 两格就是为它建的：实测**换掉这一次交接 = 分母快 5.4%–8.7%**，
#   分母不同构会让门禁虚高 → 基线已改成与运行时一致的 publish/wait（见该文件头）。
# · **第 1 项**：`jump and` 派生的 worker 还带着函数边界（`cell_w` 在 `spawn` 闭包里被调）。
#   P0-4 的展开只覆盖「单一 plain 调用点的同步 Cell」，这一类被闸门排除。
#   按纪律必须**先证明有收益**再动生成器，所以这里加一格「body 逐字展开进 spawn 闭包」。
#
# 与 `ab.sh` 的分工：`ab.sh` 是**数值热循环**（`loop_sum_opaque`）的矩阵，判据是
# `--emit=asm` 里的 `padd` 条数；本脚本是**编排形状**（互斥锁出队主导、无可向量化归约），
# `padd` 在这里恒为 0，所以判据换成「同窗耗时 + 相位复现带」。两张探针台的统计量在
# 2026-10-11 都换成了与门禁台 `run.sh` **同一套**（两相位各取中位数、平均作判定值、
# 差作带、`min` 只作参考列），否则探针读数与门禁读数无法对账。
#
# 阶梯（每格相对上一格只动一个因素，格与格的差异用字符串替换机械生成，
# 替换锚点没命中就**直接退出**——`ab.sh` 第一版有过 `.replace(X, X)` 的空操作格，
# 那一格从来没测到它声称的因素，别再犯）：
#
#   A0_base 今天的门禁分母：`bench/pipeline_and_fn_hand.rs` 原样
#   A4_join 相对 A0 只把交接换回 `join()` —— **复现**上面那条归因（分母自己多付一次 join）
#   A1_collect 相对 A0 只加生成物那句 `items.into_iter().collect()` 整表再物化
#   A2_worker  相对 A1 只把 worker 体换成生成物 `cell_w` 的 body 逐字形状
#   A3_hint    相对 A2 只加生成物那两个内联提示 → A3 与 GEN_real 只剩"运行时死代码 / argv
#              采集 / `let _ = cell_main()` 深度"这类 docs/06 已判过不收费的形状差
#   GEN_real   真实生成物（锚点行：`rscel build bench/pipeline_and.rscel --release`）
#   G1_expand  相对 GEN_real 只把 `fn cell_w` 的 body 展开进 spawn 闭包并删掉该函数
#
# A 系列的分母已经改成 publish/wait（docs/05 P0-5 第 4 项的产出），所以这一版阶梯回答的是
# **新问题**：修完分母之后，生成物比同构手写慢的那 1%–3% 是谁收的费。
#
# 用法（锚点是照 `pipeline_and` 的基线与生成物写的；换场景要把 `sub1` 的锚点一起改）：
#   bash scripts/bench/orch.sh
#   RSC_ORCH_REPS=41 bash scripts/bench/orch.sh     # 压小复现带（默认 21）
#   RSC_ORCH_ONLY="A0_base A4_join GEN_real" bash scripts/bench/orch.sh   # 只跑选中的格
#   RSC_BIN=/path/to/rscel.exe bash scripts/bench/orch.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
DIR=target/ab/orch
mkdir -p "$DIR"
REPS="${RSC_ORCH_REPS:-21}"
SCENARIO="pipeline_and"
ANCHOR_DSL="bench/${SCENARIO}.rscel"
HAND_BASELINE="bench/${SCENARIO}_fn_hand.rs"
# 与 rscel-cli release 模式逐字一致（crates/rscel-cli/src/lib.rs::BuildMode::rustc_args）。
FLAGS=(--edition=2021 -C opt-level=3 -C codegen-units=1 --crate-type=bin)
# 校验值前缀：这些程序必须算出**同一件事**，否则比值无意义（ab.sh 的老规矩）。
CHECK_KEYS='^(checksum|done|spins)='

rm -f "$DIR"/A*.rs "$DIR"/A*.exe "$DIR"/G*.rs "$DIR"/G*.exe "$DIR"/F*.rs "$DIR"/F*.exe \
      "$DIR"/gen.rs "$DIR"/gen.exe

# ---------- 真实生成物（锚点行 + A2/G1 的取材来源）----------
if [[ -n "${RSC_BIN:-}" ]]; then
  RSC="$RSC_BIN"
else
  echo "== 构建 rscel CLI =="
  cargo build -q --release -p rscel-cli
  RSC="$ROOT/target/release/rscel.exe"
fi
[[ -x "$RSC" ]] || { echo "rscel 二进制不存在: $RSC"; exit 1; }
echo "== 生成锚点产物：$ANCHOR_DSL =="
"$RSC" build "$ANCHOR_DSL" -o "$DIR/gen.exe" --release >"$DIR/gen.build.log" 2>&1 \
  || { echo "rscel build 失败，日志："; cat "$DIR/gen.build.log"; exit 1; }
GEN_RS="$DIR/gen.rs"
[[ -f "$GEN_RS" ]] || { echo "找不到生成物源码 $GEN_RS"; exit 1; }

python - "$DIR" "$SCENARIO" "$HAND_BASELINE" <<'PY'
import io, os, re, sys

d, scenario, hand_path = sys.argv[1], sys.argv[2], sys.argv[3]

manifest = []


def read(p):
    return io.open(p, encoding='utf-8', newline='\n').read()


def write(name, src, note):
    io.open(os.path.join(d, name + '.rs'), 'w', encoding='utf-8', newline='\n').write(src)
    manifest.append((name, note))


def sub1(text, old, new, where):
    """替换一次且仅一次；锚点失配或替换后无变化都直接退出。

    为什么这么凶：一个「以为自己只动了一个因素、实际上什么都没动」的格子，
    读出来的差值会被当成该因素的成本 —— ab.sh 第一版的 V05_nocopy 就是这么废的。
    """
    n = text.count(old)
    if n != 1:
        sys.exit('%s：锚点出现 %d 次（应为 1）→ %s 的形状已变，请把脚本里的锚点同步更新，'
                 '不要静默跳过这一格' % (where, n, scenario))
    out = text.replace(old, new)
    if out == text:
        sys.exit('%s：替换后文本没变化，这一格是空操作' % where)
    return out


HAND = read(hand_path)
GEN = read(os.path.join(d, 'gen.rs'))

# `fn cell_w(...) -> Option<RetW> {` —— 名字取自 DSL 里 `jump and` 的目标 Cell。
# 连它上面那行 `#[inline(...)]` 一起抓：G1 要删的是**整个**函数定义，留下属性行会直接
# 编译失败（孤零零一行 `#[inline(always)]` 后面没有 item）。
# 正则**停在 `{` 不吃换行**：`block()` 返回的 body 从 `{` 之后开始、自带那个换行，
# 这里多吃一个 `\n` 会让 `SIG + BODY + '}'` 拼不回原文（实测把 G1 变成静默空操作）。
CELL_W_SIG_RE = re.compile(r'^(?:#\[inline[^\n]*\]\n)?fn cell_w\([^\n]*\{', re.M)
_m = CELL_W_SIG_RE.search(GEN)
if _m is None:
    sys.exit('生成物里找不到 `fn cell_w(...) {`：`jump and` 的 worker 形状变了，'
             'F2/G1 两格的取材需要同步更新')
SIG = _m.group(0)


def block(src, anchor_start):
    """返回 anchor 处 `{` 的**内部**文本，以及该函数（含签名行到闭括号）的整体区间。"""
    brace = src.index('{', anchor_start)
    depth = 0
    for k in range(brace, len(src)):
        c = src[k]
        if c == '{':
            depth += 1
        elif c == '}':
            depth -= 1
            if depth == 0:
                body = src[brace + 1:k]
                # 花括号配平是靠"body 里没有字符串/注释里的括号"这个前提；
                # worker body 由生成器发的是纯算术与结构体字面量，一旦这个前提破了
                # 必须立刻说，而不是产出一个"某一格其实编译不过"的表。
                if '"' in body or "'" in body or '//' in body:
                    sys.exit('cell_w body 里出现串或注释，本脚本的括号配平提取不再安全')
                return body, brace, k + 1
    sys.exit('花括号没配平')


BODY, _, _ = block(GEN, _m.start())
if SIG + BODY + '}' not in GEN:
    sys.exit('拼不回原文：SIG+BODY+"}" 在生成物里不存在 → block() 的区间取错了，'
             '后面每一格都会是静默空操作')

# ---------- A0：今天的门禁分母，逐字照抄 ----------
write('A0_base', HAND,
      '起点＝门禁分母 %s 原样（交接已是生成物那份 publish/wait、worker 体"算完即丢"、无额外物化、无内联提示）'
      % hand_path)

# ---------- A4：只把交接换回 join() —— 复现 P0-5 第 4 项那条归因 ----------
A4_HANDOFF_OLD = '''    let g = Arc::new(Group::<RetW>::new());
    let g2 = Arc::clone(&g);
    std::thread::spawn(move || group_run_sync(items, 8, 0, &g2));
    let res: Vec<Option<RetW>> = g.wait();
    let _ = res;
'''
A4_HANDOFF_NEW = '''    let supervisor = std::thread::spawn(move || run_group_sync(items, 8, 0));
    let res = supervisor.join();
    let _ = res;
'''
a4 = sub1(HAND, A4_HANDOFF_OLD, A4_HANDOFF_NEW, 'A4 的交接换回 join')
write('A4_join', a4,
      '相对 A0 只把 supervisor 交接换回 `join()`（`Group` 留在文件里当死代码）：'
      '这一格复现 P0-5 第 4 项的归因 —— 分母多付一次线程 join 就要慢 5%–9%')

# ---------- A1：只加生成物那句额外物化 ----------
a1 = sub1(HAND, '    let g = Arc::new(Group::<RetW>::new());',
          '    let items: Vec<i64> = { items.into_iter().collect() };\n'
          '    let g = Arc::new(Group::<RetW>::new());', 'A1 的额外 collect')
write('A1_collect', a1,
      '相对 A0 只加生成物那句 `items.into_iter().collect()` 整表再物化'
      '（DSL 的 `over items` 已经是 `Vec`，这一步是生成器为了通吃"任意集合表达式"付的税）')

# ---------- A2：只换 worker 体 ----------
# 顺手把生成物 `RetStage1` 的**声明**也搬过来：worker body 里有 `Some(RetStage1 { d: y })`，
# 没有这个结构体就编译不过。它不是第二个被测因素 —— 这个类型只在被丢掉的那个块表达式里
# 出现一次，没有任何别处引用它。
a2 = sub1(a1, 'struct RetW {}',
          'struct RetW {}\n\n#[derive(Default)]\nstruct RetStage1 {\n    pub d: i64,\n}',
          'A2 的 RetStage1 声明')
a2 = sub1(a2,
          '''fn w(x: i64) -> Option<RetW> {
    let y = x * 2 + 1;
    let _ = y;
    Some(RetW {})
}''',
          'fn w(x: i64) -> Option<RetW> {' + BODY + '}',
          'A2 的 worker 体')
write('A2_worker', a2,
      '相对 A1 只把 worker 体换成生成物 `cell_w` 的 body 逐字形状'
      '（含 `RetStage1` 构造后丢弃；**不含**它的 `#[inline(always)]`，那是下一格）')

# ---------- A3：只加内联提示 ----------
a3 = sub1(a2, 'fn w(x: i64) -> Option<RetW> {',
          '#[inline(always)]\nfn w(x: i64) -> Option<RetW> {', 'A3 的 worker 提示')
a3 = sub1(a3, 'fn cell_main() {', '#[inline]\nfn cell_main() {', 'A3 的 cell_main 提示')
write('A3_hint', a3,
      '相对 A2 只加生成物那两个内联提示（`w` 用 `#[inline(always)]`、`cell_main` 用 `#[inline]`）；'
      'A3 与 GEN_real 之间剩下的差异就只有内嵌运行时死代码、argv 采集与 `let _ = cell_main()` 的深度')

# ---------- G1：把 cell_w 的 body 展开进 spawn 闭包（P0-5 第 1 项）----------
CALL_ANCHOR = '__rsc_group_run_sync(__rsc_items, __rsc_lim, 0, &__rsc_g2, |__rsc_it| cell_w(__rsc_it));'
if GEN.count(CALL_ANCHOR) != 1:
    sys.exit('生成物里找不到唯一的 `jump and` 派生调用行（出现 %d 次），'
             'G1 展开格需要更新锚点' % GEN.count(CALL_ANCHOR))
# 只动两处：删掉 `fn cell_w` 整个定义，把它的 body 逐字放进 spawn 闭包。
g1 = GEN.replace(SIG + BODY + '}\n', '')
g1 = sub1(g1, CALL_ANCHOR,
          '__rsc_group_run_sync(__rsc_items, __rsc_lim, 0, &__rsc_g2, |__rsc_it| {'
          ' let x: i64 = __rsc_it;' + BODY + '            });', 'G1 的闭包展开')
if 'fn cell_w' in g1:
    sys.exit('G1 展开后仍残留 `fn cell_w` —— 函数边界没真的消掉，这一格测的不是"展开"')
write('G1_expand', g1,
      '相对 GEN_real 只把 `fn cell_w` 的 body 逐字展开进 `spawn` 闭包里的 `f`，'
      '并删掉 `cell_w` 本身（P0-5 第 1 项：worker 侧的函数边界值不值钱）')

with io.open(os.path.join(d, 'manifest.tsv'), 'w', encoding='utf-8',
             newline='\n') as f:
    for name, note in manifest:
        f.write('%s\t%s\n' % (name, note))
print('已生成 %d 个探针' % len(manifest))
PY

# ---------- 登记表 ----------
# RSC_ORCH_ONLY="A0_base A4_join GEN_real"：只跑列出的格（空格分隔）。
# 为什么留这个口子：单因素阶梯一次要跑 6 格 × 2 相位，机器不空闲时每格的复现带会被
# 拉长到 5%–9%，正好盖住要判定的差值。砍到 2–3 格就能在同预算下把轮数翻几倍、
# 把带压到 1% 以下 —— 「加轮数压带」是 docs/05 测量纪律里唯一的解法。
LBL=(); BIN=(); NOTE=()
while IFS=$'\t' read -r name note; do
  if [[ -n "${RSC_ORCH_ONLY:-}" ]] && [[ " ${RSC_ORCH_ONLY} " != *" $name "* ]]; then continue; fi
  LBL+=("$name"); BIN+=("$DIR/$name.exe"); NOTE+=("$note")
done <"$DIR/manifest.tsv"
if [[ -z "${RSC_ORCH_ONLY:-}" || " ${RSC_ORCH_ONLY} " == *" GEN_real "* ]]; then
  LBL+=(GEN_real); BIN+=("$DIR/gen.exe"); NOTE+=("真实生成物：rscel build ${ANCHOR_DSL} --release")
fi
[[ ${#LBL[@]} -gt 0 ]] || { echo "RSC_ORCH_ONLY 里没有任何已知的格"; exit 1; }

echo "== 编译所有探针（同一组 flag：${FLAGS[*]}）=="
for ((i = 0; i < ${#LBL[@]}; i++)); do
  if [[ "${LBL[i]}" == GEN_real ]]; then continue; fi
  rustc "${FLAGS[@]}" -o "${BIN[i]}" "${DIR}/${LBL[i]}.rs" 2>"$DIR/${LBL[i]}.build.err" \
    || { echo "编译失败：${LBL[i]}"; cat "$DIR/${LBL[i]}.build.err"; exit 1; }
  if [[ -s "$DIR/${LBL[i]}.build.err" ]]; then
    echo "${LBL[i]} 编译有告警："; cat "$DIR/${LBL[i]}.build.err"
  fi
done

# ---------- 交错采样 + 相位复现带（与门禁台 run.sh **同一套统计量**）----------
#
# 同一批二进制在一个轮转窗里跑**两遍**：第二遍整体偏移半个周期，使每格落在完全不同的
# 邻居之间（run.sh 的「相位复现带」口径）。
#
# 判定统计量 2026-10-11 从"每相位取最小值"换成**中位数**（docs/05 P0-5 第 6 项）：
# 编排场景每份程序都 spawn 8+1 个线程，`min` 量到的是"最走运的那一次全局调度"而不是
# 代码属性 —— 实测某格在 101 轮窗里被一次幸运运行定在 7.42ms（比同窗 `A0_base` 快 43%、
# 自己带 ±78.8%），14 次直采样复验与其余格同带（docs/06 实验三补记）。
# 现在每格、每相位各留**全部样本**：判定耗时 = 两相位中位数的平均，复现带 = 两相位
# 中位数之差 / 该平均。`min` 仍算出来作参考列（旧表全是 min 口径，留着才对得账，
# **不许**拿旧表的 min 读数和新表的中位读数直接比）。
NPROG=${#LBL[@]}
ROT=$(( NPROG / 2 ))
S1=(); S2=(); CHK=()
for ((i = 0; i < NPROG; i++)); do S1[i]=""; S2[i]=""; CHK[i]=""; done

# run_once：跑一次，把 elapsed_ns 打到 stdout，输出留在 $DIR/.out 供校验值提取。
# 为什么要临时文件而不是命令替换：`$(...)` 里再套一层 grep 会把"程序没打 elapsed_ns"
# 和"程序根本跑崩了"两种情况都变成空字符串，报出来的诊断就没法区分它们。
run_once() { # $1=exe
  local v
  timeout 120 "$1" >"$DIR/.out" 2>&1 || true
  v="$(grep '^elapsed_ns=' "$DIR/.out" | head -1)" || v=""
  v="${v#elapsed_ns=}"
  v="${v%$'\r'}"
  if [[ ! "$v" =~ ^[0-9]+$ ]]; then
    echo "拿不到 elapsed_ns（$1），原始输出：$(head -c 400 "$DIR/.out")" >&2
    return 1
  fi
  printf '%s' "$v"
}

echo "== 交错采样 ${REPS} 轮 × 2 个相位（每轮 $(( NPROG * 2 )) 次执行）=="
for ((r = 0; r < REPS; r++)); do
  for ((i = 0; i < NPROG; i++)); do
    v="$(run_once "${BIN[i]}")" || exit 1
    S1[i]="${S1[i]}$v "
    if [[ -z "${CHK[i]}" ]]; then
      CHK[i]="$(grep -E "$CHECK_KEYS" "$DIR/.out" | head -1 || true)" || CHK[i]=""
    fi
  done
  # 第二相位：同一批程序、整体偏移半个周期再轮转一遍，样本记进 S2（下标仍是本格）。
  for ((j = 0; j < NPROG; j++)); do
    i=$(( (j + ROT) % NPROG ))
    v="$(run_once "${BIN[i]}")" || exit 1
    S2[i]="${S2[i]}$v "
  done
done

# ---------- 同构校验 ----------
for ((i = 0; i < NPROG; i++)); do
  if [[ "${CHK[i]}" != "${CHK[0]}" ]]; then
    echo "校验值不一致：A0_base=[${CHK[0]}] vs ${LBL[i]}=[${CHK[i]}] → 各格算的不是同一件事，本次数据作废" >&2
    exit 1
  fi
done

# stats 样本串 -> "中位数 最小值 样本数"（整数 ns），逐字照抄 run.sh 的实现。
stats() {
  printf '%s\n' $1 | sort -n | awk '
    { a[NR] = $1 }
    END {
      if (NR == 0) { print "0 0 0"; exit }
      med = (NR % 2) ? a[(NR + 1) / 2] : int((a[NR / 2] + a[NR / 2 + 1]) / 2)
      printf "%d %d %d\n", med, a[1], NR
    }'
}

# ---------- 合并两个相位：判定耗时 = 两份中位数的平均，复现带 = 两份中位数之差 ----------
# 不再"取两份里更小的那个"—— 那本身就是一次 min，正是本次要换掉的统计量。
TIM=(); BAND=(); MINV=(); MINBAND=(); NSAMP=(); TAIL=(); DRIFT=""
for ((i = 0; i < NPROG; i++)); do
  read -r m1 n1 c1 <<<"$(stats "${S1[i]}")"
  read -r m2 n2 c2 <<<"$(stats "${S2[i]}")"
  if [[ -z "$m1" || -z "$m2" ]] || (( m1 == 0 || m2 == 0 )); then
    echo "拿不到 ${LBL[i]} 的相位复现带（一份样本为空）" >&2; exit 1
  fi
  TIM[i]=$(( (m1 + m2) / 2 ))
  lo="$m1"; hi="$m2"
  if (( m2 < m1 )); then lo="$m2"; hi="$m1"; fi
  BAND[i]=$(( (hi - lo) * 10000 / TIM[i] ))
  MINV[i]="$n1"; lo="$n1"; hi="$n2"
  if (( n2 < n1 )); then lo="$n2"; hi="$n1"; MINV[i]="$n2"; fi
  MINBAND[i]=$(( (hi - lo) * 10000 / lo ))
  NSAMP[i]=$(( c1 + c2 ))
  # 上尾 = 中位数比自己 min 高出的百分比。编排场景本来不看这一列（线程数把分布整个
  # 撑宽），但它是 P0-6 的判据（"生成物的上尾比基线长"），探针台两个量都留着。
  TAIL[i]=$(( (TIM[i] - MINV[i]) * 10000 / MINV[i] ))
  sp="$(awk -v x="${BAND[i]}" 'BEGIN{printf "%.1f", x/100}')"
  if (( $(awk -v s="$sp" 'BEGIN{print (s>15)?1:0}') )); then
    DRIFT="${DRIFT}${LBL[i]} 复现带 ${sp}%；"
  fi
done

ms()  { awk -v v="$1" 'BEGIN{printf "%.2f", v/1e6}'; }
pct() { awk -v v="$1" 'BEGIN{printf "%.2f", v/100}'; }

# 对称口径判定（照 run.sh）：与对照格的差值先跟复现带比，出得了带才叫"快/慢"，
# 出不了带只能说"打平"。带内差值既不许当成绩、也不许当失败。
base="${TIM[0]}"
res0="${BAND[0]}"
verdict_of() { # $1=本格 $2=对照格
  local d r
  d=$(( ($1 - base) * 10000 / base ))
  r="$res0"
  if (( $2 > r )); then r="$2"; fi
  if (( d < 0 )); then d=$(( -d )); fi
  if (( d <= r )); then printf '与 %s 打平（差值在复现带内）' "${LBL[0]}"; return; fi
  if (( $1 > base )); then printf '比 %s 慢（差值出带）' "${LBL[0]}"; else printf '比 %s 快（差值出带）' "${LBL[0]}"; fi
}

report="docs/06-控制变量实验.md"
BEGIN_MARK='<!-- ORCH:AUTO:BEGIN -->'
END_MARK='<!-- ORCH:AUTO:END -->'
touch "$report"

{
  echo "$BEGIN_MARK"
  echo "> 本小节由 \`bash scripts/bench/orch.sh\` 重写（标记之间的内容别手改）；"
  echo "> 方法：同一组 rustc flag、交错采样 ${REPS} 轮、每格再跑一个错开半周期的第二相位，"
  echo "> 每格判定耗时 = **两相位中位数的平均**，**复现带** = 两相位中位数之差 / 该平均；"
  echo "> 各格 \`done=/checksum=\` 逐字硬比对。"
  echo "> 统计量口径与门禁台 \`run.sh\` 一致（2026-10-11 从每相位取最小值换成中位数，"
  echo "> docs/05 P0-5 第 6 项）：编排场景每份程序 spawn 8+1 个线程，\`min\` 会被一次幸运"
  echo "> 调度定住（实测某格 101 轮窗读 7.42ms、比同窗快 43%、自带 ±78.8% 的带）。"
  echo "> \`min\` 仍作参考列，**旧表的 min 读数不能与本表的中位读数直接比**。"
  echo "> 场景：\`${ANCHOR_DSL}\`（编排形状，无可向量化归约，所以这里不看 \`padd\` 只看耗时）。"
  if [[ -n "${RSC_ORCH_ONLY:-}" ]]; then
    echo "> **本表只跑了选中的格**：\`RSC_ORCH_ONLY=${RSC_ORCH_ONLY}\`（其余格的读数见"
    echo "> 下面人工撰写小节里抄录的那几轮）。"
  fi
  echo
  echo "| 格 | 相对哪一格、只动了哪一个因素 | 中位耗时 (ms±复现带) | min 参考 | 上尾(中位/min) | 相对 ${LBL[0]} | 判定（对 ${LBL[0]}，对称口径） |"
  echo "| --- | --- | --- | --- | --- | --- | --- |"
  for ((i = 0; i < NPROG; i++)); do
    rel="$(awk -v n="${TIM[i]}" -v b="$base" 'BEGIN{printf "%.3fx", n/b}')"
    vmd="—"
    if (( i > 0 )); then vmd="$(verdict_of "${TIM[i]}" "${BAND[i]}")"; fi
    echo "| ${LBL[i]} | ${NOTE[i]} | $(ms "${TIM[i]}") ±$(pct "${BAND[i]}")% | $(ms "${MINV[i]}") | $(pct "${TAIL[i]}")% | $rel | $vmd |"
  done
  echo
  if [[ -n "$DRIFT" ]]; then
    echo "⚠ **本窗复现带超过 15% 的格**：${DRIFT}机器不空闲，下表结论按纪律只能作方向参考，须重测。"
    echo
  fi
  echo "$END_MARK"
} >"$DIR/section.md"

python - "$report" "$DIR/section.md" "$BEGIN_MARK" "$END_MARK" <<'PY'
import io, sys
rep, sec_path, begin, end = sys.argv[1:5]
sec = io.open(sec_path, encoding='utf-8', newline='\n').read()
old = io.open(rep, encoding='utf-8', newline='\n').read()
if begin in old and end in old:
    pre, rest = old.split(begin, 1)
    _, post = rest.split(end, 1)
    new = pre + sec + post
else:
    sep = '' if old.endswith('\n\n') else ('\n' if old.endswith('\n') else '\n\n')
    new = old + sep + sec + '\n'
io.open(rep, 'w', encoding='utf-8', newline='\n').write(new)
PY

echo
echo "已把探针结果表写入 $report"
for ((i = 0; i < NPROG; i++)); do
  echo "${LBL[i]}  med_ms=$(ms "${TIM[i]}")  min_ms=$(ms "${MINV[i]}")  带±$(pct "${BAND[i]}")%  样本${NSAMP[i]}"
done
