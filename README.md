[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)

# RSupercell（`rscel`）
> If you find this interesting, a star would help a lot - issues and design critiques are even better.

## English summary

**RSupercell (`rscel`)** is a topology-oriented DSL for business-orchestration code: a
**Cell** is a flow node, a **`jump`** is an edge between nodes, and function bodies are
verbatim Rust. The compiler parses, statically checks (C1-C8), then emits **a single
`.rs` file with zero external dependencies** that plain `rustc` turns into a native binary.

- Five `jump` forms, all with real semantics: `jump` (tail transfer, terminating),
  `jump loop` (in-place iteration), `jump recurse` (child instance + `return(key -> alias)`
  mapping, inline-await by default), `jump and` (parallel derivation, multi-target,
  `over |> filter/map/...` pipelines, `concurrent_limit`), `jump sync` (join plus
  late-binding unpacking of `group.W<n>.key`).
- `read with` shares one `Arc` (`Arc::clone` per receiver, one-shot `Arc::new` promotion on
  the sender); `exit go with` / `die` are the only exits and every path must converge to
  exactly one of them.
- Cells are **synchronous functions by default** (no future state machine); a cell becomes
  `async fn` only when verbatim Rust inside it contains `.await`.
- Contract-stage diagnostics (E010-E090, W001/W002/W060) carry DSL line/column plus a
  concrete fix; rustc errors are remapped back to the DSL through `.map.json`
  (RSC-E900 = mapped to DSL, RSC-E901 = inside your own `extern rust` block, not mapped).
- CLI: `rscel check | build | graph | trace | edit`. `edit` serves a single-file, zero-CDN
  web editor on 127.0.0.1 with an interactive Cell topology graph, per-node contracts,
  live diagnostics, trace timeline replay and one-click build & run.
- **Performance**: measured against hand-written equivalents compiled with the *same* rustc
  flags, using same-round paired ratios normalized by an in-band frequency probe. Current
  verdict on this host: generated and hand-written code are **indistinguishable within
  ±5%**, but the machine's noise floor (±18.7% paired band) is wider than the gate margin,
  so no pass/fail claim is made and the benchmark report is withheld from this release
  until a window with sufficient resolution exists.

The Chinese prose stays authoritative; `docs/07` ends with a full English keyword reference.

