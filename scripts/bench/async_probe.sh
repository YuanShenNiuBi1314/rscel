#!/usr/bin/env bash
# async Cell 判定台（docs/05 P0-5 第 3 项）：async 形状里到底哪一层在收费？
#
# 为什么需要这张表而不是直接动手实现「async Cell 实体展开」：
# 第 3 项登记时写的前提是「真要做需要 `async move` 块，必须先证明它比 `async fn` 快」。
# 而"比 async fn 快"这句话里藏着两个完全不同的因素，必须分开量：
#   · **Cell 自己的 async fn 边界**（生成器能改的那一半）—— 在本场景里它只被进入**一次**
#     （`jump loop` 是原地循环，循环体在 Cell 内部），所以展开它的理论收益 = 一次调用的成本；
#   · **每轮迭代那次 `.await`**（来自用户 `extern rust` 的 async fn，生成器不该动它）。
# 不分开就会犯 docs/05 立过的那类错：把用户代码的成本算成转译的损失。
#
# 因此阶梯是（每格相对前一格的**唯一**差异写在 manifest 的 note 里）：
#   GEN_real     真实生成物（对照格）
#   GEN_always   只给 `async fn cell_sum` 加 `#[inline(always)]` —— 拿掉 Cell 自己的边界，
#                其余逐字不动。这是「边界值多少钱」的最小探针，也是实现代价最小的候选
#                （codegen 现在对 async Cell 一行内联提示都不加）。
#   GEN_block    把 Cell body 逐字展开进调用点的 `__rsc_block_on(async move { .. })`
#                —— 即第 3 项真正要实现的形状。它与 GEN_always 消掉的是同一个边界，
#                所以只有 GEN_always 出了收益才谈得上比较两者。
#   GEN_floor    只把每轮那次 `rsc_step(..).await` 换成**同 body 的同步 fn** ——
#                因素是"每轮一个 future"，与 Cell 边界无关。
#   GEN_noextern 每轮那次 extern 调用整个不要（**不是单因素**，只作下限参考：
#                它与 loop_sum_opaque 的差别只剩 `black_box` 的位置）。
#
# 下面两格是 docs/05 P0-8（`GEN_floor` 的符号翻转）的单因素复验补进来的。为什么要成对加：
# 翻转的事实是「同步版在旧运行时上快 9%–11%，在栈上 pin 之后反而慢 6%–9%」，而登记的
# 机制猜想是"同步版留成一次真调用、async 版的状态机被折进了 poll 循环"。可是
# `GEN_floor` 相对 `GEN_real` 同时动了**两个**因素（future ↔ 调用、以及两者各自的
# 可内联性），单加一格 `GEN_floor_inl` 只能回答"同步侧的提示值多少钱"，读出来是打平还是
# 更快都无法归因 —— 因为 async 侧的 `rsc_step` 同样一行提示都没有。所以补成
# **2×2**（每格相对某条边只动一个因素）：
#   GEN_extinl   相对 `GEN_real` 只给 `extern rust` 里那个每轮被 await 的
#                `async fn rsc_step` 加 `#[inline(always)]`（`GEN_always` 加的是 **Cell**
#                的提示、不是这个，所以这一格此前从未被测过）。
#   GEN_floor_inl 相对 `GEN_floor` 只给 `fn rsc_step_sync` 加同一个提示。
# 于是四条边各自单因素：
#   GEN_real → GEN_extinl   = async 侧的可内联性
#   GEN_floor → GEN_floor_inl = 同步侧的可内联性
#   GEN_real → GEN_floor     = 每轮一个 future（旧口径，含混淆）
#   GEN_extinl → GEN_floor_inl = 两侧都有提示时，"每轮一个 future" 的**干净**读数
# 这两格是本批实测后**补进来**的（冒烟窗读到 GEN_noextern 仍要 25ms，而同步展开版同一
# 条循环只要 6.5ms —— 说明收费的地方不在 Cell 边界，而在"循环活在一个 boxed future 的
# poll 函数里"这件事本身，而那属于内嵌运行时，是生成器唯一真正能动的地方）：
#   GEN_alwaysbo 只把 `__rsc_block_on` 的 `#[inline(never)]` 换成 `#[inline(always)]`
#                （因素：poll 循环能不能被内联进调用方作用域 —— 与 docs/06 判定表里
#                 V01「循环待在独立 fn」那一格同型）。
#   GEN_nobox    只把 `let mut fut = Box::pin(fut);` 换成栈上 `std::pin::pin!`
#                （因素：future 状态住堆还是住栈）。**方向会自动翻转**：若运行时已经
#                 落地成栈上 pin，这一格变成 `GEN_box` —— 把 pin 换回 `Box::pin`，
#                 这样落地之后仍能在同一窗复现该因素值多少。
#
# 三条老规矩（照 ab.sh / orch.sh，别再退回旧写法）：
# 1. 变体一律从**真实生成物**按文本锚点派生，锚点出现次数 ≠ 1 直接退出 ——
#    「以为动了其实没动」的格子会读出一个根本不存在的因素成本。
# 2. 全表同一组 rustc flag，且与 rscel-cli release 模式逐字一致。
# 3. 各格必须算出**同一个校验值**（checksum=1249999975000000，与 loop_sum_opaque 同值），
#    测量阶段逐字硬比对，不一致本次作废。
#
# 统计量与门禁台 `run.sh`、编排台 `orch.sh` 同一套：交错采样 × 两相位，判定值 = 两相位
# 中位数的平均，复现带 = 两相位中位数之差 / 该平均，`min` 只作参考列。
#
# 用法：
#   bash scripts/bench/async_probe.sh
#   RSC_ASYNC_REPS=41 bash scripts/bench/async_probe.sh        # 压带
#   RSC_ASYNC_ONLY="GEN_real GEN_always" bash scripts/bench/async_probe.sh
#   RSC_BIN=target/reldata2/release/rscel.exe bash scripts/bench/async_probe.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
DIR=target/ab/async
mkdir -p "$DIR"
REPS="${RSC_ASYNC_REPS:-21}"
ANCHOR_DSL="bench/async_step.rscel"
FLAGS=(--edition=2021 -C opt-level=3 -C codegen-units=1 --crate-type=bin)
CHECK_KEYS='^checksum='

