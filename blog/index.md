# 我同时开发了两门编程语言：RSupercell 与 INFY 的设计取舍 / Two Languages, Two Pipelines: Design Choices Behind RSupercell and INFY

> 一句话版：一门把「业务流程编排」转译成 Rust 的 DSL（RSupercell / rscel），一门规范驱动、零依赖的静态编译语言（Infinity Language / INFY）。两套编译管线，同一套诚实。
> One-liner: a DSL that transpiles business-orchestration code to Rust (RSupercell / rscel), and a spec-first, zero-dependency, statically-typed language (Infinity Language / INFY). Two pipelines, one shared discipline: honesty.

## 为什么是两门，而不是一门 / Why two, not one

做编译器的人常被问：为什么不再写一门语言？我的答案很具体——两个问题，两种解法。

**问题一**：业务编排代码（谁先跑、谁等谁、哪个分支死掉、怎么并发）一旦变复杂，用通用语言写就会变成「回调地狱 + 状态散落」。我需要一种语法，让**拓扑结构直接可见**：节点就是节点，边就是边。→ 于是有了 RSupercell（rscel）。

**问题二**：一门「可审计的编译语言」应该长什么样？如果工具链本身带 50 个依赖，谁来信任它的产物？我想要的是一门**规范先行、工具链零依赖、产物自包含**的语言。→ 于是有了 Infinity Language（INFY）。

两门语言的共同点不是语法，而是三条纪律：**文档驱动、测试兜底、如实报告**。

People who build compilers get asked: why another language? My answer is concrete - two problems, two solutions.

**Problem one**: orchestration code (who runs first, who waits for whom, which branch dies, how concurrency works) turns into callback hell plus scattered state when written in a general-purpose language. I wanted a syntax where **topology is directly visible**: nodes are nodes, edges are edges. Hence RSupercell (rscel).

**Problem two**: what should an "auditable compiler" look like? If the toolchain itself drags in 50 dependencies, who trusts its output? I wanted a **spec-first, zero-dependency, self-contained** language. Hence Infinity Language (INFY).

The common thread is not syntax but three disciplines: **docs-driven, test-backed, honest reporting**.

## RSupercell：编排 DSL → Rust，函数体透传 / RSupercell: orchestration DSL to Rust, verbatim Rust bodies

RSupercell 的核心设计决策只有一句话：**函数体就是透传的 Rust**。DSL 只负责编排（Cell 节点、jump 边、所有权流转），计算实现直接写 Rust。

这个决策规避了转译器最大的死穴——把任意语言语义映射到 Rust 的所有权系统。你不必发明「如何表达借用」，因为借用就是 Rust 本身的借用；你也不必担心生成的代码编译不过，因为生成器只做有限、可验证的变换。

- 五种 `jump` 形态：`jump`（尾转移）、`jump loop`（原地循环）、`jump recurse`（子实例 + return 映射）、`jump and`（并行派生，多目标 + 管道）、`jump sync`（等待组 + 晚绑定解包）。
- Cell 默认是同步 `fn`，只有体内出现 `.await` 才生成 `async fn`——不白付 future 状态机的钱。
- 错误反映射：`rustc` 报错通过 `.map.json` 打回 DSL 源码行列，开发者不用在生成代码里找行号。

**性能方法学（这条我想展开说）**：我们不做「我的生成物比手写快 X%」这种自我安慰式宣称。基准台用同一套 rustc flags、同轮交错计时、中位数口径、复现带、控制变量探针（ab / orch / async 三张台），并且**主动纠正过自己基准台的不公平**——旧报告里 3.19/3.97 的比值后来查明是手写对照用了不同的起始值不透明度造成的假象。现在的结论诚实得多：**生成物与同构手写代码差在几个点的量级里，其中编排形状能反复给出 ≥99%**。单窗百分比一律不算判定，只报多窗同号区间。

The core design decision is one sentence: **function bodies are verbatim Rust**. The DSL only owns orchestration (Cells as nodes, jump as edges, ownership flow); computation is written in Rust directly.

This sidesteps the classic transpiler trap - mapping arbitrary language semantics onto Rust's ownership system. You don't need to invent "how to express borrowing"; borrowing is Rust's own. You don't worry about generated code failing to compile, because the generator only performs limited, verifiable transformations.

