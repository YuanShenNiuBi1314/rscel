#!/usr/bin/env bash
# 控制变量矩阵实验：把"生成物比手写慢"这件事拆成一次只动一个因素的对照组。
#
# 为什么要这个脚本而不是继续改生成器试：之前我按"假设 → 改 codegen → 测 → 回滚"的
# 顺序做过两次，两次都在**有并发负载的机器**上测出互相矛盾的数（同一对二进制
# 21ms 与 43ms），得出的"根因"其实没有证据。正确顺序是：先把候选因素逐一隔离量出来，
# 再决定往哪个方向投-development。
#
# 表里除了 10 个合成变体，还有五行来自**真实生成物**（`rscel build bench/loop_sum_opaque.rscel
# --release`）：`GEN_real` 原样、`GEN_nort` 只删掉整段内嵌运行时、`GEN_noargv` 在 `GEN_nort`
# 上再删掉 `__rsc_argv` 采集行、`GEN_pad` 把那一行换成纳秒级的占位语句。没有它们，这张表只能
# 回答"哪一格慢"，却不能回答"哪一格解释了生成物的慢"—— 判定必须对着被测对象来读。
#
# 第五行 `GEN_two` 是**另一份 DSL 的生成物**（`bench/loop_sum_two_sites.rscel`：同一条循环、
# 同一个 checksum，但热 Cell 有**两个** plain 调用点）。它回答的是 docs/05 P0-5 第 2 项的问题：
# 展开闸门里「恰好一个调用点」这一条本身值多少 —— 与 `GEN_real`（单调用点、已被 P0-4 展开）
# 对照就是「展开 vs 不展开」的直接差值，不需要任何合成模板。
#
# 为什么反向探针要**从生成物逐字删**而不是另写一份合成变体（2026-10-11, docs/05 P0-6 动作 0 之后）：
# V00–V09 与生成物之间还差着起始值取值顺序（`a`/`i` 两个变量 + 别名拷贝）、hoisted `let`、
# `Some(Ret…)` 包装等好几处，逐字删才是**单因素**。删格用文本锚点，出现次数 ≠ 1 就直接退出
# （本脚本第一版的 `V05_nocopy` 就是"以为动了其实没动"废掉的）。
#
# 三条从实测里学到的规矩（2026-10-10 修脚本时定下，别再退回旧写法）：
#
# 1. **变体文件必须带生成物同款头部**。上一版把 `#![allow(...)]` 和
#    `#[derive(Default)] struct RetSum/RetMain` 做成了变量 `HEADER` 却从未拼进文件
#    （还漏了个没被替换的 `%s`），于是 V01 之后的变体全部编译失败。现在由 `variant()`
#    统一拼：说明注释 → `#![allow]`（inner attribute 必须在所有 item 之前）→ 结构体 → Cell 函数。
# 2. **起始值的不透明度必须全表一致**。上一版 V01 用 `black_box(0)/black_box(1)`，
#    而 V02 之后写成 `cell_sum(0i64, 1i64)` 字面常量 —— 那一格同时动了"调用深度"和
#    "起始值是否是常量"两个因素，差值完全不能读（而"常量 vs 不透明起始"在本场景里
#    实测差 2 倍以上，见 docs/05 P0-4）。现在全表统一：起始值只在**最外层**过一次
#    `black_box`，`limit` 一律在 `cell_sum` 内取，与 `loop_sum_opaque` 的生成物逐字一致。
# 3. **变体必须算出同一个结果**。每个变体都打 `checksum=`，测量阶段逐字比对，
#    不一致直接作废退出 —— 否则"某一格快了一半"可能只是它没做同样的工作。
#
# 用法：
#   bash scripts/bench/ab.sh              # 跑全部变体（要求机器空闲）
#   RSC_AB_REPS=11 bash scripts/bench/ab.sh
#   RSC_AB_ONLY="V00_main V08_runtime_flat" bash scripts/bench/ab.sh   # 只测选中的格
#   RSC_AB_REF=V02_depth2 bash scripts/bench/ab.sh   # 判定列与"相对"列的对照格
#
# 每个变体的**唯一**差异写在它的文件头注释里；判定规则在结果表下面。
# 报告里 `## 结论（人工撰写…）` 之后的内容会被保留，脚本重写不会抹掉分析。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
DIR=target/ab
mkdir -p "$DIR"
REPS="${RSC_AB_REPS:-7}"
# 与 rscel-cli release 模式逐字一致（crates/rscel-cli/src/lib.rs::BuildMode::rustc_args）。
# 合成变体与真实生成物必须在同一组 flag 下比，否则量的就成了 flag 差异。
FLAGS=(--edition=2021 -C opt-level=3 -C codegen-units=1 --crate-type=bin)
ANCHOR_DSL="bench/loop_sum_opaque.rscel"
# 多调用点探针格的来源（docs/05 P0-5 第 2 项）：与 ANCHOR_DSL 同一条循环、同一个校验值，
# 唯一的结构差是热 Cell 的 plain 调用点从 1 个变成 2 个。
TWO_SITE_DSL="bench/loop_sum_two_sites.rscel"