rm -f "$DIR"/GEN_*.rs "$DIR"/GEN_*.exe "$DIR"/GEN_*.build.err "$DIR"/HAND_iso.rs \
      "$DIR"/HAND_iso.exe "$DIR"/gen.rs "$DIR"/gen.exe

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
[[ -f "$DIR/gen.rs" ]] || { echo "找不到生成物源码 $DIR/gen.rs"; exit 1; }

python - "$DIR" <<'PY'
import io, os, re, sys

d = sys.argv[1]
manifest = []


def read(p):
    return io.open(p, encoding='utf-8', newline='\n').read()


def write(name, src, note):
    io.open(os.path.join(d, name + '.rs'), 'w', encoding='utf-8', newline='\n').write(src)
    manifest.append((name, note))


def sub1(text, old, new, where):
    n = text.count(old)
    if n != 1:
        sys.exit('%s：锚点出现 %d 次（应为 1）→ 生成物形状已变，请把本脚本的锚点同步更新，'
                 '不要静默跳过这一格' % (where, n))
    out = text.replace(old, new)
    if out == text:
        sys.exit('%s：替换后文本没变化，这一格是空操作' % where)
    return out


GEN = read(os.path.join(d, 'gen.rs'))

# ---------- 分母防过期核对（docs/05 P0-6 动作 0 那一类错误的解药）----------
# 为什么要这一条：`bench/async_step_fn_hand.rs` 是**比值**的分母，而它与生成物之间的关系
# 只有一句"执行器同构"。本仓库已经两次栽在"分母自己还在用旧形状"上（实验六把
# `loop_sum_opaque_fn_hand` 逐处重写、实验三把编排分母的 supervisor 交接改对）。
# 这里不靠人记得改：把两边的 `block_on` 函数体抠出来，去掉注释与空白、去掉 `__rsc_` 前缀，
# 要求**逐字相等**；生成器改了执行器而分母没跟上 → 当场退出，而不是读出一个假比值。
def extract_fn(text, header):
    i = text.find(header)
    if i < 0:
        return None
    j = text.find('\n}\n', i)
    if j < 0:
        return None
    return text[i:j]


def norm(s):
    keep = [l.strip() for l in s.split('\n') if l.strip() and not l.strip().startswith('//')]
    return ' '.join(' '.join(keep).replace('__rsc_', '').split())


g_bo = extract_fn(GEN, 'fn __rsc_block_on<')
h_bo = extract_fn(read(os.path.join('bench', 'async_step_fn_hand.rs')), 'fn block_on<')
if g_bo is None or h_bo is None:
    sys.exit('分母核对失败：生成物或 `bench/async_step_fn_hand.rs` 里抠不出 block_on 的定义')
