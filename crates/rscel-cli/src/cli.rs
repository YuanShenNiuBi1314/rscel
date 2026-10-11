//! 手写 CLI 参数解析（零依赖；§4.1 + §4.3）。
//!
//! 子命令 + 简单 flag 解析，**不用 clap**：硬规约是零外部依赖，而且这里的语法
//! 就四个子命令 + 五六个 flag，写死反而比配置一个解析器更好读、更好测。
//!
//! `run` 接收 [`Handlers`]（一组 `fn` 指针）而不是直接调实现：诊断/产物文本
//! 需要能被测试替换或注入（见 `rscel_cli::run_build_with` 的同类口子），
//! 同时 `parse` 本身是纯函数，可以脱离实现单测。

use crate::serve::{EditOptions, DEFAULT_PORT};
use crate::trace_cli::TraceRequest;
use crate::{build_cache::CachePolicy, BuildMode, HELP, VERSION};
use std::path::{Path, PathBuf};

/// 子命令的实现集合（`lib.rs` 里装配）。
pub struct Handlers {
    pub check: fn(&Path) -> i32,
    pub build: fn(&Path, &Path, BuildMode, CachePolicy) -> i32,
    pub graph: fn(&Path, bool) -> i32,
    pub trace: fn(&TraceRequest) -> i32,
    pub edit: fn(&EditOptions) -> i32,
}

pub fn run(args: &[String], h: &Handlers) -> i32 {
    match parse(args) {
        // 字段是 fn 指针，调用要加括号（`h.check(...)` 会被当成方法查找）。
        Ok(Command::Check { file }) => (h.check)(&file),
        Ok(Command::Build { file, output, mode, cache }) => (h.build)(&file, &output, mode, cache),
        Ok(Command::Graph { file, mermaid }) => (h.graph)(&file, mermaid),
        Ok(Command::Trace(req)) => (h.trace)(&req),
        Ok(Command::Edit(opts)) => (h.edit)(&opts),
        Ok(Command::Version) => {
            println!("{VERSION}");
            0
        }
        Ok(Command::Help) => {
            println!("{HELP}");
            0
        }
        Err(msg) => {
            // 参数错误：保持既有行为（`cli_smoke::parse_unknown_subcommand_exits_2`
            // 与 `build_missing_o_flag_exits_2` 钉的是 2，§4.1 的 3 留给 IO / 端口）。
            eprintln!("RSC: {msg}\n\n{HELP}");
            2
        }
    }
}

#[derive(Debug)]
pub enum Command {
    Check { file: PathBuf },
    Build {
        file: PathBuf,
        output: PathBuf,
        mode: BuildMode,
        /// §16.2 增量缓存的开关（`--no-cache` 关掉读与写）。
        cache: CachePolicy,
    },
    /// §16.4：`rscel graph <file> [--mermaid]`
    Graph { file: PathBuf, mermaid: bool },
    /// §16.5：`rscel trace <file> [--max-items N] [--arg v]...`
    Trace(TraceRequest),
    /// docs/05 P1：`rscel edit [file.rscel] [--port 8421]`
    Edit(EditOptions),
    Version,
    Help,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let second = args.get(1).map(String::as_str);