# 上一轮遗留的变体必须清掉：`ls V*.rs` 会把过期变体一起编、一起测。
rm -f "$DIR"/V*.rs "$DIR"/V*.exe "$DIR"/V*.build.err

python - "$DIR" <<'PY'
import io, os, sys
d = sys.argv[1]

# 生成物每个 .rs 的头部（rscel build 的产物就是这么排的）。
ATTR = "#![allow(dead_code, unused_assignments, non_snake_case)]\n"
STRUCTS = "#[derive(Default)]\nstruct RetMain {}\n\n#[derive(Default)]\nstruct RetSum {}\n"

RUNTIME = io.open(os.path.join('crates', 'rscel-gen', 'src', 'runtime.rs'),
                  encoding='utf-8').read()
# 取出真正嵌入生成物的那段（r#" ... "# 之间）。
RUNTIME = RUNTIME.split('r#"', 1)[1].rsplit('"#', 1)[0]

# 循环体：手写基线与生成物逐字同形（除了各变体要隔离的那一个因素）。
LOOP_FRESH = """    loop {
        let next_acc = v_a + v_b;
        let next_i = v_b + 1;
        if !(next_i <= limit) { break; }
        v_a = next_acc;
        v_b = next_i;
    }"""
# 生成器现在实际发的形状：两个 let 声明在循环外、循环内只赋值。
LOOP_HOISTED = """    let mut next_acc;
    let mut next_i;
    loop {
        next_acc = v_a + v_b;
        next_i = v_b + 1;
        if !(next_i <= limit) { break; }
        v_a = next_acc;
        v_b = next_i;
    }"""

CALL = "cell_sum(std::hint::black_box(0i64), std::hint::black_box(1i64))"


def cell_sum(loop_text=LOOP_FRESH, attr='', alias_copy=True, io_outside_timer=False):
    """生成物 `fn cell_sum` 的形状。三个参数各对应矩阵里的一格，别混用。"""
    src = (attr + '\n') if attr else ''
    if alias_copy:
        src += 'fn cell_sum(a: i64, b: i64) -> Option<RetSum> {\n'
        src += '    let mut v_a: i64 = a;\n    let mut v_b: i64 = b;\n'
    else:
        # V05：循环变量就是形参本身（去掉别名拷贝），所以签名要 mut。
        src += 'fn cell_sum(mut v_a: i64, mut v_b: i64) -> Option<RetSum> {\n'
    src += '    let t0 = std::time::Instant::now();\n'
    src += '    let limit = std::hint::black_box(50000000i64);\n'
    src += loop_text + '\n'
    if io_outside_timer:
        # V07：结果用 black_box 消费掉，先把 elapsed 取成一个变量，**再**打 checksum。
        # 为什么不是"干脆删掉 checksum"：那一行是这个脚本判断"各格算的是同一件事"的
        # 唯一凭据（2026-10-10 第一版就是这么写的，被自己的校验硬比对拦下来了）。
        # 把两行 IO 移到计时窗口之外，量的才是"循环期间的 IO 成本"本身。
        src += '    std::hint::black_box(v_a);\n'
        src += '    let __el = t0.elapsed().as_nanos();\n'
        src += '    println!("checksum={}", v_a);\n'
        src += '    println!("elapsed_ns={}", __el);\n'
    else:
        src += '    println!("checksum={}", v_a);\n'
        # elapsed_ns 这一行**所有变体都必须有**：它是测量脚本唯一的取数入口。
        src += '    println!("elapsed_ns={}", t0.elapsed().as_nanos());\n'
    src += '    return Some(RetSum {  });\n}\n'
    return src


MAIN_FLAT = 'fn main() {\n    let _ = ' + CALL + ';\n}\n'
MAIN_DEPTH2 = ('fn cell_main() -> Option<RetMain> {\n'
               '    { let _ = ' + CALL + '; return Some(RetMain {}); }\n'
               '}\n' + MAIN_FLAT)

# V00 的 body 单独抽出来：V08 要拿**同一份文本**只加内嵌运行时。
# 为什么必须有 V08（docs/05 P0-6 动作 1，2026-10-11）：V06 那一格把死代码加在
# **独立 `fn` 持循环**的形状上（20ms 那批格），而 P0-4 的实体展开落地之后，生成物
# 已经是"循环在 main 里"的 6.4ms 形状 —— 固定成本（分页 / 缓存占用）在更短的运行里
# 占比大 3 倍，"死代码收不收费"必须重问在**生成物当前的形状**上，否则 V06 的
# 零结果答的不是 P0-6 问的那个问题。
MAIN_FLAT_BODY = """fn main() {
    let t0 = std::time::Instant::now();
    let mut v_a: i64 = std::hint::black_box(0i64);
    let mut v_b: i64 = std::hint::black_box(1i64);
    let limit = std::hint::black_box(50000000i64);
""" + LOOP_FRESH + """
    println!("checksum={}", v_a);
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}
"""

