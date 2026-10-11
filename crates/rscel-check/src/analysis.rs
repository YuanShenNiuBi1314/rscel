//! 跨检查共享的数据结构（§9.5 / §11）。
//!
//! 主要暴露：
//! - [`CellId`]：Cell 标识（用 `Ident` = String）。
//! - [`ContractTable`]：契约表（§9.5 / §11.1）。
//! - [`AnalysisResult`]：check 整体对外接口。

use std::collections::{BTreeSet, HashMap};

use rscel_parser::{Ident, Program};

use crate::contracts::build_contract_table;

/// 本地补充的检查错误码（diag.rs 已有 E010/E011/E014/E020/E040–E044/E050/E060/E070/
/// E080/E090/W001；本模块补 §6 扩展与所有权子码）。
///
/// **约束**：parser 的 `diag::code` 模块已冻结（§3 文档）；此处只声明 rscel-check 内部
/// 使用的补充码，按文档要求对接。
pub mod codes {
    // §6 扩展码
    pub const E034: u16 = 34; // cell 后缺 receive（与 parser 的 code::E034 同值，供 check 复用）
    pub const E035: u16 = 35; // jump sync 未创建/已 sync
    pub const E036: u16 = 36; // `jump recurse` 的 return 别名不是 Option<T> 声明
    pub const E037: u16 = 37; // `read with` 变量的接收类型不是 Arc<T>
    pub const E038: u16 = 38; // 接收/传参类型无法确定
    pub const E039: u16 = 39; // concurrent_limit 表达式无法 usize
    pub const E045: u16 = 45; // over 派生实例数与多包 receive 静态槽位不符
    pub const E046: u16 = 46; // 出口式 jump（plain）上挂了 return(...) / group / over / concurrent_limit
    pub const E047: u16 = 47; // 同一个 recurse 的返回值既用 return(...) 又用多包 receive 读取
    pub const W002: u16 = 2;   // over 集合运行期未知

    // §11.4 所有权子码
    pub const E061: u16 = 61;
    pub const E062: u16 = 62;
    pub const E063: u16 = 63;
    pub const E064: u16 = 64;
}

/// Cell 标识（直接复用 `Ident`；不引入新类型以便 [`Program::cell`] 的 `&str` 查询兼容）。
pub type CellId = Ident;

/// 一个 Cell 的输入键（裸键形式；包限定键见 [`crate::contracts::resolve_receive_keys`]）。
pub type InputKey = Ident;

/// 一个 Cell 的输出键（exit go with 收集的键集）。
pub type OutputKey = Ident;

/// 一条 jump 边的传入键集（按 key 推导，§9.1）。
#[derive(Debug, Clone, Default)]
pub struct EdgeKeys {
    /// 边使用的键名（顺序与 jump.args 对齐）。
    pub keys: Vec<Ident>,
    /// `kind == Loop` 时该字段被忽略。
    pub kind_is_loop: bool,
}

/// 全局契约表（§9.5）。
#[derive(Debug, Clone, Default)]
pub struct ContractTable {
    /// cell → 输入键（裸键形式，包限定键独立保留）。
    pub cell_inputs: HashMap<CellId, Vec<InputKey>>,
    /// cell → 输出键（exit 键集并集）。
    pub cell_outputs: HashMap<CellId, Vec<OutputKey>>,
    /// 每条 jump 边（按 cell + stmt_index 标识）的传入键集。
    pub edge_keys: HashMap<(CellId, usize), EdgeKeys>,
    /// `jump and` 创建的 group → 子 Cell 名集合（用于 E043/E044/E050）。
    pub group_members: HashMap<Ident, Vec<CellId>>,
    /// Cell 是否"只可能 die"（无任何 exit go with）——多包 receive 缺键时用。
    pub only_dies: HashMap<CellId, bool>,
}

impl ContractTable {
    /// 仅构造空表（用于 gen 层；check 阶段用 [`build_contract_table`]）。
    pub fn empty() -> Self {
        ContractTable::default()
    }
}

/// 跨检查分析结果。
#[derive(Debug, Clone, Default)]
pub struct AnalysisResult {
    /// 契约表（§11.1）。
    pub contracts: ContractTable,
    /// 入口可达的 Cell 集合（C6）。
    pub reachable: BTreeSet<CellId>,
    /// 全局依赖图（jump and 子实例间；用于 C3）。
    pub depends: HashMap<CellId, BTreeSet<CellId>>,
}

impl AnalysisResult {
    pub fn new() -> Self {
        AnalysisResult::default()
    }

    /// 仅做契约表构建（供其他模块按需复用）。
    pub fn contracts(program: &Program) -> ContractTable {
        build_contract_table(program)
    }
}