- Five `jump` forms with real semantics: `jump` (tail transfer), `jump loop` (in-place iteration), `jump recurse` (child instance + return mapping), `jump and` (parallel derivation, multi-target + pipelines), `jump sync` (join + late-binding unpacking).
- Cells are synchronous `fn`s by default; a Cell becomes `async fn` only when verbatim Rust inside it contains `.await` - no wasted future state machines.
- Error remapping: `rustc` errors are mapped back to DSL source positions via `.map.json`.

**On performance methodology (worth expanding)**: we do not make self-soothing claims like "my output is X% faster than hand-written". The benchmark bench uses the same rustc flags, interleaved timing, median statistics, reproducibility bands and control-variable probes (ab / orch / async), and we **actively corrected our own unfair benchmark** - the old 3.19/3.97 ratios turned out to be an artifact of the hand-written baseline using different starting-value opacity. The honest conclusion: **generated code is within a few points of hand-written equivalents, with orchestration shapes reliably hitting ≥99%**. Single-window percentages are never treated as verdicts; only multi-window same-sign ranges count.

## INFY：规范驱动、零依赖、双轨产物 / INFY: spec-first, zero-dependency, dual-track

INFY 是另一条路线：**先写完整规范 v2.0，再写参考实现**。语言版本、工具链版本、ABI（IFABI-2）、字节码（IFBC v1）、标准库版本全部版本化。

- **零第三方依赖**：工具链 `offline = true` 构建，编译器本身是信任边界——供应链模型只适用于用户代码。
- **双轨产物**：AOT 路径（INFY → C11 → 宿主 `cc` → 单文件可执行 + `.inf` 段）覆盖独立程序；IFBC 字节码 + 六项装载期校验覆盖嵌入宿主场景。
- **210 个诊断码**，渲染做到字节级版式（gutter、caret、note/help、建议行），金样测试逐字符对齐。
- **供应链校验**：模块/包/文件三层 + SHA-256 Merkle dirhash，`MODULE.infy.lock` 可复现构建。
- **诚实**：README 明确区分「已实现 / 收敛中 / 明确未实现」，并且**不声称「INFY v2.0 兼容」**——规范要求 2300 用例 100% 通过，没跑满就不吹。

INFY takes the other path: **write the full spec v2.0 first, then the reference implementation**. Language version, toolchain version, ABI (IFABI-2), bytecode (IFBC v1) and std version are all versioned.

- **Zero third-party dependencies**: the toolchain builds with `offline = true`; the compiler is the trust boundary - the supply-chain model applies only to user code.
- **Dual-track output**: the AOT path (INFY -> C11 -> host cc -> single executable + `.inf` segment) covers standalone programs; IFBC bytecode with six load-time checks covers embedding.
- **210 diagnostic codes** with byte-exact rendering (gutter, caret lines, note/help, suggestion lines), golden-tested character by character.
- **Supply-chain verification**: module/package/file three levels + SHA-256 Merkle dirhash, `MODULE.infy.lock` reproducible builds.
- **Honesty**: the README separates implemented / still converging / explicitly not implemented, and does **not** claim "INFY v2.0 conformance" - 100% of the 2300-case suite or no claim.

## 学到的东西 / What I learned

1. **性能报告比性能本身更容易做假**：转译器的「快」大多数时候是基准台的不公平。换中位数、加复现带、做单因素探针，才敢说一句话。
2. **规范先行是最大的杠杆**：先写 spec 再写实现，每一条「规范未定处」都变成需要记录的偏差（`spec-deviations.md`），而不是实现者的暗箱决定。
3. **两套管线互补**：转译到 Rust 拿到的是生态和性能；自己写运行时拿到的是可控和可审计。它们回答的是不同的问题。

1. **Benchmarks lie more easily than performance does**: most "faster" claims from transpilers are unfair baselines. Switching to medians, adding reproducibility bands, and probing one factor at a time is the only way to say anything.
2. **Spec-first is the biggest lever**: write the spec before the implementation, and every "unspecified corner" becomes a recorded deviation (`spec-deviations.md`) instead of an implementer's hidden decision.
3. **The two pipelines complement each other**: transpiling to Rust buys ecosystem and performance; writing your own runtime buys control and auditability. They answer different questions.

## 链接 / Links

- RSupercell (rscel)：https://github.com/YuanShenNiuBi1314/rsupercell-lang
- Infinity Language (INFY)：https://github.com/YuanShenNiuBi1314/infinity-lang

欢迎任何方向的反馈：设计、诊断、基准方法学、规范偏差。批评比 star 更有价值。
Feedback on design, diagnostics, benchmark methodology and spec deviations is very welcome. Critiques are worth more than stars.