manifest = []


def variant(name, note, body, with_runtime=False):
    src = '// 变体 %s\n// 唯一差异：%s\n\n' % (name, note)
    src += ATTR
    if with_runtime:
        src += RUNTIME + '\n'
    src += STRUCTS + '\n' + body
    io.open(os.path.join(d, name + '.rs'), 'w', encoding='utf-8',
            newline='\n').write(src)
    # note 同步进 manifest，报告直接读它（GEN_real 没有变体文件，也走同一张表）。
    manifest.append((name, note))


# ---- V00：循环直接写在 main 里（= bench/loop_sum_opaque_hand.rs 的形状，已知最快） ----
variant('V00_main',
        '相对手写基线不变：循环在 fn main 内，无 Cell 函数、无 Option 返回',
        MAIN_FLAT_BODY)

# ---- V01：只加"函数边界 + Option 返回" ----
variant('V01_cellfn',
        '相对 V00 只把循环放进一个返回 Option<RetSum> 的普通函数，由 main 直接调用',
        cell_sum() + MAIN_FLAT)

# ---- V02：只再加一层调用（main→cell_main→cell_sum，与生成物同深度） ----
variant('V02_depth2',
        '相对 V01 只多一层 cell_main 包装（生成物就是 main→cell_main→cell_sum）',
        cell_sum() + MAIN_DEPTH2)

# ---- V03：只再加内联提示（= 已提交的 48cbb79 的做法） ----
variant('V03_inline',
        '相对 V02 只给 cell_sum 与 cell_main 各加 #[inline(always)]',
        cell_sum(attr='#[inline(always)]')
        + '#[inline(always)]\n' + MAIN_DEPTH2)

# ---- V04：只换循环形状：let 提到循环外（生成器实际发的形状） ----
variant('V04_hoisted',
        '相对 V02 只把循环体内那两个 let 移到循环外（生成器实际就是这么发的）',
        cell_sum(loop_text=LOOP_HOISTED) + MAIN_DEPTH2)

# ---- V05：只去掉别名拷贝（检验 S2 调用约定的 v_<id> 拷贝是否有代价） ----
variant('V05_nocopy',
        '相对 V02 只去掉 let mut v_a = a / let mut v_b = b 这两行拷贝，循环直接消耗形参',
        cell_sum(alias_copy=False) + MAIN_DEPTH2)

# ---- V06：只加整个内嵌运行时（同文件死代码/模块规模效应） ----
variant('V06_runtime',
        '相对 V02 只在同一文件里加上生成物携带的整段内嵌运行时（全是死代码）',
        cell_sum() + MAIN_DEPTH2, with_runtime=True)

# ---- V07：只把循环结果的 IO 移出计时窗口（验证计时是否被打印偏袒） ----
variant('V07_noio',
        '相对 V02 只把两行 println 移到 elapsed 取值之后，循环结果改由 black_box 消费'
        '（计时窗口内不再做 IO）',
        cell_sum(io_outside_timer=True) + MAIN_DEPTH2)

# ---- V08：在**展开形状**（V00）上只加内嵌运行时死代码（docs/05 P0-6 动作 1）----
variant('V08_runtime_flat',
        '相对 V00 只在同一文件里加上生成物携带的整段内嵌运行时（全是死代码）。'
        '**与 V06 是同一个因素、不同的形状**：V06 加在"独立 fn 持循环"的旧生成物形状上，'
        '而 P0-4 展开之后生成物是 V00 这种 6.4ms 形状，固定成本的占比大 3 倍，'
        '所以"死代码在典型运行口径下收不收费"必须在这一格重问',
        MAIN_FLAT_BODY, with_runtime=True)