> **拓扑化业务编排 DSL**：以「Cell（格子）」为业务流程节点，以「函数」为计算实现。源码经前端编译、静态检查后翻译为 Rust 源码，再调用 `rustc` 编译为原生二进制。
>
> **当前实现状态**：M1–M5 达成，M6 进行中。M7 的判定统计量在 2026-10-11 从「最小值」换成
> **中位数**（`min` 在并发形状上会被单次幸运调度定住：放大后的场景用 min 口径连测八窗，
> `pipeline_and` 跨 94.40%–103.09%，而紧带窗之间互相打架；数据与推导见 `docs/06` 实验四，
> 表里 `min` 仍作参考列，旧读数全部保留）。换完之后**七窗**（逐窗原始表在 `docs/06` 实验四，
> 没有挑窗；另有第 8 窗 = 报告所附，四格读数全部落在既有区间内，按八窗计数是
> `pipeline_and` 8/8、`read_share` 7/8、`loop_sum` 7/8、`loop_sum_opaque` 1/8）：`pipeline_and` **99.03%–101.28%**（**七窗全部 ≥99%**）、
> `read_share` **98.22%–100.79%**（六窗 ≥99%、一窗掉线下）、
> `loop_sum` **98.66%–100.60%**（六窗 ≥99%）、`loop_sum_opaque` **97.89%–99.41%**（六窗 <99%）。
> → 能站住的最强句子是「**四个场景与同构手写的差都在几个点的量级里**」，
> 但**只有编排形状能反复给出 ≥99%**（`pipeline_and` 两轮 4/4、7/7 全部 ≥99%）：
> 编排两格七窗跨 2.3–2.6 个点、`loop_sum` 跨 1.9 个点，而 `loop_sum_opaque` 在**分母跟到展开形状之后**四窗跨 **3.5 个点**
> （96.35%–99.82%）→ 那一格连"±2.5 个点"都不成立（`docs/06` 实验六）。
> 按对称口径不许写"比手写快"，也**只有 `pipeline_and` 一格**能说跨窗同号 ≥99%。
> → **跨窗漂移 1.5–2.6 个点是这台机器的分辨率底线，而 99% 门禁线与被测差值同量级**，
> 所以任何单窗百分比都不算判定，只能报多窗同号区间。
> `loop_sum_opaque` 那一格在 min 口径两边最快运行互相打平（6.15–6.31 / 6.22–6.32ms、
> 谁快谁慢逐窗翻转）、中位数却低一档，一次独立交错直采样探针方向相反 → 该现象已定位并结案：
> 既不计入成绩也不是已证实的缺陷，因为反向探针显示这一格的差值
> 来自**代码布局敏感度**而不是转译成本（下面第 ③④ 条与 `docs/06` 实验六）。
> **该问题已结案（2026-10-11, `docs/06` 实验五 + 实验六）**：① 原来那句"生成物的**上尾**更长"
> 被直接量掉了 —— 新加的 `上尾`（中位/min）列自己就跨窗摆 1.77%–6.13%，生成物并非每窗最长；
> 可复现的是**整个分布向右平移约 1%**（真实生成物的中位数比同窗手写展开形状慢
> +0.78%…+2.33%，**六窗同号**，而两边 `min` 互相打平）。② 内嵌运行时死代码（新增 `V08` 格，
> 四窗 |Δ| ≤0.2%）与 entry 的 `#[inline(always)] fn cell_main` 外壳（新增 `V09` 格，两窗都不比
> 对照慢）两个候选**都被实测剔除**。③ 那一格的**门禁分母同构度确实过期**，随后已把它跟到
> 展开后的形状（逐处照抄生成物：块表达式 + hoisted `let` + 别名拷贝 + `Some(Ret…)` 包装，
> `-D warnings` 零告警、`checksum` 逐字相等）——**但残差没有跟着消失**：四窗读
> **99.82% / 97.90% / 96.35% / 98.33%**（方向不同号，其中两窗按对称口径出带判"未达门禁"），
> 比旧分母的七窗（97.89%–99.41%，1.5 个点）反而**更散** → "分母过期解释这一格"这个假设被数据否掉。
> ④ 反向探针（从真实 `gen.rs` 按锚点逐字删/换的 `GEN_nort` / `GEN_noargv` / `GEN_pad`）
> 把最后两个候选也剔掉了：死代码七窗符号翻 |Δ|≤1.35%；`__rsc_argv` 采集行"值" 0.6%–3.0%，
> **但同位置换成纳秒级占位语句复现同一量级**，而那行在 `t0` 之前、按定义进不了 `elapsed_ns`
> → 这一格的差值属**代码布局敏感度**（asm 里两侧热循环指令混合相同、`padd` 各 7 条、
> 循环前同为 `.p2align 4`），不是转译成本。**没有为那 1.5% 改动生成器**（数据不支持收益）。
>
> - 5 种 jump 形态全部有语义实现：`jump`（出口转移）、`jump loop`（原地循环）、`jump recurse`（子实例 + `return` 映射，D3 内联 await）、`jump and`（并行派生，含**多目标**）、`jump sync`（等待组 + 多包 receive late binding）。
> - `read with` 生成 `Arc::clone`，未共享变量按 §8.1 一次性 `Arc::new` 提升（附录 A.7 三层转发端到端通过）。
> - Cell 默认是**同步 fn**（零 future 状态机）；只有透传 Rust 里出现 `.await` 的 Cell 才生成 `async fn`。
> - 契约期诊断覆盖 E010/E011/E014/E020/E030 段/E035/E036/E037/E039/E040–E047/E050/E060–E064/E070/E080/E090 + W001/W002/W060。
>   其中 C4 所有权（E060–E064）的受跟踪名字 = `receive` 别名 **+ `let` 绑定**，`over <coll>`
>   按 §10.1 计入 move，所以 `over items` 之后再 `items.len()` 在 `rscel check` 就报，
>   不用等 rustc 的 E0382（2026-10-11）。
> - M7 性能：**四个基准场景对同构手写 Rust 只能说"差在几个点的量级里"，且其中只有编排形状
>   能反复给出 ≥99%**，**分辨率地板已撞到**
>   （中位数口径七窗：`pipeline_and` 99.03%–101.28% 七窗全 ≥99%、`read_share` 98.22%–100.79%、
>   `loop_sum` 98.66%–100.60%、`loop_sum_opaque` 97.89%–99.41%，见上面说明与 `docs/06` 实验四）。
>   第 5 个场景 `async_step`（唯一含 `.await` 的那个）2026-10-11 才进门禁台，**目前只有 1 窗
>   99.01%（复现带 ±2.10%）** → 按"单窗不算判定"这条，它现在只能说与手写打平，**不许**把它
>   算进"≥99%"那一句覆盖的范围里。
>   `loop_sum_opaque` 的门禁分母**已跟到展开后的形状**（上面第 ③ 条），
>   跟完之后那一格读 96.35%–99.82%（四窗、两窗出带判未达、一窗打平、一窗未定论 → 不同号）
>   → 按 §17.2 **不**声称 ≥99%，也不判成已证实的缺陷（原因见上面第 ④ 条）。
>   **"同构"这个词现在的问题换了一格**：`bench/loop_sum_fn_hand.rs` 仍是"循环在独立 `fn`"的
>   旧形状（常量起始那一格所有形状都挤在同一档：四窗 98.97% / 99.11% / 99.63% / 98.57%，
>   生成物 13.17–13.23ms vs 它 13.03–13.19ms，形状差没露出来），文件头已改成如实描述。
>   旧报告里的 `jump loop` 比值 3.19/3.97 是**基准台自身不公平**造成的假象（手写对照用了
>   与 DSL 不同的起始值不透明度），已修正并查清；剩下的那格真缺陷——「起始值为运行期
>   未知值时生成物慢约 2 倍」（旧 `loop_sum_opaque` 49.6%–50.3%）——根因已由控制变量
>   实验定死：**只有「循环待在独立 `fn` 里」这一个因素收费**，`Option<Ret_x>` 返回、
>   多一层 `cell_main`、`let` 外提、`v_<id>` 别名拷贝、内嵌运行时死代码、`println!`
>   全部实测零开销（各格互相差 ≤2%，数据见 `docs/06-控制变量实验.md`）。
>   生成器据此实现了**热 Cell 实体展开**（同步 Cell 的 body 直接抄进它的**每一个** plain
>   调用点的块表达式，预算 = 份数 × body 语句数 ≤ 24；原先只放行单一调用点，
>   2026-10-11 的实验七量出「多一个调用点」这一条本身值 **2.00x**；`#[inline(always)]`
>   只收回一半，因为 LLVM 虽把函数内联进调用方、
>   向量化器却不接手——asm 里 `padd` 为 0），展开后 `loop_sum_opaque` 两轮
>   **100.10% / 100.17%**（min 口径，复现带 ±0.03%–0.05%；中位数口径下这一格的读数见上面说明）。
>   **S2 类型化调用约定的 `v_<id>` 别名拷贝实测零开销**（V05 ≈ V02，差 0.1%）。
>   编排两场景（`pipeline_and` / `read_share`）的**同构分母已修正**（`scripts/bench/orch.sh`
>   的单因素探针，方法见 `docs/06-控制变量实验.md` 实验三）：`bench/*_fn_hand.rs` 原来用
>   `supervisor.join()` 取并发组结果，而生成物的 `jump sync` **不 join supervisor**
>   （publish/wait + `catch_unwind`）。既然这两个文件的约定就是"与生成物同构"，这一处差异
>   按定义就该消掉；改完分母变快，于是旧报告里 `pipeline_and` **106.02% / 106.28%** 那两格
>   有一部分是**分母自己带的形状差**，不是转译白赚。修正后的六窗全表（**min 口径**，
>   `docs/06` 实验三；随后换了统计量，新读数在上面的实现状态段与 `docs/06` 实验四）：
>   `pipeline_and` = 98.52 / 99.56 / 98.70 / 104.66 / 99.92 / 104.80，
>   `read_share` = 97.22 / 129.95 / 104.11 / 103.65 / 94.04 / 99.27 ——
>   **没有任何一格可以写成"比手写快"**，带 >5% 的窗判**未定论**，其余只说「与手写打平」。
>   同一批里没被碰过的 `loop_sum` / `loop_sum_opaque` 也同样摆 4–5 个点，所以**单窗百分比
>   一律不算结论**，这条已写进测量纪律；把这条推到终点的正是上面那句
>   统计量变更（跨度 8.7 个点 → 1.1 个点）。基准台的判定同时改成**对称口径**：
>   缺口出得了复现带才叫「未达门禁」，出不了带只叫「未定论」（旧逻辑允许把带内噪声当成失败，
>   却不允许把它当成成绩）。
>   数值见 `docs/04-性能基准报告.md`，判定方法见 `docs/06-控制变量实验.md`。
> - `jump and ... over ... read with(...)` 这条路径在 2026-10-10 之前**从来没有编译成功过**
>   （元组 extra 的 E0609/E0507），现已修复并有端到端回归。
> - M6（rustc 错误反映射 / srcmap / trace / 增量缓存）与「前端专属编辑器」正在持续迭代。