if norm(g_bo) != norm(h_bo):
    sys.exit('分母**已与生成物不同构**：`bench/async_step_fn_hand.rs` 的执行器和嵌入运行时不一致。\n'
             '生成物: %s\n分母  : %s\n'
             '→ 先把分母跟到当前形状（照 docs/06 实验六的做法），再谈比值；否则这一台量出来的'
             '比值量的是"执行器差异"而不是"转译差异"。' % (norm(g_bo)[:200], norm(h_bo)[:200]))
print('分母核对通过：两边 block_on 去注释/去前缀后逐字相等')

# --- 锚点：Cell 函数头、调用点那一行、每轮的 await 行、extern 里的 async fn ---
CELL_HEAD = 'async fn cell_sum(a: i64, i: i64) -> Option<RetSum> {'
CALL_SITE = ('    { let _ = __rsc_block_on(cell_sum(std::hint::black_box(0i64), '
             'std::hint::black_box(1i64))); return Some(RetMain {}); }')
AWAIT = '        next_acc = rsc_step(v_a + v_b).await;'
EXTERN = '    async fn rsc_step(x: i64) -> i64 { std::hint::black_box(x) }'
BO_SIG = ('#[inline(never)]\nfn __rsc_block_on<F: std::future::Future<Output = T>, T>(fut: F) -> T {')
BO_BOX = '    let mut fut = Box::pin(fut);'
BO_PIN = '    let mut fut = std::pin::pin!(fut);'
for anchor, where in ((CELL_HEAD, 'Cell 函数头'), (CALL_SITE, '调用点'),
                      (AWAIT, '每轮 await 行'), (EXTERN, 'extern 里的 async fn'),
                      (BO_SIG, '__rsc_block_on 的签名')):
    if GEN.count(anchor) != 1:
        sys.exit('生成物里%s的锚点不是唯一一份（出现 %d 次）：%s\n'
                 '→ 生成器形状变了，本表的单因素前提不再成立，先同步脚本再测。'
                 % (where, GEN.count(anchor), anchor))

# --- GEN_always：只加内联提示 ---
write('GEN_always',
      sub1(GEN, CELL_HEAD, '#[inline(always)]\n' + CELL_HEAD, 'GEN_always'),
      '相对 `GEN_real` 只给 `async fn cell_sum` 加 `#[inline(always)]`（拿掉 Cell 自己的边界；'
      'async Cell 目前在 codegen 里一行内联提示都没有）')

# --- GEN_alwaysbo：只把执行器从 inline(never) 改成 inline(always) ---
write('GEN_alwaysbo',
      sub1(GEN, BO_SIG, '#[inline(always)]\n' + BO_SIG[len('#[inline(never)]\n'):],
           'GEN_alwaysbo'),
      '相对 `GEN_real` 只把 `__rsc_block_on` 的 `#[inline(never)]` 换成 `#[inline(always)]`'
      '（因素：驱动 future 的那个 poll 循环能不能进调用方作用域 —— 与判定表 V01'
      '「循环待在独立 fn」那一格同型，但这次边界是运行时带来的）')

# --- GEN_extinl：只给每轮那个 extern async fn 加内联提示 ---
# 与 GEN_always 的区别要写清楚，否则下一批会以为这是同一格的重复：GEN_always 加提示的对象
# 是**生成的 Cell 函数** `async fn cell_sum`（整个运行只进入一次，实测打平），这一格加的对象
# 是**用户 `extern rust` 里每轮都被 await 的 `async fn rsc_step`**（每轮都过）。
write('GEN_extinl',
      sub1(GEN, EXTERN, '    #[inline(always)]\n' + EXTERN, 'GEN_extinl'),
      '相对 `GEN_real` 只给 `extern rust` 里那个每轮被 await 的 `async fn rsc_step` 加 '
      '`#[inline(always)]`（因素：**async 侧那次调用的可内联性**；与 `GEN_floor_inl` 配成 '
      '2×2 的两条竖边，见脚本头部）')