# ---- V09：只把 V08 的那段循环包进 entry Cell 的外壳（P0-6 residual 的下一格）----
# 为什么加这一格：实测 GEN_real 的中位数比 V00/V08 慢约 1%（四窗同号），而 V08 已经
# 证明"整段运行时死代码"不收费 → 剩下的结构差就是生成物把展开体放进
# `#[inline(always)] fn cell_main()` 里的**块表达式**（entry Cell 不参与展开，见
# codegen 的展开闸门"不是 entry"）。这一格只动那层外壳。
MAIN_WRAPPER_BODY = (
    '#[inline(always)]\n'
    'fn cell_main() -> Option<RetMain> {\n'
    '    { let _ = {\n'
    '        let mut v_a: i64 = std::hint::black_box(0i64);\n'
    '        let mut v_b: i64 = std::hint::black_box(1i64);\n'
    '        let t0 = std::time::Instant::now();\n'
    '        let limit = std::hint::black_box(50000000i64);\n'
    + LOOP_FRESH + '\n'
    '        println!("checksum={}", v_a);\n'
    '        println!("elapsed_ns={}", t0.elapsed().as_nanos());\n'
    '        Some(RetSum {  })\n'
    '    }; return Some(RetMain {}); }\n'
    '}\n'
    'fn main() {\n    let _ = cell_main();\n}\n'
)
variant('V09_entrywrapper',
        '相对 V08 只把同一段循环包进 `#[inline(always)] fn cell_main()` 里的块表达式'
        '（= 生成物对 entry Cell 的实际做法：展开的是被调 Cell，外壳还在）',
        MAIN_WRAPPER_BODY, with_runtime=True)

with io.open(os.path.join(d, 'manifest.tsv'), 'w', encoding='utf-8',
             newline='\n') as f:
    for name, note in manifest:
        f.write('%s\t%s\n' % (name, note))
print('已生成 %d 个变体' % len(manifest))
PY

# ---------- 真实生成物（锚点行）----------
if [[ -n "${RSC_BIN:-}" ]]; then
  RSC="$RSC_BIN"
else
  echo "== 构建 rscel CLI =="
  cargo build -q --release -p rscel-cli
  RSC="$ROOT/target/release/rscel.exe"
fi
[[ -x "$RSC" ]] || { echo "rscel 二进制不存在: $RSC"; exit 1; }
echo "== 生成锚点产物：$ANCHOR_DSL =="
"$RSC" build "$ANCHOR_DSL" -o "$DIR/gen" --release >"$DIR/gen.build.log" 2>&1 \
  || { echo "rscel build 失败，日志："; cat "$DIR/gen.build.log"; exit 1; }
echo "== 生成多调用点产物：$TWO_SITE_DSL =="
"$RSC" build "$TWO_SITE_DSL" -o "$DIR/gen_two" --release >"$DIR/gen_two.build.log" 2>&1 \
  || { echo "rscel build $TWO_SITE_DSL 失败，日志："; cat "$DIR/gen_two.build.log"; exit 1; }

# ---------- 反向探针格：从真实生成物**逐字删**因素 ----------
python - "$DIR" <<'PY'
import io, os, sys
d = sys.argv[1]

src = io.open(os.path.join(d, 'gen.rs'), encoding='utf-8').read()
RUNTIME = io.open(os.path.join('crates', 'rscel-gen', 'src', 'runtime.rs'),
                  encoding='utf-8').read()
RUNTIME = RUNTIME.split('r#"', 1)[1].rsplit('"#', 1)[0]


def cut(text, needle, what):
    """按文本锚点删一段；出现次数不是 1 就退出，绝不产出"其实什么都没删"的格子。"""
    n = text.count(needle)
    if n != 1:
        sys.stderr.write('锚点失配：%s 在生成物里出现 %d 次（应为 1 次）\n' % (what, n))
        sys.exit(1)
    return text.replace(needle, '')


nort = cut(src, RUNTIME, '整段内嵌运行时')
ARGV_LINE = '    let __rsc_argv: Vec<String> = std::env::args().skip(1).collect();\n'
noargv = cut(nort, ARGV_LINE, '__rsc_argv 采集行')

# GEN_pad：argv 采集行换成一条**几乎不做事**的语句（同位置、保住 `main` 里的这段代码）。
# 为什么必须有这一格（2026-10-11, docs/05 P0-6 动作 0 之后）：实测 GEN_noargv 比 GEN_nort
# 快 1.67%–3.00%（三窗同号、全部出带），而那行采集落在 `t0` **之前**（`main` 里、计时区之外），
# 它的运行成本按定义进不了 `elapsed_ns`。于是只剩两个候选：① `std::env::args()` 首次调用
# 把堆/页/代码状态推贵了，成本确实漏进计时区；② 删掉一行让展开后的热循环整体**对齐**挪了
# 位置，几个百分点是代码布局运气。这一格用一条 `black_box` 语句占住同一个位置：它必然生成
# 真实的 mov/store（否则又被优化回 GEN_noargv，那一格就"以为动了其实没动"），但工作量为纳秒级。
# 读法：GEN_pad ≈ GEN_nort（慢）→ 布局解释成立，残差不该记在转译头上；
#       GEN_pad ≈ GEN_noargv（快）→ 是 `std::env::args()` 本身。
pad = nort.replace(ARGV_LINE, '    let _pad: i64 = std::hint::black_box(1i64);\n'
                              '    std::hint::black_box(&_pad);\n')