## 项目结构

```
rscel/
├── Cargo.toml                      # workspace（5 个 crate）
├── crates/
│   ├── rscel-parser/               # lexer + parser + AST + 诊断（116 测试 / 0 失败）
│   ├── rscel-runtime/              # Packet/Group/Semaphore/标准库自研 executor（37 测试）
│   ├── rscel-check/                # C1-C8 静态检查（25 测试）
│   ├── rscel-gen/                  # S2 静态类型化代码生成（含 examples/*.rscel 端到端验收样例）
│   └── rscel-cli/                  # check / build 编排（+ assets/editor.html 前端编辑器）
├── examples/
│   ├── multi_branch_die.rscel      # 附录 A.5（修正版）
│   └── extern_rust_mix.rscel       # 附录 A.6（修正版）
├── bench/                          # 性能基准场景 + 手写等价对照（§17.3；每个场景一到两份基线：
│                                   #   `_hand.rs` 自然写法 / `_fn_hand.rs` 与生成物同构 = 门禁分母；
│                                   #   `async_step` 只有后者，因为它的"自然写法"必然与生成物不同构）
├── scripts/bench/run.sh            # 门禁台：同 flag 编译、取中位数、算比值、卡门禁 + async 分母同构守卫
├── scripts/bench/{ab,orch,async_probe}.sh   # 三张归因探针台（与门禁台同一套中位数/双相位/复现带口径）
├── scripts/bench/{cache_check,buildtime}.sh # 增量缓存的两座基准台（等价性扫描 + 构建侧计时）
├── scripts/bench/editor_build.sh   # 编辑器 `/api/build` 的构建侧计时台（稳定名 + 缓存命中）
├── docs/
│   ├── 00-内存模型与选型报告.md     # 选型 + 性能模型（v0.3-rev1）
│   ├── 01-选型决策记录.md           # ADR-001（S1-S4 选型决策）
│   ├── 02-Rscel语言规范.md          # 语言规范（**读完就会**：动机 → Cell → jump → 所有权 → 附录 A 全样例 → 速查）
│   ├── 04-性能基准报告.md           # 由 scripts/bench/run.sh 自动生成，勿手改数值
│   └── 05-迭代待办.md               # 待办与问题跟踪清单（按优先级分级）
└── scripts/bench/
    └── packet_overhead.rs          # S2 选型基准（实测类型擦除 vs 类型化 69-89× 差距）
```

