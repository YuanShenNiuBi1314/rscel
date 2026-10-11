//! # rscel-check —— RSupercell 静态检查器（批次 2）
//!
//! 实现 §11 的全部 8 项检查（C1–C8）。
//!
//! ## 模块
//!
//! - [`contracts`]：C1 契约表 + E040/E041/E042/E043/E044/E045/E011/E035；
//! - [`converge`]：C2 收敛（E010 / E014）；
//! - [`ownership`]：C4 所有权 / 借用（E060 / E061 / E062 / E063 / E064）；
//! - [`shadow`]：C5 遮蔽（E070）；
//! - [`reach`]：C6 入口可达性 + E020 + W001；
//! - [`over_closure`]：C7 over 闭包纯函数校验（E080）；
//! - [`extern_rust`]：C8 extern rust 边界校验（E090）；
//! - [`deps`]：C3 `concurrent_limit` 子实例依赖（E050）+ 全局依赖图；
//! - [`topology`]：Cell 拓扑图模型 + §16.4 的文本 / mermaid 调用链路报告；
//! - [`trace`]：§16.5 的干跑模拟（不真正 spawn，只按 CFG 顺序走消息链路）；
//! - [`analysis`]：跨检查共享数据结构（`ContractTable`、`AnalysisResult`）。
//!
//! C3 依赖检查只在 `jump and` 带 `concurrent_limit` 时启用（§7.3 明文），
//! 判据是子 Cell 之间的传递可达 jump 边；父侧 `read with` 下发只读 Arc 是允许的例外。

pub mod analysis;
pub mod contracts;
pub mod converge;
pub mod deps;
pub mod extern_rust;
pub mod over_closure;
pub mod ownership;
pub mod reach;
pub mod shadow;
pub mod topology;
pub mod trace;

use rscel_parser::{Diagnostic, Program};

pub use analysis::{AnalysisResult, CellId, ContractTable};

/// 整体 check 结果。
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// 全部诊断（错误 + 警告）。
    pub diagnostics: Vec<Diagnostic>,
    /// 跨检查分析结果（契约表 + 可达集 + 依赖图等）。
    pub analysis: AnalysisResult,
}

impl CheckOutcome {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == rscel_parser::Severity::Error)
    }

    pub fn has_warnings(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == rscel_parser::Severity::Warning)
    }
}

/// 顶层入口：对 `Program` 做全部 8 项检查。
///
/// 任一错误级诊断 → 调用方应终止流水线（§6）。
pub fn check_program(program: &Program) -> CheckOutcome {
    let mut diags = Vec::new();

    // ---- C6 + E020（先确定入口）----
    let (reach_diags, reachable) = reach::check_reachability(program);
    diags.extend(reach_diags);

    // ---- 契约表构建 + C1 ----
    let contracts = contracts::build_contract_table(program);

    // 重新跑一轮输出契约收集（build_contract_table 未填 cell_outputs，留给 check 输出）
    let mut full_contracts = contracts.clone();
    for cell in program.cells() {
        let outs = contracts::collect_output_keys(cell);
        full_contracts.cell_outputs.insert(cell.name.clone(), outs);
        full_contracts
            .only_dies
            .insert(cell.name.clone(), contracts::cell_only_dies(cell));
    }
    // 回填 jump and / jump recurse 创建的 group 信息 + 每条 jump 边的键集
    contracts::fill_group_members(program, &mut full_contracts);
    contracts::fill_edge_keys(program, &mut full_contracts);

    // C1：jump 边校验
    let contracts_diags = contracts::check_contracts(program, &full_contracts);
    diags.extend(contracts_diags);

    // C1：E042/E043/E044 多包校验（按本 Cell 内创建 group 的 jump 解析槽位）
    for cell in program.cells() {
        contracts::check_multisrc_receive(cell, &full_contracts, &mut diags);
    }

    // ---- C2 收敛 ----
    for cell in program.cells() {
        let conv_diags = converge::check_convergence(cell);
        diags.extend(conv_diags);
    }

    // ---- C4 所有权 ----
    for cell in program.cells() {
        let own_diags = ownership::check_ownership(cell);
        diags.extend(own_diags);
    }

    // ---- C5 遮蔽 ----
    for cell in program.cells() {
        let shadow_diags = shadow::check_shadow(cell);
        diags.extend(shadow_diags);
    }

    // ---- C7 over 闭包 ----
    for cell in program.cells() {
        let over_diags = over_closure::check_over_closures(cell);
        diags.extend(over_diags);
    }

    // ---- C8 extern rust ----
    for cell in program.cells() {
        let ext_diags = extern_rust::check_extern_rust(cell);
        diags.extend(ext_diags);
    }

    // ---- C3 依赖（§7.3：仅带 concurrent_limit 的 jump and 启用）----
    let (dep_diags, depends) = deps::check_concurrent_deps(program);
    diags.extend(dep_diags);

    let analysis = AnalysisResult {
        contracts: full_contracts,
        reachable: reachable.into_iter().collect(),
        depends,
    };

    CheckOutcome { diagnostics: diags, analysis }
}