    match second {
        None | Some("--help") | Some("-h") => Ok(Command::Help),
        Some("--version") | Some("-V") => Ok(Command::Version),
        Some("check") => {
            let file = args.get(2).ok_or_else(|| "check 需要 <file.rscel>".to_string())?;
            Ok(Command::Check { file: PathBuf::from(file) })
        }
        Some("build") => {
            let file = args.get(2).ok_or_else(|| "build 需要 <file.rscel>".to_string())?;
            let file = PathBuf::from(file);
            let mut output: Option<PathBuf> = None;
            let mut mode = BuildMode::Release; // §4.1 默认 release
            let mut cache = CachePolicy::Enabled; // §16.2 默认开，`--no-cache` 才关
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "-o" => {
                        let v = args.get(i + 1).ok_or_else(|| "-o 需要 <out>".to_string())?;
                        output = Some(PathBuf::from(v));
                        i += 2;
                    }
                    "--release" => {
                        mode = BuildMode::Release;
                        i += 1;
                    }
                    "--debug" => {
                        mode = BuildMode::Debug;
                        i += 1;
                    }
                    "--no-cache" => {
                        cache = CachePolicy::Disabled;
                        i += 1;
                    }
                    other => return Err(format!("未知参数: {other}")),
                }
            }
            let output = output.ok_or_else(|| "build 需要 -o <out>".to_string())?;
            Ok(Command::Build { file, output, mode, cache })
        }
        Some("graph") => {
            let mut file: Option<PathBuf> = None;
            let mut mermaid = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--mermaid" => mermaid = true,
                    other if other.starts_with('-') => return Err(format!("graph 不认这个 flag: {other}")),
                    other => {
                        if file.is_some() {
                            return Err("graph 只接受一个 <file.rscel>".to_string());
                        }
                        file = Some(PathBuf::from(other));
                    }
                }
                i += 1;
            }
            Ok(Command::Graph { file: file.ok_or_else(|| "graph 需要 <file.rscel>".to_string())?, mermaid })
        }
        Some("trace") => {
            let mut req = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--max-items" => {
                        let v = args.get(i + 1).ok_or_else(|| "--max-items 需要 <N>".to_string())?;
                        let n: usize = v.parse().map_err(|_| format!("--max-items 需要非负整数，收到 {v:?}"))?;
                        // 夹到 1：0 会让「至少走 1 轮 / 记 1 条派生记录」的兜底失效。
                        req.get_or_insert_with(|| TraceRequest::new(PathBuf::new())).max_items = n.max(1);
                        i += 2;
                    }
                    "--arg" => {
                        let v = args.get(i + 1).ok_or_else(|| "--arg 需要 <值>（或 key=值）".to_string())?;
                        req.get_or_insert_with(|| TraceRequest::new(PathBuf::new())).args.push(v.clone());
                        i += 2;
                    }
                    other if other.starts_with('-') => return Err(format!("trace 不认这个 flag: {other}")),
                    other => {
                        let r = req.get_or_insert_with(|| TraceRequest::new(PathBuf::new()));
                        if r.file.as_os_str().is_empty() {
                            r.file = PathBuf::from(other);
                        } else {
                            return Err("trace 只接受一个 <file.rscel>".to_string());
                        }
                        i += 1;
                    }
                }
            }
            let mut req = req.ok_or_else(|| "trace 需要 <file.rscel>".to_string())?;
            if req.file.as_os_str().is_empty() {
                return Err("trace 需要 <file.rscel>".to_string());
            }
            if req.max_items == 0 {
                req.max_items = crate::api::DEFAULT_MAX_ITEMS;
            }
            Ok(Command::Trace(req))
        }
        Some("edit") => {
            let mut opts = EditOptions { file: None, port: DEFAULT_PORT, workdir: None };
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--port" => {
                        let v = args.get(i + 1).ok_or_else(|| "--port 需要 <端口>".to_string())?;
                        // 0 是合法端口号（= 让内核选），留给 Server::start 处理；
                        // 非数字或超 u16 才是参数错误。
                        opts.port = v.parse().map_err(|_| format!("--port 需要 0-65535 的整数，收到 {v:?}"))?;
                        i += 2;
                    }
                    other if other.starts_with('-') => return Err(format!("edit 不认这个 flag: {other}")),
                    other => {
                        if opts.file.is_some() {
                            return Err("edit 只接受一个可选的 <file.rscel>".to_string());
                        }
                        opts.file = Some(PathBuf::from(other));
                        i += 1;
                    }
                }
            }
            Ok(Command::Edit(opts))
        }
        Some(other) => Err(format!("未知子命令: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(args: &[&str]) -> Vec<String> {
        let mut v = vec!["rscel".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn graph_flags_parse() {
        match parse(&a(&["graph", "x.rscel"])) {
            Ok(Command::Graph { file, mermaid }) => {
                assert_eq!(file, PathBuf::from("x.rscel"));
                assert!(!mermaid);
            }
            other => panic!("应为 graph: {other:?}"),
        }
        match parse(&a(&["graph", "--mermaid", "x.rscel"])) {
            Ok(Command::Graph { mermaid, .. }) => assert!(mermaid, "--mermaid 可以在文件名前面"),
            other => panic!("{other:?}"),
        }
        assert!(parse(&a(&["graph"])).is_err(), "缺文件名必须报参数错");
        assert!(parse(&a(&["graph", "x.rscel", "y.rscel"])).is_err(), "两个文件名要拒");
        assert!(parse(&a(&["graph", "--nope", "x.rscel"])).is_err(), "未知 flag 要拒");
    }

    #[test]
    fn trace_flags_parse_and_repeat() {
        match parse(&a(&["trace", "x.rscel", "--max-items", "7", "--arg", "n=5", "--arg", "3"])) {
            Ok(Command::Trace(req)) => {
                assert_eq!(req.file, PathBuf::from("x.rscel"));
                assert_eq!(req.max_items, 7);
                assert_eq!(req.args, vec!["n=5".to_string(), "3".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        // 缺省 max-items = api::DEFAULT_MAX_ITEMS
        match parse(&a(&["trace", "x.rscel"])) {
            Ok(Command::Trace(req)) => assert_eq!(req.max_items, crate::api::DEFAULT_MAX_ITEMS),
            other => panic!("{other:?}"),
        }
        // `--max-items 0` 夹到 1（0 会让兜底变成什么都不做）
        match parse(&a(&["trace", "x.rscel", "--max-items", "0"])) {
            Ok(Command::Trace(req)) => assert_eq!(req.max_items, 1),
            other => panic!("{other:?}"),
        }
        assert!(parse(&a(&["trace", "x.rscel", "--max-items"])).is_err(), "--max-items 缺值要拒");
        assert!(parse(&a(&["trace", "x.rscel", "--max-items", "abc"])).is_err(), "非数字要拒");
        assert!(parse(&a(&["trace", "x.rscel", "--arg"])).is_err(), "--arg 缺值要拒");
    }

    #[test]
    fn edit_file_is_optional_and_port_has_default() {
        match parse(&a(&["edit"])) {
            Ok(Command::Edit(o)) => {
                assert!(o.file.is_none());
                assert_eq!(o.port, DEFAULT_PORT);
            }
            other => panic!("{other:?}"),
        }
        match parse(&a(&["edit", "x.rscel", "--port", "9000"])) {
            Ok(Command::Edit(o)) => {
                assert_eq!(o.file, Some(PathBuf::from("x.rscel")));
                assert_eq!(o.port, 9000);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&a(&["edit", "--port"])).is_err());
        assert!(parse(&a(&["edit", "--port", "70000"])).is_err(), "端口超 u16 要拒");
        assert!(parse(&a(&["edit", "a.rscel", "b.rscel"])).is_err(), "两个文件要拒");
        assert!(parse(&a(&["edit", "--nope"])).is_err());
    }

    #[test]
    fn existing_subcommands_still_parse() {
        assert!(matches!(parse(&a(&["check", "x.rscel"])), Ok(Command::Check { .. })));
        assert!(matches!(parse(&a(&["build", "x.rscel", "-o", "app", "--debug"])), Ok(Command::Build { .. })));
        assert!(matches!(parse(&a(&["--version"])), Ok(Command::Version)));
        assert!(matches!(parse(&a(&[])), Ok(Command::Help)));
        assert!(parse(&a(&["frobnicate"])).is_err());
    }

    /// §16.2：增量缓存**默认开**，`--no-cache` 是唯一开关；而且它得能和 mode flag 任意先后。
    #[test]
    fn build_cache_policy_defaults_on_and_no_cache_turns_it_off() {
        match parse(&a(&["build", "x.rscel", "-o", "app"])) {
            Ok(Command::Build { cache, mode, .. }) => {
                assert_eq!(cache, CachePolicy::Enabled, "默认就该用缓存");
                assert_eq!(mode, BuildMode::Release, "§4.1 默认 release 不能被顺手改掉");
            }
            other => panic!("{other:?}"),
        }
        match parse(&a(&["build", "x.rscel", "-o", "app", "--no-cache"])) {
            Ok(Command::Build { cache, .. }) => assert_eq!(cache, CachePolicy::Disabled),
            other => panic!("{other:?}"),
        }
        // `--no-cache` 放在 `-o` 前面也要认（脚本里两种写法都会出现）。
        match parse(&a(&["build", "x.rscel", "--no-cache", "-o", "app", "--debug"])) {
            Ok(Command::Build { cache, mode, output, .. }) => {
                assert_eq!(cache, CachePolicy::Disabled);
                assert_eq!(mode, BuildMode::Debug);
                assert_eq!(output, PathBuf::from("app"));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&a(&["build", "x.rscel", "-o", "app", "--no-caches"])).is_err(), "拼错的 flag 要拒而不是当成缓存开关");
    }

    #[test]
    fn run_dispatches_to_every_handler() {
        // 每个假 handler 返回一个不同的码：证明五个子命令真的各自接到了自己的实现，
        // 而不是全都落进 check（那种「路由写错但测试照样绿」的坑最怕只用 `== 0`）。
        let h = Handlers {
            check: |_| 11,
            build: |_, _, _, _| 12,
            graph: |_, _,| 13,
            trace: |_| 14,
            edit: |_| 15,
        };
        assert_eq!(run(&a(&["check", "x"]), &h), 11);
        assert_eq!(run(&a(&["build", "x", "-o", "y"]), &h), 12);
        assert_eq!(run(&a(&["graph", "x"]), &h), 13);
        assert_eq!(run(&a(&["trace", "x"]), &h), 14);
        assert_eq!(run(&a(&["edit"]), &h), 15);
        assert_eq!(run(&a(&["frobnicate"]), &h), 2, "未知子命令按既有约定退 2");
    }
}