## 构建与使用

```powershell
# 一次性编译
cargo build --release

# 跑测试（427 项，含端到端编译+运行断言，需 rustc；不跳过任何用例）
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # 必须零告警

# 性能基准（与手写等价 Rust 同 flag、交错轮转计时；门禁 RSC_GATE=99 即「99% 性能转译」；
# 判定统计量是中位数（2026-10-11 从最小值换过来，min 仍作参考列）；每个程序自动跑两个
# 错开的相位，两份中位数之差 = 这一窗的「复现带」，带超过 RSC_RES_MAX
# 默认 5% 就直接判「未定论」而不是 PASS）
bash scripts/bench/run.sh                 # 全部场景（loop_sum / loop_sum_opaque / pipeline_and / read_share / async_step）
bash scripts/bench/run.sh loop_sum        # 单个场景
RSC_REPS=15 bash scripts/bench/run.sh     # 正式判定用：轮数多、且要两轮同号
# 判定是**对称**的：带内不许当成绩，也不许当失败 —— 缺口出得了复现带才判「未达门禁」，
# 出不了带判「未定论」（同样不给 PASS）
# `async_step` 是 2026-10-11 加进来的第 5 个门禁场景，它同时是
# **唯一含 `.await` 的那一个**；跑门禁台时它会额外做一次「分母同构守卫」：把生成物里嵌入
# 运行时的 `__rsc_block_on` 与 `bench/async_step_fn_hand.rs` 的 `block_on` 去注释、去
# `__rsc_` 前缀后要求**逐字相等**，失配直接退出 —— 否则改了执行器却没跟分母，那一格量的
# 就是"执行器差异"而不是"转译差异"（基线形状过期曾多次导致误判，见 docs/06 实验三/六/八）。
# 如需回退，把 `run.sh` 的 `scenarios=(…)` 里 `async_step` 删掉再跑一次门禁台即可，
# 前四个场景的基线读数不受影响（比值是逐场景独立算的）。

# 归因实验（"慢/快是哪一格造成的"，一次只动一个因素；数据表写进 docs/06）
bash scripts/bench/ab.sh                  # 数值热循环矩阵（V00–V09 + 真实生成物锚点），判据含 --emit=asm 的 padd 条数
bash scripts/bench/orch.sh                # 编排形状阶梯（同 flag + 相位复现带 + 校验值硬比对；锚点失配直接报错退出）
bash scripts/bench/async_probe.sh         # async 形状阶梯（同一套中位数口径；变体全部从真实生成物按唯一锚点派生）
RSC_ASYNC_ONLY="GEN_floor GEN_floor_inl GEN_extinl" RSC_ASYNC_REPS=41 \
  bash scripts/bench/async_probe.sh       # 那张 2×2：{async,sync} × {无内联提示,有内联提示}
RSC_ORCH_REPS=101 RSC_ORCH_ONLY="A0_base A4_join GEN_real" bash scripts/bench/orch.sh   # 只跑选中格、压小复现带
# 两张探针台与门禁台用**同一套统计量**（每相位取中位数、平均作判定值、差作复现带，min 只作参考列），
# 所以它们的百分比可以直接对账（2026-10-11, docs/06 实验五）。选格跑矩阵时要显式给对照格：
RSC_AB_REPS=101 RSC_AB_REF=V08_runtime_flat RSC_AB_ONLY="V00_main V08_runtime_flat V09_entrywrapper GEN_real" bash scripts/bench/ab.sh

# 实际用：阶乘（样例真身在 crates/rscel-gen/examples/）
.\target\release\rscel.exe build .\crates\rscel-gen\examples\factorial_loop.rscel -o factorial.exe --release
.\factorial.exe
# 预期输出：120

# 实际用：多分支互斥
.\target\release\rscel.exe build .\examples\multi_branch_die.rscel -o mb.exe --release
.\mb.exe
# 预期输出：（无——main 传 Default::default()=0 不匹配任何分支 → die → 无输出）

# 实际用：extern rust 混合
.\target\release\rscel.exe build .\examples\extern_rust_mix.rscel -o er.exe --release
.\er.exe
# 预期输出：（无——exit go with 无副作用）

# 实际用：--debug 的运行期事件流（§15.3，2026-10-11 落地）
.\target\release\rscel.exe build .\examples\multi_branch_die.rscel -o mb.exe --debug
.\mb.exe 1
# 预期 stderr（stdout 一行都不受影响，埋点全走 stderr）：
#   [rsc] cell start spawn @ 8:1 recv=[val]
#   [rsc] cell start jump -> one @ 10:1 send=[n]
#   [rsc] cell one spawn @ 15:1 recv=[n]
#   [rsc] cell one exit @ 18:1 send=[]
# 传 7（三个分支都不命中 → 走 die）则只有两行：spawn + `[rsc] cell start die @ 13:1`。
# 带 when 的 jump 在条件不成立时一行都不打 —— 否则事件流会显示一次根本没发生的传输。

# 实际用：§16.2 增量缓存（2026-10-11 落地）——同一条命令行第二次构建跳过 rustc
.\target\release\rscel.exe build .\examples\multi_branch_die.rscel -o mb.exe --release   # 冷构建
.\target\release\rscel.exe build .\examples\multi_branch_die.rscel -o mb.exe --release   # 命中
# 命中那行的 stderr 是：RSC: 增量缓存命中，跳过 rustc（… 字节产物来自同一条命令行的构建）
# 实测（基准台 `bash scripts/bench/buildtime.sh`，release 档编译器、同一台机器、N=7 与 N=9
# 两窗交错、判定值取中位数，两窗同号）：
#   冷构建 373–935ms → 命中 80.6–82.2ms，加速 **4.5x–11.5x**（loop_sum_opaque 4.5/4.6x、
#   pipeline_and 11.0/11.1x、read_share 11.5/11.5x、multi_branch_die 4.7/4.7x）
#   同一批早期用 node 台量过一版（5.4x–14.0x、命中 64–66ms）：同一结论、绝对值不可跨台混比，
#   差值就是每台每次 spawn 的固定开销（~15–25ms）。
# 代价也说清：带缓存的冷构建比 `--no-cache` 贵 **49–69ms**，而这一格几乎整块就是
#   `rustc -vV` 那一次进程启动（本台单测它自己 = med 65.3–66.9ms / node 台 50.9–52.4ms）——
#   版本必须进缓存键，否则换了工具链还吃旧产物。`--no-cache` 既不读也不写（连目录都不建）。
# 为什么键是"整次 rustc 调用"而不是 §16.2 字面的"每 Cell 文本哈希"，以及为什么缓存
# 只盖 rustc（前端实测只占构建的 1.4%–2.9%）：见 crates/rscel-cli/src/build_cache.rs 头部。
# 等价性（缓存命中与否 .rs / .map.json 逐字相同）由基准台复现：bash scripts/bench/cache_check.sh
#
# 事件词汇表（@ 行:列 是 **DSL** 的行列，与诊断反映射同源，编辑器可直接高亮回源文件）：
#   cell <名> spawn @ 行:列 recv=[键名表]      实例创建 + 收到的 Packet
#   cell <名> jump -> <目标> @ 行:列 send=[..]  plain jump 发包
#   cell <名> recurse -> <目标> @ 行:列 send=[..]  recurse 内联路径发包
#   cell <名> exit @ 行:列 send=[键名表]        exit go with 交回结果（同时是实例销毁点）
#   cell <名> implicit exit @ 行:列 send=[]      body 以 jump and 收尾时的兜底收敛
#   cell <名> die @ 行:列                       die 的 DSL 行号
#   group <组名> derive <调用方> -> <目标> limit={} items={} @ 行:列 send=[..]
#   group <组名> permit cell=<目标> acquire | release died=<是否 die>   并发令牌，每元素一对
#   group <组名> done cell=<目标> slots={} died_in_slots={} @ 行:列      jump sync 处的组完成
# 三处有意不插：extern rust{} 块内（用户 Rust 逐字透传）、jump loop（§9.2 它不搬 Packet，
# 且每轮一次会变成刷屏）、以及 **release 模式一行都不发**（生成物必须与本功能存在之前
# 逐字相同，它是性能门禁的被测对象；这条由 `gen_trace_test` 与一次 HEAD 对照构建共同钉住）。
# `done` 的 slots/died_in_slots 只统计**被保留的槽位**（D5 丢弃了 slots 之外的结果），
# 「哪个实例死了」的唯一真相来源是 permit 的 release 行。

# 前端编辑器。只服务本机浏览器：Host 或 Origin 指向别处一律 403，
# 因为 /api/save 会写文件、/api/build 会编译并运行提交的代码。
# 构建模式选 --debug 时，上面那段事件流会随产物的 stderr 一起回传到浏览器
# （/api/build 的 stderr 字段里「// 产物运行期 stderr」那一节）。
.\target\release\rscel.exe edit .\crates\rscel-gen\examples\factorial_loop.rscel --port 8421
# 浏览器打开 http://127.0.0.1:8421 （Ctrl-C 退出）
```

