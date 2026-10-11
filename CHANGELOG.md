# Changelog

本项目遵循语义化版本；`rscel` 的**发布版本**（本节）与**语言规范修订号**
（`docs/02` 标题里的 v0.3）是两个独立编号，不要混用。

## 0.1.0 — 2026-10-11（首个公开版本）

首个可公开使用的版本。语言核心与编译器管线端到端可用，工具链与性能基准都可复现。

### 已达成（有测试与实测支撑）
- 语言核心：`cell` / `receive`（含 `as` 别名与多包 `g.W<n>.key`）/ `exit go with` /
  `die` / `out` / `entry` / `extern rust`（顶层块、Cell 内块、含 DSL 语句的 Rust 块）。
- 5 种 jump 形态全部有语义实现：`jump`（出口转移）、`jump loop`（原地循环）、
  `jump recurse`（子实例 + `return(key -> 别名)`，按 D3 默认内联 await）、
  `jump and`（并行派生，含多目标与 `over |> filter/map/...` + `concurrent_limit` 限流）、
  `jump sync`（等待组 + S4 late binding 解包）。
- `read with` 生成 `Arc::clone`，非 Arc 变量按 §8.1 一次性 `Arc::new` 提升（只共享一份数据）。
- 调用约定：Cell **默认同步 fn**（零 future 状态机），仅透传出现 `.await` 才生成 `async fn`；
  生成物是单个 `.rs` 文件、**零外部依赖**，`rustc` 直接可编译。
- 静态检查 C1–C8 全实现，含契约码 E010–E014、E020、E030–E037、E039–E047、E050、
  E060–E064、E070–E090 与 W001/W002/W060；错误全部带 DSL 行列与修复建议。
- 工具链：`rscel check` / `build [--release|--debug]` / `graph [--mermaid]` /
  `trace [--max-items]` / `edit [--port]`（本地 127.0.0.1 前端编辑器，集成 Cell 拓扑图、
  节点契约详情、trace 时间线回放、一键构建运行、`.rs`/`.map.json` 查看）；
  `.map.json` 源映射 + rustc 错误反映射（RSC-E900 / RSC-E901）；构建增量缓存。
- 测试：`cargo test --workspace` 436 项全绿；`clippy --all-targets -D warnings` 零告警；
  无 `todo!` / `unimplemented!` / `#[ignore]` / 静默降级。

### 已知限制（公开版本必须如实声明）
- **性能**：空闲机器、20× 工作量、21 轮交错复测下，5 个场景对手写基线落在
  **96%–104%**（`loop_sum` 100.39% / `loop_sum_opaque` 96.06% / `pipeline_and` 104.05% /
  `read_share` 107.05% / `async_step` 98.94%）。复现带 ±1.4%~±7.3% 大于门禁余量，
  因此本轮**不给"是否达标"的结论**，只给区间。维护者已裁定：≥97% 为可接受的转译代价，
  99% 是持续优化方向而非硬门槛（`docs/04` 暂不随发布物分发，等空闲窗重测达标后再公开）。
- D2 类型推导对 `extern rust` 顶层 `fn` 的返回类型仍无推导来源，此类样例需要显式
  `as <类型>` 消歧（见 `docs/05` P0）。
- `rscel trace` 是**干跑模拟**，`when` 条件只解释字面量与简单标识符比较，
  不保证与真实执行的数值一致（§16.5 的明示范围）。
- Windows 为主验证平台；MSRV 声明 1.75，但公开 CI 尚未建立。