if pad == nort:
    sys.stderr.write('锚点失配：GEN_pad 没能替换 argv 采集行\n')
    sys.exit(1)

for name, body in (('GEN_nort', nort), ('GEN_noargv', noargv), ('GEN_pad', pad)):
    io.open(os.path.join(d, name + '.rs'), 'w', encoding='utf-8',
            newline='\n').write(body)
print('反向探针格已生成: GEN_nort / GEN_noargv / GEN_pad')
PY

# ---------- 登记表：下标平行的数组 ----------
# RSC_AB_ONLY="V00_main V02_depth2 V06_runtime"：只测选中的格（空格分隔）。
# 变体文件**全部**照常生成与编译（矩阵的自洽性要看全表），只有采样按选择过滤，
# 所以"某一格其实什么都没测"这种事不会发生 —— 选中的名字对不上就直接报错退出。
LBL=(); BIN=(); NOTE=()
while IFS=$'\t' read -r name note; do
  if [[ -n "${RSC_AB_ONLY:-}" ]] && [[ " ${RSC_AB_ONLY} " != *" $name "* ]]; then continue; fi
  LBL+=("$name"); BIN+=("$DIR/$name.exe"); NOTE+=("$note")
done <"$DIR/manifest.tsv"
# `rscel build -o <out>` 把可执行文件**原样**写到 `<out>`（不加 .exe，见
# crates/rscel-cli 的行为），Git Bash 直接执行无后缀路径没问题。
if [[ -z "${RSC_AB_ONLY:-}" || " ${RSC_AB_ONLY} " == *" GEN_real "* ]]; then
  LBL+=(GEN_real); BIN+=("$DIR/gen"); NOTE+=("真实生成物：rscel build ${ANCHOR_DSL} --release")
fi
# 多调用点格（docs/05 P0-5 第 2 项）：与 GEN_real **同一个热 Cell**，只多一个 plain 调用点
# → 当前闸门不展开，`fn cell_sum(..)` 留着。它相对 GEN_real 的差值就是「闸门里那条
# 恰好一个调用点」收了多少费；选择规则同 GEN_real。
if [[ -z "${RSC_AB_ONLY:-}" || " ${RSC_AB_ONLY} " == *" GEN_two "* ]]; then
  LBL+=(GEN_two); BIN+=("$DIR/gen_two"); NOTE+=("真实生成物：rscel build ${TWO_SITE_DSL} --release（热 Cell 有两个 plain 调用点 → 不展开）")
fi
# 反向探针格（与 GEN_real 同一份源码，只差被删掉/换掉的那一段；选择规则同 GEN_real）。
for probe in GEN_nort GEN_noargv GEN_pad; do
  if [[ -n "${RSC_AB_ONLY:-}" ]] && [[ " ${RSC_AB_ONLY} " != *" $probe "* ]]; then continue; fi
  case "$probe" in
    GEN_nort)   note="GEN_real 逐字删掉整段内嵌运行时（= 死代码在生成物**自己的形状**上收不收费）" ;;
    GEN_noargv) note="GEN_nort 再删掉 main 里的 __rsc_argv 采集行（= P0-6 最后一处未隔离的结构差）" ;;
    GEN_pad)    note="GEN_nort 把 argv 采集行换成一条纳秒级的 black_box 占位语句（= 分清「env 本身收费」与「代码布局运气」）" ;;
  esac
  LBL+=("$probe"); BIN+=("$DIR/$probe.exe"); NOTE+=("$note")