## 退出码（§4.1）

| 码 | 场景 |
| --- | --- |
| 0 | 成功 |
| 1 | 前端错误（parse / check） |
| 2 | 参数错误 / rustc 编译错误 |
| 3 | IO 错误（文件不存在等） |

## 选型（S1–S4，由开发者于 2026-09-27 全部确认，详见 ADR-001）

- **S1**：async runtime = 标准库自研极简 executor（零外部依赖）
- **S2**：Cell 调用约定 = 静态类型化 monomorphized（`async fn cell_x(typed_args) -> Option<Ret_x>`），plain jump 零 Packet 零分配
- **S3**：`die` 时父侧多包 receive 缺键 → 契约期静态拒绝（**已实施**：rscel-check 报 E042/E044）
- **S4**：多包 `receive(g1.W1.A)` → late binding，在 `jump sync g1` 之后解包（**已实施**：rscel-gen 等待 sync stmt 出现再 emit）

## 性能（实测，详见报告 §4.1）

- 类型擦除 Packet：85–90 ns / 跳
- 类型化直接传参：1.0–1.2 ns / 跳
- **比值 69–89×**：S2 是 ≥90% 性能目标达成的必要条件
- `read with` 单价：12.2–12.8 ns / 次转发

## 当前已知限制

| 项 | 状态 |
| --- | --- |
| `jump loop` 生成形状 | 与同构手写的差**只有 1–2 个点，而该量级等于这一格对代码布局的敏感度** → **既不声称 ≥99%，也不判成已证实的缺陷**（2026-10-11, `docs/06` 实验六）：`loop_sum`（常量起始，中位数口径七窗）**98.66%–100.60%**；`loop_sum_opaque`（入参为运行期未知值）两边**最快运行相等**（gen min 6.15–6.31 / 同构侧 6.16–6.32ms，谁快谁慢逐窗翻转），典型运行读 **96.35%–99.82%**（门禁分母跟到展开形状之后的四窗，其中**两窗出带判未达**；旧分母七窗 97.89%–99.41%）→ **跨窗不同号，按 §17.2 收缩措辞**。为什么不能再往下归因（反向探针：从真实生成物**逐字删/换**因素的 `GEN_nort` / `GEN_noargv` / `GEN_pad` 三格，七窗）：① 内嵌运行时死代码七窗符号翻、\|Δ\|≤1.35% → **不收费**（候选 3 第三次被判掉）；② `__rsc_argv` 采集行"值" 0.6%–3.0%，但**同一位置换成纳秒级的占位语句复现了同一量级**，而那行在 `t0` 之前、按定义进不了 `elapsed_ns` → 剩下的解释只有**代码位置 / 寄存器分配层面的布局敏感度**（asm 核对：两侧热循环指令混合相同、`padd` 各 7 条、循环前同为 `.p2align 4`；生成物与 `GEN_nort` 的循环**逐字相同**）。这一格的结论已写入 `docs/05`，**没有**为那 1.5% 改动生成器。展开的边界放宽到「预算内的每一个 plain 调用点」（根因：LLVM 会内联函数但不接手向量化，`#[inline(always)]` 只收回一半，见 `docs/06-控制变量实验.md`）：实验七用两份真实生成物做单因素对照（同一条递推、同一个 `checksum`，只差一个运行时**永不执行**的 `jump`），量出「多一个 plain 调用点」这一条闸门本身值 **2.006x / 1.998x**（两窗同号、各自复现带 ≤0.8%）→ 现在 2、3、N 个调用点都逐点展开（份数 × body 语句数 ≤ 24 为体积预算，arity 不一致则整格退回函数形状），`recurse`/`and` 指向的 Cell 与 async Cell 仍带函数边界（前者经实验判定；后者结论是"不做"：`GEN_block`（把 async Cell body 展开进调用点的 `__rsc_block_on(async move { .. })`）两窗读 1.019x / 1.019x，`GEN_always`（只给 `async fn` 加内联提示，消的是同一个边界）读 0.997x / 1.003x → 全都打平或更慢，结构原因是 `jump loop` 原地循环让 Cell 的 `async fn` 边界**整个运行只进入一次**，见 `docs/06` 实验八）。**`jump and` worker 那一格已用探针判掉**：`G1_expand`（把 worker 体展开进 `spawn` 闭包）对真实生成物五个可用窗读 +0.9%/−4.5%/+1.8%/+2.2%/+1.4%，方向不稳且全在带内 → 数据不支持，生成器不动（详见 `docs/05`）；**换成中位数口径重测三窗仍成立**（`G1_expand` 对真实生成物 −1.08% / +0.36% / +0.03%，`docs/06` 实验五） |
| async 形状（Cell 透传含 `.await` ⇒ `async fn cell_x`） | **与同构手写打平**（2026-10-11，`docs/06` 实验八）：新场景 `bench/async_step.rscel` 与门禁场景 `loop_sum_opaque` 同一条递推、同一个 `checksum=1249999975000000`，分母 `bench/async_step_fn_hand.rs`（含**同构的执行器**）；两窗性能读数 **99.76% / 101.23%**（复现带 ≤2.63%）。这一格**改前不是打平的**：内嵌执行器 `__rsc_block_on` 原来写 `let mut fut = Box::pin(fut)`，堆指针让 async 热循环每轮多 5 条穿过 `%r8` 的存储（asm 已核对），同窗单因素探针读 **−16.6% / −14.2%**；改成栈上 `std::pin::pin!(fut)` 之后，把该因素**换回去**反向复测四窗同号 **+18.1% / +18.9% / +18.9% / +17.2%** → 已落地。**仍然固有、不属于转译损失的那 3 倍**：`GEN_noextern`（每轮不要那次 `await`）读 22.8–23.5ms 而同一条循环同步展开跑 6.5ms，机制是循环待在有 suspension point 的状态机里向量化器不接手（`padd`：async 四格全 0，同步生成物 7 条），而**同构手写同样 `padd=0`、同样 71ms** → 这份钱是 DSL 程序自己要求每轮 `.await` 付出的。**这一场景已升为第 5 个门禁场景**（`run.sh` 场景表 + 分母同构守卫）。
进门禁之后那一格只有 1 窗（99.01%，复现带 ±2.10%），按"单窗不算判定"**不并入 ≥99% 的声称**。
实验九又补了两条（`docs/06`）：① "给 `extern rust` 的 `async fn` 发内联提示"这条候选
**被 asm 判掉**——`#[inline(always)]` 加与不加，归一化后的指令流**一字不差**（独立复核：
942 条 vs 942 条、0 行差），LLVM 早就内联了那个单行 fn；② `GEN_floor`（每轮换成同步 fn）
那个"翻转"**符号跟着干扰强度走**（七窗按上尾排序：≤17.2% 的四窗全读同步版更慢、≥24.6% 的三窗
全读更快，中间不重叠）→ 本机的规矩写死成一句：**不要跨形状比百分比**（async↔sync、
展开↔函数），只比同形状的两个程序。栈上 pin 的代价与边界写在
`crates/rscel-gen/src/runtime.rs` 注释与 `docs/06` 实验八/九 |
| 编排形状（`jump and` + `read with`） | **2026-10-11 换量法之后第一次跨窗同号**：场景放大 ×10（200 万 / 50 万元素，单次运行 130–150ms / 60–67ms）+ 判定统计量从 min 换成**中位数**，六窗全部 ≥99%：`pipeline_and` **99.03%–101.28%**、`read_share` **99.16%–100.79%**（复现带 ±0.50%–1.96%，跨度 2.3/1.6 点）。同一批用 min 口径连测八窗读 **94.40%–103.09%**（跨度 8.7 个点、紧带窗互相打架）→ 那批百分比全部作废为历史数据（`docs/06` 实验四有整张对照表）。措辞仍按对称口径停在「与手写打平」：差值 ≤ 本窗带，不许写"比手写快"；而**跨窗 1.5–2.3 个点的漂移是本机新下限**，99% 线与之同量级，所以单窗读数一律不算判定。旧 106.02%/106.28% 里有一部分是**基线自己与生成物不同构**（`join()` vs publish/wait）造成的，已把基线改成与运行时一致；交接那一处的**单因素幅度已经复现**：`orch.sh` 的 `A4_join`（只把 supervisor 交接换回 `join()`）三窗读 **+4.07% / +4.19% / +4.37%**、全部出带、跨度只有 0.3 个点 → **这一处形状差 +4.1%–+4.4%**（中位数口径三窗同号，`docs/06` 实验五；与实验三 min 口径的正向 −4.1%…−8.7% 不同口径不同方向，只能对方向）。两张探针台（`ab.sh` / `orch.sh`）已与门禁台**统一成同一套中位数口径**，其单因素读数可以直接与本表对账（详见 `docs/05`，数据在 `docs/06` 实验五） |
| 入口 Cell 的命令行实参 | `entry` 的 `receive` 键类型只有在 argv 能承载时才真接（`String` / `Vec<String>` / 整数 / 浮点 / `bool` / `char`）；其余类型**静默取 `Default::default()`**，传了实参也不报警（详见 `docs/05`） |
| rustc 错误反映射（§15.2）+ `.map.json` | 已落地（RSC-E900/E901，能把 `cell main @ 13:1` 这种 DSL 位置打回来）；跨文件/宏 span 的边界仍待办（详见 `docs/05`） |
| `rscel graph` / `rscel trace` / 增量缓存 | 三条都已实现。§16.2 的增量缓存**只缓存 rustc 的产物**（`.rscel-cache/`，从源文件目录向上找项目根），命中与否 `.rs` 与 `.map.json` 逐字相同（`bash scripts/bench/cache_check.sh` 实测 9/9），`--no-cache` 绕过读与写；按构建阶段实测（前端只占构建的 **1.4%–2.9%**，node 口径 0.3%–3.4%，两台一致地 <3.5%）放弃规格字面的「每 Cell 文本哈希 + 按 Cell 拆 `mod`」，因为那与热 Cell 实体展开直接冲突且省不到东西。**两个未覆盖点已经只剩一个**：① ~~`rscel edit` 的一键 build 每次把产物名递增 ⇒ 那条路径永远 miss~~
**已修（2026-10-11）**：`/api/build` 改用稳定名 `{prefix}-last`，只有在那个位置
真写不进去（上一次产物还在跑）时才退回递增名并把原因打到 `stderr`。同一条源文本连点两次实测
（基准台 `bash scripts/bench/editor_build.sh`，release 档、REPS=7、每窗先清会话缓存、中位数）：
修之前 549.4/563.4ms → 559.5/568.1ms（第二次**没省**，命中 0/7、形状 14/14 递增名），
修之后 535.5/535.1/535.2ms → **134.5/139.0/133.1ms**（**3.85x–4.02x**，命中 7/7、形状 14/14 稳定名；
`second` 档摆动 16.5%–21.7% 过了 15% 线，但绝对散布只有 22–35ms，而判定量是 4 倍）。
**改了源文本那一次绝不许命中**（稳定名的全部风险就是把上一个程序的产物端给用户），
这条由 `editor_test::consecutive_builds_hit_the_cache_but_a_changed_source_does_not` 钉住；
退路行为由 `…an_unwritable_stable_artifact_falls_back_to_an_incrementing_name_with_a_reason` 钉住。
② 带缓存的冷构建贵 **49–69ms**，而这一格几乎整块就是 `rustc -vV` 那一次进程启动（本台 med 65–67ms / node 台 51–52ms；能否进一步优化待后续评估） |
| 前端专属编辑器（拓扑调试） | 已就位：`rscel edit` 起 `127.0.0.1` 本地服务 + 单文件前端（自绘 Cell 有向图、契约/所有权查看、trace 回放、一键 build+run）。**安全边界四条已齐**（2026-10-11）：只绑回环、请求体 4 MiB / 请求头 64 KiB 上限、写文件只允许「启动时那个文件 + 工作目录**直接**下面的 `.rscel`」、**来源校验**（`Host` 非回环 或 `Origin` 非回环含 `null` → 路由之前就 403；响应一律 `X-Frame-Options: DENY`）。最后一条为什么是硬边界而不是提示：`/api/build` 会 rustc 编译并**运行请求方提交的代码**，而任何网页都能对 `127.0.0.1` 发无预检的表单 POST —— 靠 `Origin` 挡跨站、靠 `Host` 挡 DNS rebinding、靠拒绝内嵌挡 iframe（那种形状下两个头全都是合法的）。**未做**：`/api/save` 的新建文件落点是**进程工作目录**而非文档旁边 |
| `over` 之后再用同一个集合 | **契约期拦**（2026-10-11）：`over <coll>` 按 §10.1 是 move，之后再读它发 **RSC-E062**（透传文本里，如 `let n = items.len();` / `println!(.., items.len())` / `out(..)`）或 **RSC-E060**（传输实参位，如再写一次 `over items`、`go with(items -> k)`）。`let` 绑定与 `receive` 别名同样入状态机，POD 注解（`let n: i64`）豁免；判定分辨率见 `docs/02` §10.3 的表（花括号**之内**的引用按"宁可漏报不误报"留给 rustc） |
| `jump and` 的多余实参 / 非 Clone 实参 | 契约期**不拦**，要么落到 rustc 报错、要么静默（部分已修，其余见 `docs/05`） |
| Cell 体内有**带 `when` 的 plain jump**，而该 Cell 自己的 `exit go with` 带键 | 生成物在该分支里构造 `Ret_<本 Cell> {}`（这条路径走不到自己的 exit，没有任何值可交）→ rustc **E0063: missing field**，而 `rscel check` 目前**不拦**（2026-10-11 实测：`check OK: 3 cells, 0 errors` → `rscel build --debug` 退 2）。反映射是好的（`RSC-E900` 指回 `cell w @ 9:1`），缺的是契约期诊断。已登记，待定「契约期拒」还是「给这条路径定义语义」（详见 `docs/05`）；**不**用 `Ret<X>::default()` 兜，因为那会把零值当成真算出来的结果交给 `jump and` 的结果槽 = 静默降级 |
| 多目标 `jump and` 同时带 `over` | 规范 §7 未定义其语义；当前不拦也不支持，需先定语义（详见 `docs/05`） |
| D2 类型推导 | 注解优先 → 入边传播 → 表达式上的 `as <类型>` → **调用点可见的 `extern rust` fn 签名返回类型** → 字面/带注解 let 推导；推不出发 `compile_error!` 要求注解，绝不猜。`as` 消歧只覆盖基本类型名，复杂类型请写 `receive(k: T)`（详见 `docs/05`）。走 `compile_error!` 那条路径时生成物只有一串问题陈述 + **一行空 `fn main() {}`**（2026-10-11）：补 main 不是为了让编译通过，而是消掉 rustc 顺带白送的那条与用户程序无关的 `error[E0601]`，此前它把真正该看的诊断稀释成「2 errors」 |