# --- GEN_nobox / GEN_box：堆装箱与栈上 pin 之间来回换 ---
# 为什么两个方向都要有：本台跑完后这条因素**可能已经落地**（`__rsc_block_on` 改用栈上
# `pin!`）。那时"删掉堆装箱"会退化成空操作，而判定一个因素值多少钱的正确做法是
# **在落地之后把它换回去再量一次**（同一窗、同一组 flag、同一校验值）。所以按生成物
# 当前实际形状选方向，两个方向都只动这一行。
#
# 为什么栈上写法是 `let mut fut = std::pin::pin!(fut);` 而不是裸一句 `std::pin::pin!(fut);`：
# 后者作为语句展开出的内部绑定**不会**遮蔽参数 `fut`，于是下面的 `fut.as_mut()` 直接
# E0599（实测：报的是 "no method named `as_mut` found for type parameter `F`"）。
if GEN.count(BO_BOX) == 1 and GEN.count(BO_PIN) == 0:
    write('GEN_nobox', sub1(GEN, BO_BOX, BO_PIN, 'GEN_nobox'),
          '相对 `GEN_real` 只把 `let mut fut = Box::pin(fut);` 换成栈上 `std::pin::pin!`'
          '（因素：future 的状态住堆还是住栈）')
elif GEN.count(BO_PIN) == 1 and GEN.count(BO_BOX) == 0:
    write('GEN_box', sub1(GEN, BO_PIN, BO_BOX, 'GEN_box'),
          '相对 `GEN_real` 只把栈上 `std::pin::pin!` 换回 `Box::pin(fut)`'
          '（**反方向复测**：运行时已落地栈上 pin，这一格把该因素换回去量它值多少）')
else:
    sys.exit('__rsc_block_on 的装箱行既不是 Box::pin 也不是 pin!（或两者都有）→ '
             '执行器形状变了，本台的这一格需要同步更新')

# --- GEN_block：body 逐字展开进调用点的 __rsc_block_on(async move { .. }) ---
brace = GEN.index('{', GEN.index(CELL_HEAD))
depth = 0
for k in range(brace, len(GEN)):
    if GEN[k] == '{':
        depth += 1
    elif GEN[k] == '}':
        depth -= 1
        if depth == 0:
            end = k + 1
            break
else:
    sys.exit('找不到 cell_sum 的闭括号')
BODY = GEN[brace + 1:end - 1]
CELL_FN = GEN[GEN.index(CELL_HEAD):end] + '\n'
if GEN.count(CELL_FN) != 1:
    sys.exit('GEN_block：删不出整个 `async fn cell_sum` 定义（锚点失配）')
# 展开体 = 形参按位置绑定 + 原 body 逐字（与同步展开 emit_inlined_block 同形），外面套
# `async move { .. }` 再由**同一个** __rsc_block_on 驱动 —— 这就是第 3 项要实现的形状。
#
# 为什么要把收尾那行 `return Some(RetSum {  });` 换成块尾表达式 `Some(RetSum {  })`：
# `async move { .. }` 的输出类型由块尾值决定，而 `return x;` 是语句（块尾仍是 `()`）→
# 直接抄会得到 E0308。生成器真要落地时也必然走这条（同步展开发的就是 `{ let _ = {..};
# return Some(..); }` 里那个"块表达式"形状）。除此之外 body 一行未改。
if BODY.count('return') != 1 or not BODY.rstrip().endswith('return Some(RetSum {  });'):
    sys.exit('GEN_block：body 里的 `return` 不止一处或收尾形状变了，'
             '块尾表达式的改法不再等价，先同步脚本')
BODY_TAIL = BODY.rstrip('\n')[:-len('return Some(RetSum {  });')] + 'Some(RetSum {  })\n'
expanded = ('    { let a: i64 = std::hint::black_box(0i64); let i: i64 = std::hint::black_box(1i64);\n'
            '      let _ = __rsc_block_on(async move {' + BODY_TAIL + '      });\n'
            '      return Some(RetMain {}); }')
out = sub1(GEN, CALL_SITE, expanded, 'GEN_block 调用点')
out = out.replace(CELL_FN, '', 1)
if 'async fn cell_sum' in out:
    sys.exit('GEN_block：独立函数没删干净')
write('GEN_block', out,
      '相对 `GEN_real` 把 Cell body 逐字展开进调用点的 `__rsc_block_on(async move { .. })` '
      '并删掉 `async fn cell_sum` —— 即 P0-5 第 3 项要实现的形状（与 GEN_always 消掉同一个边界）')

# --- GEN_floor：每轮的 future 换成同 body 的同步 fn ---
SYNC_EXTERN = '    fn rsc_step_sync(x: i64) -> i64 { std::hint::black_box(x) }'
out = sub1(GEN, EXTERN, SYNC_EXTERN, 'GEN_floor extern')
out = sub1(out, AWAIT, '        next_acc = rsc_step_sync(v_a + v_b);', 'GEN_floor await 行')
write('GEN_floor', out,
      '相对 `GEN_real` 只把每轮那次 `rsc_step(..).await` 换成**同 body 的同步 fn** —— '
      '因素是「每轮一个 future」，与 Cell 边界无关')