done
[[ ${#LBL[@]} -gt 0 ]] || { echo "RSC_AB_ONLY 里没有任何已知的格（可用名见 target/ab/manifest.tsv）"; exit 1; }

echo "== 编译所有变体（同一组 flag：${FLAGS[*]}）=="
for ((i = 0; i < ${#LBL[@]}; i++)); do
  if [[ "${LBL[i]}" == GEN_real ]]; then continue; fi   # 已由 rscel build 编好
  rustc "${FLAGS[@]}" -o "${BIN[i]}" "${DIR}/${LBL[i]}.rs" 2>"$DIR/${LBL[i]}.build.err" \
    || { echo "编译失败：${LBL[i]}"; cat "$DIR/${LBL[i]}.build.err"; exit 1; }
  if [[ -s "$DIR/${LBL[i]}.build.err" ]]; then
    echo "${LBL[i]} 编译有告警："; cat "$DIR/${LBL[i]}.build.err"
  fi
done

# ---------- 交错采样：每轮把全部被测程序各跑一次，**两个错相位的相位各留全部样本** ----------
#
# 判定统计量 2026-10-11 从「每格取最小值」换成**中位数**，与门禁台 `run.sh`、探针台
# `orch.sh` 同一套口径（docs/05 P0-6 动作 1）。为什么要换：这一台原来只量"最快的那一次"，
# 而 P0-6 要回答的现象恰恰是**典型运行**——实测生成物的最快运行与同构基线打平、
# 中位数却低 1%–2%（两边上尾长度不同：gen 的中位比自己 min 高 4%–6%，基线只高 2%–3%）。
# 用 min 口径的矩阵永远看不见这一格。
# 现在：判定耗时 = 两相位中位数的平均，复现带 = 两相位中位数之差 / 该平均；
# `min` 与「中位/min」（上尾）都进表，最坏样本仍留着做>15% 的负载检查（那是"机器不空闲"
# 的信号，不是判定量）。
NPROG=${#LBL[@]}
ROT=$(( NPROG / 2 ))
# 每轮把「正序格」和「偏移 ROT 的格」配成一对，所以 ROT 必须是 1..NPROG-1；
# 只有一格时两个相位会撞进同一条样本串，复现带就没法算了 —— 直接报错，不出半张表。
if (( NPROG < 2 )); then
  echo "本次只登记了 $NPROG 格：相位复现带至少需要 2 格（RSC_AB_ONLY 要多选一格当对照）" >&2
  exit 1
fi
S1=(); S2=(); WORST=(); CHK=()
for ((i = 0; i < NPROG; i++)); do S1[i]=""; S2[i]=""; WORST[i]=""; CHK[i]=""; done

echo "== 交错采样 ${REPS} 轮 × 2 个相位（每轮 $(( NPROG * 2 )) 次执行）=="
for ((r = 0; r < REPS; r++)); do
  for ((j = 0; j < NPROG; j++)); do
    for i in "$j" "$(( (j + ROT) % NPROG ))"; do
      out="$(timeout 60 "${BIN[i]}" 2>&1 | tr -d '\r')" || out=""
      line="$(printf '%s\n' "$out" | grep '^elapsed_ns=' | head -1 || true)" || line=""
      v="${line#elapsed_ns=}"
      if [[ ! "$v" =~ ^[0-9]+$ ]]; then
        echo "${LBL[i]} 拿不到 elapsed_ns，原始输出：$(printf '%s' "$out" | head -c 300)" >&2
        exit 1
      fi
      if (( i == j )); then S1[i]="${S1[i]}$v "; else S2[i]="${S2[i]}$v "; fi
      if [[ -z "${WORST[i]}" ]] || (( v > WORST[i] )); then WORST[i]="$v"; fi
      if [[ -z "${CHK[i]}" ]]; then
        CHK[i]="$(printf '%s\n' "$out" | grep '^checksum=' | head -1 || true)" || CHK[i]=""
      fi
    done
  done
done

# ---------- 同构校验：所有格必须算出同一个结果 ----------
for ((i = 1; i < ${#LBL[@]}; i++)); do
  if [[ "${CHK[i]}" != "${CHK[0]}" ]]; then
    echo "校验值不一致：${LBL[0]}=[${CHK[0]}] vs ${LBL[i]}=[${CHK[i]}] → 各格算的不是同一件事，本次数据作废" >&2
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

ms()   { awk -v v="$1" 'BEGIN{printf "%.2f", v/1e6}'; }
pct()  { awk -v v="$1" 'BEGIN{printf "%.2f", v/100}'; }

TIM=(); BAND=(); MINV=(); MINBAND=(); NSAMP=(); TAIL=()
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
  # 上尾 = 判定值（中位）比自己 min 高出的百分比。P0-6 的整条现象就是"gen 的上尾更长"，
  # 所以这一列与判定值同等重要：V06 若与 V02 的**中位**打平而上尾明显更长，
  # 说明内嵌运行时死代码的代价落在典型运行而不是最快运行上。
  TAIL[i]=$(( (TIM[i] - MINV[i]) * 10000 / MINV[i] ))
done

# ---------- 摆动检查：同一变体最坏/最快差 >15% 视为被并发负载污染 ----------
drift=""
for ((i = 0; i < NPROG; i++)); do
  sp="$(awk -v a="${WORST[i]}" -v b="${MINV[i]}" 'BEGIN{printf "%.1f", (a/b-1)*100}')"
  if (( $(awk -v s="$sp" 'BEGIN{print (s>15)?1:0}') )); then
    drift="${drift}${LBL[i]} 最坏/最快 ${sp}%；"
  fi
done
if [[ -n "$drift" ]]; then
  echo "⚠ 摆动超过 15% 的格：${drift} → 按测量纪律该数据需重测（机器不空闲）" >&2
fi

report="docs/06-控制变量实验.md"
# 保留人工撰写的小节：脚本只重写数据表，不抹掉上一轮的分析结论。
keep=""
if [[ -f "$report" ]]; then
  keep="$(awk '/^## 结论（人工撰写/ {p=1} p' "$report")" || true
fi

# 对照格（判定列与"相对"列的基准）：默认取登记表第一格。选格跑矩阵时必须显式给，
# 因为矩阵的语义是"每格相对**上一格**只动一个因素"—— V06 该对 V02 读，不是对 V00 读。
REF="${RSC_AB_REF:-${LBL[0]}}"
refi=""
for ((i = 0; i < NPROG; i++)); do
  if [[ "${LBL[i]}" == "$REF" ]]; then refi="$i"; fi
done
[[ -n "$refi" ]] || { echo "RSC_AB_REF=$REF 不在本次登记表里（可用名见 target/ab/manifest.tsv）"; exit 1; }

# 对称口径判定（照 run.sh）：与对照格的差值先跟复现带比，出得了带才叫"快/慢"，
# 出不了带只能说"打平"。带内差值既不许当成绩、也不许当失败。
base="${TIM[refi]}"
res0="${BAND[refi]}"
verdict_of() { # $1=本格耗时 $2=本格复现带
  local d r
  d=$(( ($1 - base) * 10000 / base ))
  r="$res0"
  if (( $2 > r )); then r="$2"; fi
  if (( d < 0 )); then d=$(( -d )); fi
  if (( d <= r )); then printf '与 %s 打平（差值在复现带内）' "$REF"; return; fi
  if (( $1 > base )); then printf '比 %s 慢（差值出带）' "$REF"; else printf '比 %s 快（差值出带）' "$REF"; fi
}

{
  echo "# 控制变量实验：生成物 vs 手写的时间差究竟来自哪一个因素"
  echo
  echo "> 由 \`bash scripts/bench/ab.sh\` 生成：同一组 rustc flag（\`${FLAGS[*]}\`，与"
  echo "> \`rscel build --release\` 逐字一致）、交错采样 ${REPS} 轮 × 2 个错开半周期的相位，"
  echo "> 每格判定耗时 = **两相位中位数的平均**，复现带 = 两相位中位数之差 / 该平均；"
  echo "> 全表起始值只在**最外层**过一次 \`black_box\`（不透明度逐格一致，见文件头规矩 2），"
  echo "> 各格 \`checksum=\`"
  echo "> 逐字相等（脚本会硬比对，不一致直接退出）。变体源码在 \`target/ab/\`，文件头写明它"
  echo "> 相对上一格只动了哪一个因素。"
  echo ">"
  echo "> **判定统计量 2026-10-11 从最小值换成中位数**（docs/05 P0-6 动作 1，与门禁台"
  echo "> \`run.sh\`、编排探针台 \`orch.sh\` 同一套口径）。换的理由不是"
  echo "> 旧口径算错，而是它**看不见 P0-6 那一格**：实测生成物的最快运行与同构基线打平、"
  echo "> 典型运行却差 1%–2%（gen 的中位比自己 min 高 4%–6%，基线只高 2%–3%）。"
  echo "> 「最坏/最快」两列仍保留 —— 那是判断机器忙不忙的负载信号，不是判定量。"
  echo "> 表里的 \`上尾\` = 判定值比自己 \`min\` 高出的百分比，本批就是为它加的。"
  if [[ -n "${RSC_AB_ONLY:-}" ]]; then
    echo ">"
    echo "> **本表只测了选中的格**：\`RSC_AB_ONLY=${RSC_AB_ONLY}\`，对照格"
    echo "> \`RSC_AB_REF=${REF}\`（矩阵其余格的旧读数见下面人工撰写的小节）。"
  fi
  echo
  echo "| 格 | 相对上一格唯一的差异 | 中位耗时 (ms±复现带) | min 参考 | 上尾(中位/min) | 相对 ${REF} | 判定（对 ${REF}，对称口径） | 最坏/最快 |"
  echo "| --- | --- | --- | --- | --- | --- | --- | --- |"
  for ((i = 0; i < NPROG; i++)); do
    rel="$(awk -v n="${TIM[i]}" -v b="$base" 'BEGIN{printf "%.2fx", n/b}')"
    sp="$(awk -v a="${WORST[i]}" -v b="${MINV[i]}" 'BEGIN{printf "%.1f%%", (a/b-1)*100}')"
    vmd="—（对照格）"
    if (( i != refi )); then vmd="$(verdict_of "${TIM[i]}" "${BAND[i]}")"; fi
    echo "| ${LBL[i]} | ${NOTE[i]} | $(ms "${TIM[i]}") ±$(pct "${BAND[i]}")% | $(ms "${MINV[i]}") | $(pct "${TAIL[i]}")% | $rel | $vmd | $sp |"
  done
  echo
  if [[ -n "$drift" ]]; then
    echo "⚠ **本轮最坏/最快摆动超 15% 的格**：${drift}机器不空闲，下表结论按纪律只能作方向参考，须重测。"
    echo
  fi
  echo "## 判定表（读出结论的规则）"
  echo
  echo "- 读数规则（2026-10-11 起）：先看**判定列**——差值出得了复现带才叫"
  echo "  「慢/快」，出不了带只能说打平；再看**上尾列**，它回答的是"
  echo "  「差在典型运行还是最快运行」（docs/05 P0-6 的现象就是上尾长度差）。"
  echo "- **V01 明显慢于 V00** → 差距出在“把循环放进一个函数”这一步（函数边界 + 返回"
  echo "  \`Option<RetSum>\`），与 DSL 无关；对策见 docs/05 候选清单 1（热 Cell 真实体展开）"
  echo "  或 2（循环分块 + 多累加器），或按 §17.2 把这类纯算术热循环划出适用边界。"
  echo "- **V03 < V02** → 内联提示有效，48cbb79 的判断成立；若 V03 仍明显慢于 V00，说明"
  echo "  提示没被兑现（\`codegen-units=1\` 下仍不向量化），需要源码级展开而不是属性。"
  echo "- **V02 ≈ V01** → 多一层 \`cell_main\` 包装不要钱，生成物“先展开再内联”的深度不是因素。"
  echo "- **V04 ≈ V02** → 先前“let 外提阻碍向量化”的假设确实是错的（已回滚，正确）。"
  echo "- **V06 ≫ V02** → 差距来自同文件的运行时死代码改变了内联/向量化决策；对策是候选 3"
  echo "  （把内嵌 runtime 关进 \`mod\` 或加 \`#[inline(never)]\`），而不是动 Cell 形状。"
  echo "  ⚠ 这一格在 min 口径下判过「不收费」；**P0-6 动作 1 要求在中位数口径下重判**，"
  echo "  因为“整文件多一段死代码”的代价可能只落在典型运行（镜像变大→分页/缓存行为），"
  echo "  min 恰好看不见。裁决与数据写在该小节下面的「结论（人工撰写）」里。"
  echo "- **V08 明显慢于 V00** → 同一个因素（内嵌运行时死代码）落在**生成物现在的形状**"
  echo "  （P0-4 展开后的 6.4ms 形状）上收费；这一格才是 docs/05 P0-6 问的那个问题，"
  echo "  V06 的零结果只覆盖了展开前的旧形状。对策仍是候选 3（runtime 关进 \`mod\` /"
  echo "  \`#[inline(never)]\`）。"
  echo "- **V05 ≈ V02** → \`v_<id>\` 别名拷贝不是成本来源，S2 调用约定可认为实测零开销"
  echo "  （这句该写进 README 的性能段，才是目标 1 的正当证据）。"
  echo "- **V07 ≈ V02** → 循环结果那行 \`println!\` 的 IO 不参与差距，计时方式没有偏袒任何"
  echo "  一边。（V07 并没有删掉 checksum，只是把它移到 elapsed 取值之后，这样它仍可用于"
  echo "  全表同构校验。）"
  echo "- **V09 明显慢于 V08** → 剩下那 ~1% 的 residual 来自 entry Cell 的外壳"
  echo "  （\`#[inline(always)] fn cell_main()\` + 块表达式）。对策是把 entry 也纳入展开"
  echo "  或直接发 \`fn main\` 体，但那会动到 §12.9 映射与「entry 不是被调 Cell」的形状，"
  echo "  必须先有数据。"
  echo "- **GEN_real 对上哪一格** → 那一格就是生成物慢的原因所在；若 GEN_real ≈ V04+V06 的"
  echo "  组合效果，说明矩阵模型自洽，可以放心按上表挑对策。"
  echo "- **GEN_two 明显慢于 GEN_real** → 展开闸门的「恰好一个 plain 调用点」这一条在收费："
  echo "  两格是**同一个热 Cell、同一份工作量**，唯一区别是第二个调用点让 Cell 退回"
  echo "  \`fn cell_x(..)\` 形状（docs/05 P0-5 第 2 项）。对策是把展开覆盖面放到"
  echo "  「全部 plain 调用点」，不是再找新因素。"
  echo "- **GEN_two ≈ GEN_real** → 多调用点不额外收费，P0-5 第 2 项被数据剔除，"
  echo "  不许为了「覆盖面整齐」去扩展开（体积翻倍换不到东西）。"
} >"$report"

if [[ -n "$keep" ]]; then
  {
    echo
    printf '%s\n' "$keep"
  } >>"$report"
fi

echo
echo "结果表已写入 $report"
for ((i = 0; i < NPROG; i++)); do
  echo "${LBL[i]}  med_ms=$(ms "${TIM[i]}")  min_ms=$(ms "${MINV[i]}")  带±$(pct "${BAND[i]}")%  上尾$(pct "${TAIL[i]}")%  样本${NSAMP[i]}"
done