## 文档缺陷与裁定

实现过程中发现规格文档 **18 处缺陷**（B1–B18），全部在 `docs/00-内存模型与选型报告.md` §3 记录，也作为**完整教学章节**收录在 `docs/02-Rscel语言规范.md` §13。已落地 8 处自洽裁定（D1–D8）确保生成的代码与原 spec 文档等价。

## 学习路径

1. **`docs/02-Rscel语言规范.md`**（首读）：动机 → 完整语法 → 所有权模型 → 附录 A 全样例 → 速查表。看完应能直接写程序。
2. **`docs/00-内存模型与选型报告.md`**（深入）：为什么这样设计——CSP 所有权、Packet vs 类型化调用约定、性能模型与基准。
3. **`docs/01-选型决策记录.md`**（必要时）：每个选型决策的 ADR。

## 仓库历史

```
30f5b3f chore: ignore ad-hoc test artifact bad.rscel
2cca603 feat(cli): rscel-cli 串接 parser+check+gen+rustc (批次 3B)
0e92722 feat(gen): rscel-gen 最小可验证端到端单元 (批次 3A)
f83d307 feat(runtime+check): 批次 2 — rscel-runtime 与 rscel-check
14f449a feat(parser): rscel-parser 完整实现 (批次 1)
14c491b docs: 内存模型·选型·性能预估报告（rev1）+ 选型决策记录 ADR-001 + S2 选型基准
8710c8b chore: RSupercell 规格文档入库 + Cargo workspace 骨架（rscel-parser crate 清单）
```