# --- GEN_floor_inl：同步版 + 内联提示（P0-8 的那一条边）---
# 为什么必须与 GEN_extinl 成对读：这一格相对 GEN_floor 只动了「同步侧的可内联性」，
# 若它读出"加提示就把翻转抹平了"，说明 GEN_floor 那 6%–9% 的代价属于**函数调用本身**
# 而不是"每轮一个 future"；那时干净的"每轮一个 future"读数只能取
# GEN_extinl → GEN_floor_inl 这条边（两侧都有提示）。反过来，若它与 GEN_floor 打平，
# 说明提示在源码层根本不起作用（LLVM 早晚会内联它），翻转的机制猜想就此被数据驳回。
out = sub1(GEN, EXTERN, '    #[inline(always)]\n' + SYNC_EXTERN, 'GEN_floor_inl extern')
out = sub1(out, AWAIT, '        next_acc = rsc_step_sync(v_a + v_b);', 'GEN_floor_inl await 行')
write('GEN_floor_inl', out,
      '相对 `GEN_floor` 只给 `fn rsc_step_sync` 加 `#[inline(always)]`'
      '（因素：**每轮那次调用的可内联性**；P0-8 符号翻转的单因素复验）')

# --- GEN_noextern：每轮那次调用整个不要（下限参考，非单因素）---
write('GEN_noextern', sub1(GEN, AWAIT, '        next_acc = v_a + v_b;', 'GEN_noextern'),
      '每轮那次 extern 调用整个不要（**不是单因素**：同时去掉了调用与 future，'
      '只作"这一格最快能到哪"的下限参考）')

with io.open(os.path.join(d, 'manifest.tsv'), 'w', encoding='utf-8', newline='\n') as f:
    for name, note in manifest:
        f.write('%s\t%s\n' % (name, note))
print('已生成 %d 个变体' % len(manifest))
PY

# ---------- 登记表：GEN_real 永远排在第 0 格（对照格 / 分母，不参与 RSC_ASYNC_ONLY 过滤）----------
LBL=(GEN_real); BIN=("$DIR/gen.exe")
NOTE=("真实生成物：\`rscel build bench/async_step.rscel --release\`（\`async fn cell_sum\` 无内联提示）")
while IFS=$'\t' read -r name note; do
  if [[ -n "${RSC_ASYNC_ONLY:-}" ]] && [[ " ${RSC_ASYNC_ONLY} " != *" $name "* ]]; then continue; fi
  LBL+=("$name"); BIN+=("$DIR/$name.exe"); NOTE+=("$note")
done <"$DIR/manifest.tsv"

# ---------- 同构手写分母（比值那一列的落点）----------
# 为什么要它在**同一窗**里：判定列是"相对 GEN_real"，而对外要说的话是"async 程序转译到
# 了同构手写的百分之几"。分母不同窗就没法对账（docs/05 测量纪律：绝对 ms 不跨台引用，
# 跨窗同样不许）。这一格不是锚点派生的，是 `bench/async_step_fn_hand.rs` 那份文件本身。
HAND_RS="bench/async_step_fn_hand.rs"
if [[ -f "$HAND_RS" ]] &&
   { [[ -z "${RSC_ASYNC_ONLY:-}" ]] || [[ " ${RSC_ASYNC_ONLY} " == *" HAND_iso "* ]]; }; then
  cp "$HAND_RS" "$DIR/HAND_iso.rs"
  LBL+=(HAND_iso); BIN+=("$DIR/HAND_iso.exe")
  NOTE+=("**同构手写分母** \`bench/async_step_fn_hand.rs\`：逐处照抄生成物结构（含同构的栈上 \`pin!\` 执行器），只留两处已实测免费的有意差异 → 这一格的「相对 GEN_real」就是 async 形状的转译比值")
fi
[[ ${#LBL[@]} -gt 1 ]] || { echo "RSC_ASYNC_ONLY 里没有任何变体格（GEN_real 是分母，不能单独跑）"; exit 1; }

echo "== 编译所有变体（同一组 flag：${FLAGS[*]}）=="
for ((i = 1; i < ${#LBL[@]}; i++)); do
  rustc "${FLAGS[@]}" -o "${BIN[i]}" "${DIR}/${LBL[i]}.rs" 2>"$DIR/${LBL[i]}.build.err" \
    || { echo "编译失败：${LBL[i]}"; cat "$DIR/${LBL[i]}.build.err"; exit 1; }
  if [[ -s "$DIR/${LBL[i]}.build.err" ]]; then
    echo "${LBL[i]} 编译有告警："; cat "$DIR/${LBL[i]}.build.err"
  fi
done

# ---------- 交错采样 + 相位复现带（与 run.sh / orch.sh 同一套统计量）----------
NPROG=${#LBL[@]}
ROT=$(( NPROG / 2 ))
S1=(); S2=(); CHK=()
for ((i = 0; i < NPROG; i++)); do S1[i]=""; S2[i]=""; CHK[i]=""; done

run_once() { # $1=exe
  local v
  timeout 300 "$1" >"$DIR/.out" 2>&1 || true
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
  for ((j = 0; j < NPROG; j++)); do
    i=$(( (j + ROT) % NPROG ))
    v="$(run_once "${BIN[i]}")" || exit 1
    S2[i]="${S2[i]}$v "
  done
done

for ((i = 0; i < NPROG; i++)); do
  if [[ "${CHK[i]}" != "${CHK[0]}" ]]; then
    echo "校验值不一致：GEN_real=[${CHK[0]}] vs ${LBL[i]}=[${CHK[i]}] → 各格算的不是同一件事，本次数据作废" >&2
    exit 1
  fi
done

stats() {
  printf '%s\n' $1 | sort -n | awk '
    { a[NR] = $1 }
    END {
      if (NR == 0) { print "0 0 0"; exit }
      med = (NR % 2) ? a[(NR + 1) / 2] : int((a[NR / 2] + a[NR / 2 + 1]) / 2)
      printf "%d %d %d\n", med, a[1], NR
    }'
}

TIM=(); BAND=(); MINV=(); NSAMP=(); TAIL=(); DRIFT=""
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
  lo="$n1"; hi="$n2"; MINV[i]="$n1"
  if (( n2 < n1 )); then lo="$n2"; hi="$n1"; MINV[i]="$n2"; fi
  NSAMP[i]=$(( c1 + c2 ))
  TAIL[i]=$(( (TIM[i] - MINV[i]) * 10000 / MINV[i] ))
  sp="$(awk -v x="${BAND[i]}" 'BEGIN{printf "%.1f", x/100}')"
  if (( $(awk -v s="$sp" 'BEGIN{print (s>15)?1:0}') )); then
    DRIFT="${DRIFT}${LBL[i]} 复现带 ${sp}%；"
  fi
done

ms()  { awk -v v="$1" 'BEGIN{printf "%.2f", v/1e6}'; }
pct() { awk -v v="$1" 'BEGIN{printf "%.2f", v/100}'; }

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
BEGIN_MARK='<!-- ASYNC:AUTO:BEGIN -->'
END_MARK='<!-- ASYNC:AUTO:END -->'
touch "$report"

{
  echo "$BEGIN_MARK"
  echo "> 本小节由 \`bash scripts/bench/async_probe.sh\` 重写（标记之间的内容别手改）。"
  echo "> 方法：与门禁台同一套（同一组 rustc flag、交错采样 ${REPS} 轮 × 2 相位，判定值 = 两相位"
  echo "> 中位数的平均，复现带 = 两相位中位数之差 / 该平均；各格 \`checksum=\` 逐字硬比对）。"
  echo "> 场景：\`${ANCHOR_DSL}\` —— 与 \`bench/loop_sum_opaque.rscel\` **同一条递推、同一个校验值**，"
  echo "> 唯一结构差是热 Cell 的透传 Rust 含 \`.await\`（因此它是 \`async fn\`，且被展开闸门硬排除）。"
  if [[ -n "${RSC_ASYNC_ONLY:-}" ]]; then
    echo "> **本表只跑了选中的格**：\`RSC_ASYNC_ONLY=${RSC_ASYNC_ONLY}\`。"
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
    echo "⚠ **本窗复现带超过 15% 的格**：${DRIFT}机器不空闲，本表结论按纪律只能作方向参考，须重测。"
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
echo "已把变体结果表写入 $report"
for ((i = 0; i < NPROG; i++)); do
  echo "${LBL[i]}  med_ms=$(ms "${TIM[i]}")  min_ms=$(ms "${MINV[i]}")  带±$(pct "${BAND[i]}")%  样本${NSAMP[i]}"
done
