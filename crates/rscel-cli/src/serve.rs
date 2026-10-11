//! `rscel edit` 的本地 HTTP 服务（§16 + docs/05 P1 的安全边界）。
//!
//! ## 为什么是 std::net + 每连接一条线程
//!
//! 硬规约是零外部依赖，所以没有 hyper/axum/tokio 可用。这个服务的量级是
//! 「本机一个人开着编辑器」：每连接 `thread::spawn` + `Connection: close`
//! 足够，而且没有 keep-alive 状态机 / 连接池要维护 —— 出错面最小。
//!
//! ## 四条必须守住的安全边界（docs/05 P1 原文要求 + 本批补齐的第 4 条）
//!
//! 1. **只绑 `127.0.0.1`**（`Ipv4Addr::LOCALHOST`），不是 `0.0.0.0`。这个服务能读写
//!    文件、能起 rustc 子进程，暴露到局域网等于把开发机交给同网段任何人。
//! 2. **请求体上限 4 MiB、请求头上限 64 KiB**，超限回 400，不 OOM、不 panic。
//! 3. **任何写文件的路径都要过 [`api::save_target`] 的白名单**（浏览器里任意页面
//!    都能对 127.0.0.1 发 POST，所以这条不是可选项）。
//! 4. **来源校验**：`Host` 必须是回环名，`Origin`（存在时）必须是回环来源，
//!    响应一律带 `X-Frame-Options: DENY`。第 1 条只挡住「别人直连」，挡不住浏览器：
//!    任何网页都能对 `127.0.0.1:8421` 发**无预检**的表单 POST
//!    （`application/x-www-form-urlencoded` 在 CORS 简单请求白名单里），而
//!    `/api/build` 会 rustc 编译并**运行请求方提交的代码**（`extern rust{}` 是任意
//!    Rust），`/api/save` 往磁盘写文件 —— 于是访问一个恶意页面就等于交出本机
//!    代码执行。三个方向各堵一处：非回环 `Origin` 挡跨站请求，非回环 `Host` 挡
//!    DNS rebinding（域名 TTL 过期后解析到 127.0.0.1，浏览器仍发攻击者域名的 Host），
//!    `DENY` 挡把编辑器嵌进别人 iframe 的**同源**形状（那时 Origin/Host 全都合法）。
//!
//! ## 单连接出错不能带崩服务
//!
//! accept 循环只负责收连接并派发；解析、路由、渲染都在那条连接的线程里做，
//! 路由外面还套了 `catch_unwind`：某个端点真 panic 时回 500，服务与其它连接照常。
//! accept 自身的临时错误（`WouldBlock` / `Interrupted` / 平台差异的
//! `NotConnected`）只多睡一轮，**绝不退出循环** —— 否则一次网络抖动就把编辑器打死。

use crate::api::{self, Session};
use crate::json::{field, form_decode, form_get, Json};
use crate::BuildMode;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// 默认端口（docs/05 P1 与任务书里写的 8421）。
pub const DEFAULT_PORT: u16 = 8421;
/// 请求体上限：4 MiB。源码编辑器的单请求不该更大。
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// 请求头（含请求行）上限。
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// 单连接读超时：客户端连上不发数据不会把线程一直挂着。
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 非阻塞 accept 的轮询间隔 —— 停止标志最多这么久就生效。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 403 的固定提示：被来源边界拒掉的人**最可能**是自己（用户换了访问方式），
/// 所以消息里要说清合法的访问形状，而不是只复读一遍规则。
const LOCAL_ORIGIN_HINT: &str = "请用 http://127.0.0.1:<端口> 打开编辑器页面；命令行客户端请带 Host: 127.0.0.1";

/// `include_str!` 而不是运行时读文件：`rscel edit` 是单二进制交付，
/// 页面必须跟着二进制走，否则换个工作目录起服务就 404。
const EDITOR_HTML: &str = include_str!("../assets/editor.html");

/// `rscel edit` 的参数。
#[derive(Clone, Debug)]
pub struct EditOptions {
    /// 启动时打开的 `.rscel` 文件（同时是 `/api/save` 的白名单目标）。
    pub file: Option<PathBuf>,
    /// 监听端口（只作用于 127.0.0.1）。`0` = 让内核选端口（测试用）。
    pub port: u16,
    /// `/api/save` 的根目录。`None` = 进程当前工作目录（真实用法）；
    /// 测试注入临时目录，这样「允许写」与「拒绝越界」两条分支都能真跑到，
    /// 而且不会往仓库里落文件。
    pub workdir: Option<PathBuf>,
}

impl Default for EditOptions {
    fn default() -> Self {
        EditOptions { file: None, port: DEFAULT_PORT, workdir: None }
    }
}

/// 跑 `rscel edit`：绑定 → 打印地址 → 阻塞服务到进程结束。
///
/// 退出码（§4.1）：绑定失败（端口被占用 / 权限不足）/ 文件读不了 = IO 错误 → **3**，
/// 并且把「怎么腾出端口」写进消息，而不是一句 `Address already in use`。
pub fn run_edit(opts: &EditOptions) -> i32 {
    let mut server = match Server::start(opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("RSC: {}", bind_hint(&e, opts.port));
            return 3;
        }
    };
    println!("rscel 编辑器已就绪: http://127.0.0.1:{}  (Ctrl-C 退出)", server.port());
    {
        let session = server.session();
        match &session.file {
            Some(p) => println!(
                "  文档: {}   可写范围: 该文件 + {} 下的 .rscel",
                p.display(),
                session.workdir.display()
            ),
            None => println!("  文档: <内置样例>   可写范围: {} 下的 .rscel", session.workdir.display()),
        }
    }
    server.block();
    0
}

/// 把 io::Error 翻译成**可操作**的提示（任务书对端口占用这条的硬要求）。
fn bind_hint(e: &std::io::Error, port: u16) -> String {
    if e.kind() == ErrorKind::AddrInUse {
        format!(
            "端口 {port} 已被占用（{e}）。两种解法：\n  1) 换端口：rscel edit --port {}\n  2) 先找出占用者再结束它：\n     Windows: netstat -ano | findstr :{port}   然后  taskkill /PID <pid> /F\n     Unix   : lsof -i :{port}  或  ss -ltnp | grep :{port}",
            port + 1
        )
    } else {
        format!("绑定 127.0.0.1:{port} 失败: {e}")
    }
}

// ---------------------------------------------------------------- 服务器

/// 已经起来的服务。`Drop` 置停止标志，accept 线程随后退出
/// （测试跑完断言就 drop，不留后台进程）。
pub struct Server {
    addr: SocketAddr,
    session: Arc<Session>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    /// 绑定并起 accept 线程。
    pub fn start(opts: &EditOptions) -> std::io::Result<Server> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, opts.port));
        let listener = TcpListener::bind(addr)?;
        // 非阻塞 accept 的唯一目的：让停止标志能在一个 POLL_INTERVAL 内生效，
        // 不需要「往自己端口再连一次」把主线程从 accept 里叫醒的花招。
        listener.set_nonblocking(true)?;
        let bound = listener.local_addr()?;
        let session = Arc::new(Session::new(opts.file.clone(), opts.workdir.clone())?);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let flag = Arc::clone(&stop);
            let state = Arc::clone(&session);
            thread::spawn(move || accept_loop(listener, state, flag))
        };
        Ok(Server { addr: bound, session, stop, worker: Some(worker) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// 会话（测试用得上：确认保存根目录、读回初始文档）。
    pub fn session(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    /// 只置停止标志（不 join）：可以在任何地方调。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 阻塞到 accept 线程结束 —— **真实 `rscel edit` 停在这里**。
    ///
    /// 为什么不设停止条件：零依赖下没有信号处理器（`ctrl-c` crate 是要禁的），
    /// 所以「一直服务，直到进程被 Ctrl-C / kill 终止」才是诚实的形状。
    /// join 而不是自旋睡眠：accept 线程万一自己结束了（例如平台差异导致的
    /// 绑定失效），命令行也该跟着返回，而不是变成一个「还在跑但没人接客」的僵尸。
    pub fn block(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }

    /// 置停止标志并等 accept 线程退出（测试 / 受控关停用）。
    pub fn wait(&mut self) {
        self.stop();
        self.block();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // 只置标志、**不 join**：drop 可能发生在 panic 展开路径上，
        // 那里 join 会把「一个端点炸了」升级成「测试进程挂住」。
        self.stop();
    }
}

fn accept_loop(listener: TcpListener, session: Arc<Session>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let state = Arc::clone(&session);
                // 每连接一条线程，**不 join**：join 就把并发连接退化成串行，
                // 一个慢客户端（或者只连上不发数据的端口扫描）会卡住整个 accept。
                // spawn 失败（系统线程耗尽）只丢这条连接，不该带走服务，所以忽略返回值。
                let _ = thread::Builder::new()
                    .name("rscel-edit-conn".to_string())
                    .spawn(move || {
                        if let Err(e) = serve_connection(stream, &state) {
                            // 对端提前断开是常态（浏览器预连接、curl -v 探活都会这样），
                            // 只留一行，不打断服务也不刷屏。
                            eprintln!("rscel edit: 连接结束: {e}");
                        }
                    });
            }
            Err(e) => {
                // WouldBlock 是预期路径（非阻塞 accept 每 POLL_INTERVAL 问一次）；
                // 其余错误同样只睡一轮，绝不 break。
                let _ = e;
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

// ---------------------------------------------------------------- 连接处理

/// 一条连接的完整生命周期。返回 `Err` 只表示 socket 本身坏了（对端提前关闭等），
/// 调用方忽略即可 —— 服务必须继续。
fn serve_connection(mut stream: TcpStream, session: &Session) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(msg) => {
            // 畸形请求统一 400（JSON 里带理由），绝不 unwrap / 不 panic。
            let body = Json::object(vec![field("error", Json::text(&msg))]).to_pretty();
            return respond(&mut stream, 400, "application/json; charset=utf-8", body.as_bytes());
        }
    };
    // 来源边界在路由**之前**：被拒的请求不该碰到任何端点的副作用（写文件 / 起 rustc）。
    // 放在解析之后是为了让畸形请求继续拿到 400（那是"请求写错了"，不是"你不许用"）。
    let resp = match guard_local(&req) {
        Err(msg) => Response::json(
            403,
            Json::object(vec![field("error", Json::text(&msg)), field("hint", Json::text(LOCAL_ORIGIN_HINT))]),
        ),
        Ok(()) => route_with_isolation(&req, session),
    };
    respond(&mut stream, resp.status, &resp.content_type, &resp.body)
}

/// 路由 + panic 隔离：单条连接的 panic 只回 500，不带崩服务（`catch_unwind` 在这里）。
fn route_with_isolation(req: &Request, session: &Session) -> Response {
    match panic::catch_unwind(AssertUnwindSafe(|| route(req, session))) {
        Ok(r) => r,
        Err(_) => Response::text(500, "RSC: 端点处理时 panic（已隔离，服务继续运行）\n"),
    }
}

struct Response {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, v: Json) -> Response {
        Response { status, content_type: "application/json; charset=utf-8".to_string(), body: v.to_pretty().into_bytes() }
    }

    fn text(status: u16, s: &str) -> Response {
        Response { status, content_type: "text/plain; charset=utf-8".to_string(), body: s.as_bytes().to_vec() }
    }

    fn error(msg: impl Into<String>) -> Response {
        Response::json(400, Json::object(vec![field("error", Json::text(&msg.into()))]))
    }
}

/// 一个解析完的请求。
///
/// 头部里只留**来源边界用得到的两个**（`host` / `origin`，见 [`guard_local`]），
/// 其余解析完就丢：留着没人读的字段，下次改路由时就会有人以为它能用。
/// `Content-Length` 同理 —— 它在 [`read_request`] 内部消费掉了。
#[derive(Clone, Debug)]
struct Request {
    method: String,
    /// 去掉 query 的路径。
    path: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
    /// `Host:` 头的值；HTTP/1.1 必填，缺失按**非法来源**处理。
    host: Option<String>,
    /// `Origin:` 头的值。浏览器只在跨源/POST 时发，所以 `None` 是合法形状
    /// （curl、同源 GET），而**存在但非回环**一定是别人家的页面。
    origin: Option<String>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    // `take(MAX_HEADER_BYTES)`：没有换行的恶意请求行读到上限就变 EOF，
    // 而不是先把内存吃满才想起来拒绝。
    let cloned = stream.try_clone().map_err(|e| format!("clone socket 失败: {e}"))?;
    let mut reader = BufReader::new(cloned.take(MAX_HEADER_BYTES as u64));

    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|e| format!("读请求行失败: {e}"))? == 0 {
        return Err("连接被关闭，没有收到请求行".to_string());
    }
    let request_line = line.trim_end_matches(['\r', '\n']).to_string();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("");
    if method.is_empty() || target.is_empty() {
        return Err(format!("请求行不完整: {request_line:?}"));
    }
    if !version.starts_with("HTTP/") {
        return Err("请求行缺少 HTTP 版本".to_string());
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).map_err(|e| format!("读请求头失败: {e}"))?;
        total += n;
        if n == 0 {
            // 头部没写完就 EOF（只发了请求行）：缺头就缺头，不 hang 也不 panic。
            return Err("请求头缺失（读到空行之前连接就结束了）".to_string());
        }
        if total > MAX_HEADER_BYTES {
            return Err(format!("请求头超过 {MAX_HEADER_BYTES} 字节上限"));
        }
        let trimmed = h.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let (k, v) = trimmed.split_once(':').ok_or_else(|| format!("请求头行缺少冒号: {trimmed:?}"))?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }

    let mut body = Vec::new();
    let host = header_value(&headers, "host");
    let origin = header_value(&headers, "origin");
    if method == "POST" {
        let len_str = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map(|(_, v)| v.clone())
            .ok_or_else(|| "POST 缺少 Content-Length（无法确定请求体长度）".to_string())?;
        let len: usize = len_str.trim().parse().map_err(|_| format!("Content-Length 非法: {len_str:?}"))?;
        if len > MAX_BODY_BYTES {
            return Err(format!("请求体 {len} 字节，超过 {MAX_BODY_BYTES} 字节上限"));
        }
        // 4 MiB 一次性分配是有意的：上限已经卡死，与其做流式拼接的多状态机，
        // 不如一次到位 —— 但要夹在 `MAX_BODY_BYTES` 内，不接受声明值 > 上限。
        body = vec![0u8; len];
        reader.read_exact(&mut body).map_err(|e| format!("读请求体失败（声明 {len} 字节）: {e}"))?;
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), form_decode(q).map_err(|e| format!("query 解码失败: {e}"))?),
        None => (target.clone(), Vec::new()),
    };
    if path.contains('\0') {
        return Err("路径含 NUL 字符".to_string());
    }
    Ok(Request { method, path, query, body, host, origin })
}

/// 取头部值（`headers` 里的键已经小写化）。
fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

// ---------------------------------------------------------------- 来源边界

/// 路由**之前**的来源校验：不合格回 403，端点一个都不碰
/// （所以被拒的请求既不写文件，也不起 rustc —— 这是集成测试里用构建计数器钉住的事实）。
///
/// 两个头各自挡一种形状，缺一不可：
/// - `Host` 非回环 → **DNS rebinding**：攻击者的域名 TTL 过期后解析成 `127.0.0.1`，
///   浏览器把「访问 evil.example」的流量真的打到本机服务上，但 `Host` 仍是 evil.example。
///   只绑回环挡不住这一种，因为只有浏览器自己知道它连的是谁。
/// - `Origin` 非回环（含 `null`）→ 别人家的页面发的跨站请求。`null` 也拒：
///   它是 `file://` 页面和沙箱 iframe 的 Origin，而这两种页面**本来就读不到**
///   我们的响应（没有 CORS），放行只会留下「能触发副作用、拿不到结果」的攻击面。
/// - 两个头都合法但请求来自 iframe：那与 API 同源，头层面分辨不出来，
///   由 [`respond`] 里的 `X-Frame-Options: DENY` 拒掉内嵌。
fn guard_local(req: &Request) -> Result<(), String> {
    let host = req.host.as_deref().ok_or_else(|| {
        "缺少 Host 头：HTTP/1.1 必填，本服务只认明确指向回环地址的请求".to_string()
    })?;
    if !is_loopback_host(host_of_authority(host)) {
        return Err(format!(
            "Host `{host}` 不是回环地址：本服务只接受 127.0.0.1 / localhost / [::1]（防 DNS rebinding）"
        ));
    }
    if let Some(origin) = &req.origin {
        let loopback = origin_host(origin).map(is_loopback_host).unwrap_or(false);
        if !loopback {
            return Err(format!(
                "Origin `{origin}` 不是本机来源：跨站页面不能驱动本服务（/api/save 会写文件，/api/build 会编译并运行提交的代码）"
            ));
        }
    }
    Ok(())
}

/// 从 `Host` 这类 authority 里取出主机名，去掉端口、保留 IPv6 的括号内容。
///
/// 端口部分只在「恰好一个冒号且冒号后面全是数字」时才剥：`::1`（裸 IPv6）有两个
/// 以上冒号，`127.0.0.1:abc`（畸形）后缀不是数字 —— 两种都按整串处理，
/// 于是必然落进白名单之外，而不是被误当成合法主机。
fn host_of_authority(raw: &str) -> &str {
    let s = raw.trim();
    if let Some(bracket_end) = s.find(']') {
        // `[::1]:8421` → `[::1]`
        return &s[..=bracket_end];
    }
    match s.rfind(':') {
        // 冒号后面全是数字、且前面没有别的冒号 → 那才是端口分隔符。
        Some(i) if s[i + 1..].bytes().all(|b| b.is_ascii_digit()) && !s[..i].contains(':') => &s[..i],
        _ => s,
    }
}

/// `Origin` 头 → 主机名。没有 `scheme://` 的一律 `None`（`null` 就是这么被拒的）。
fn origin_host(origin: &str) -> Option<&str> {
    let (_, rest) = origin.split_once("://")?;
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or("");
    Some(host_of_authority(authority))
}

/// 主机名是否是回环。判断按**整串精确匹配**，不做前缀/包含比较：
/// `127.0.0.1.evil.example`、`localhost.evil.example`、`evil.localhost` 用
/// `starts_with` / `contains` 都会被放行，而那正是这类白名单最常见的写法错误。
fn is_loopback_host(raw: &str) -> bool {
    let h = raw.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if h == "localhost" || h == "::1" {
        return true;
    }
    // `127.0.0.0/8` 整段都是回环（Windows 与 Linux 都把 127.x 路由回本机），
    // 所以用户手打 `127.0.0.2:8421` 也算本机；但必须**逐段**是合法八位组。
    let mut parts = h.split('.');
    let (Some(a), Some(b), Some(c), Some(d), None) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    a == "127" && [b, c, d].iter().all(|p| is_octet(p))
}

/// 合法 IPv4 八位组：1–3 位数字且 ≤ 255（`0` 允许，`127.0.0.0/8` 的定义如此）。
fn is_octet(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 3
        && part.bytes().all(|b| b.is_ascii_digit())
        && part.parse::<u16>().map(|v| v <= 255).unwrap_or(false)
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    // **每个响应都带 Content-Length**：缺它前端的 fetch 会一直等流结束
    // （我们回的是 Connection: close，浏览器要等 socket 关掉才认 body 完整）。
    //
    // `X-Frame-Options: DENY` 给**所有**响应而不是只给页面：来源边界靠 `Origin`/`Host`
    // 两个头，而「别人家的页面把编辑器嵌成 iframe」时那两个头全都是同源的合法值 ——
    // 只有拒绝内嵌才堵得住这条路（详见模块头的第 4 条边界）。
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\n\
         Connection: close\r\nCache-Control: no-store\r\nX-Frame-Options: DENY\r\n\r\n",
        len = body.len(),
        reason = reason_phrase(status),
        ctype = content_type,
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "rscel",
    }
}

// ---------------------------------------------------------------- 路由

/// 路由与响应严格照 `assets/editor.html` 文件头注释里的契约。
fn route(req: &Request, session: &Session) -> Response {
    let path = if req.path == "/index.html" { "/" } else { req.path.as_str() };
    match (req.method.as_str(), path) {
        ("GET", "/") => Response {
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            body: EDITOR_HTML.as_bytes().to_vec(),
        },
        ("GET", "/api/initial") => Response::json(200, api::initial_json(session)),
        ("GET", "/api/source") => {
            let kind = form_get(&req.query, "kind").unwrap_or("rs");
            match api::artifact_text(session, kind) {
                Some(text) => Response {
                    status: 200,
                    // .rs / .map.json 都按纯文本给：前端是 `<pre>` + textContent。
                    // 声明成 application/json 反而会让别的工具试图解析它。
                    content_type: "text/plain; charset=utf-8".to_string(),
                    body: text.into_bytes(),
                },
                None => Response::json(
                    404,
                    Json::object(vec![field("error", Json::text("还没有构建产物：先 POST /api/build 再来读"))]),
                ),
            }
        }
        ("POST", "/api/check") => with_form(req, |pairs| {
            let source = require(pairs, "source")?;
            Ok(Response::json(200, api::check_json(source)))
        }),
        ("POST", "/api/graph") => with_form(req, |pairs| {
            let source = require(pairs, "source")?;
            Ok(Response::json(200, api::graph_json(source)))
        }),
        ("POST", "/api/trace") => with_form(req, |pairs| {
            let source = require(pairs, "source")?;
            let max_items = opt_usize(pairs, "maxItems")?;
            Ok(Response::json(200, api::trace_json(source, max_items)))
        }),
        ("POST", "/api/build") => with_form(req, |pairs| {
            let source = require(pairs, "source")?;
            let mode = match form_get(pairs, "mode") {
                None | Some("release") => BuildMode::Release,
                Some("debug") => BuildMode::Debug,
                Some(other) => return Err(format!("未知 mode `{other}`；可用值 release|debug")),
            };
            Ok(Response::json(200, api::build_json(source, mode, session)))
        }),
        ("POST", "/api/save") => with_form(req, |pairs| {
            let source = require(pairs, "source")?;
            let file = form_get(pairs, "file").unwrap_or("");
            Ok(match api::save_json(source, file, session) {
                Ok(v) => Response::json(200, v),
                Err(msg) => Response::error(msg),
            })
        }),
        // 已知 API 路径但方法不对：405 比 404 诚实（路径确实是我们的）。
        (_, p) if p.starts_with("/api/") => Response::json(
            405,
            Json::object(vec![
                field("error", Json::text(&format!("`{p}` 不接受 {} 方法", req.method))),
                field("path", Json::text(p)),
            ]),
        ),
        _ => Response::json(
            404,
            Json::object(vec![
                field("error", Json::text("not found")),
                field("path", Json::text(&req.path)),
                field(
                    "hint",
                    Json::text(
                        "可用: GET / · GET /api/initial · GET /api/source?kind=rs|map|stdout · POST /api/check|graph|trace|build|save",
                    ),
                ),
            ]),
        ),
    }
}

/// 把「解 body 表单 → 跑 handler → 失败出 400」这条固定流程收在一个地方。
fn with_form<F>(req: &Request, f: F) -> Response
where
    F: FnOnce(&[(String, String)]) -> Result<Response, String>,
{
    let text = match std::str::from_utf8(&req.body) {
        Ok(t) => t,
        Err(e) => return Response::error(format!("请求体不是合法 UTF-8: {e}")),
    };
    let pairs = match form_decode(text) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("表单解码失败: {e}")),
    };
    match f(&pairs) {
        Ok(r) => r,
        Err(msg) => Response::error(msg),
    }
}

fn require<'a>(pairs: &'a [(String, String)], key: &str) -> Result<&'a str, String> {
    form_get(pairs, key).ok_or_else(|| format!("表单缺少字段 `{key}`"))
}

/// 可选的整数上限字段；缺省或空串 → 用 api 的默认值，给了但非数字 → 400。
fn opt_usize(pairs: &[(String, String)], key: &str) -> Result<usize, String> {
    match form_get(pairs, key) {
        None => Ok(api::DEFAULT_MAX_ITEMS),
        Some(raw) => {
            let t = raw.trim();
            if t.is_empty() {
                return Ok(api::DEFAULT_MAX_ITEMS);
            }
            let n: usize = t.parse().map_err(|_| format!("`{key}` 需要非负整数，收到 {raw:?}"))?;
            // 夹到 1：`maxItems=0` 会让「至少走 1 轮」的兜底变成什么都不做（静默降级）。
            Ok(n.max(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(host: Option<&str>, origin: Option<&str>) -> Request {
        Request {
            method: "POST".to_string(),
            path: "/api/save".to_string(),
            query: Vec::new(),
            body: Vec::new(),
            host: host.map(str::to_string),
            origin: origin.map(str::to_string),
        }
    }

    #[test]
    fn authority_splits_port_only_when_that_is_what_it_is() {
        assert_eq!(host_of_authority("127.0.0.1:8421"), "127.0.0.1");
        assert_eq!(host_of_authority("evil.example:8421"), "evil.example");
        assert_eq!(host_of_authority("[::1]:8421"), "[::1]");
        assert_eq!(host_of_authority("127.0.0.1"), "127.0.0.1");
        // 裸 IPv6 的冒号不是端口分隔符（整串留给白名单去认 `::1`）。
        assert_eq!(host_of_authority("::1"), "::1");
        // 端口位不是数字 → 不剥，于是整串必然落在白名单外。
        assert_eq!(host_of_authority("127.0.0.1:abc"), "127.0.0.1:abc");
    }

    #[test]
    fn loopback_matching_is_exact_not_prefix_based() {
        for ok in ["127.0.0.1", "127.0.0.42", "127.255.255.254", "LOCALHOST", "[::1]", "::1"] {
            assert!(is_loopback_host(host_of_authority(ok)), "{ok} 应当算回环");
        }
        for bad in [
            "0.0.0.0",
            "192.168.1.5",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "evil-localhost",
            "notlocalhost",
            "127.0.0.999",
            "127.0.0",
            "::2",
            "",
        ] {
            assert!(!is_loopback_host(host_of_authority(bad)), "{bad} 必须被拒（前缀/包含匹配会放行它）");
        }
    }

    #[test]
    fn origin_yields_its_authority_host_and_rejects_a_path_trick() {
        assert_eq!(origin_host("http://127.0.0.1:8421"), Some("127.0.0.1"));
        assert_eq!(origin_host("http://evil.example/127.0.0.1"), Some("evil.example"));
        assert_eq!(origin_host("https://localhost"), Some("localhost"));
        // `null`（file:// 与沙箱 iframe）没有 scheme → None → 由 guard_local 拒掉。
        assert_eq!(origin_host("null"), None);
        assert_eq!(origin_host(""), None);
    }

    #[test]
    fn guard_local_accepts_loopback_shapes_and_refuses_the_rest() {
        // 本机客户端（curl / 同源页面）：有 Host、可以没有 Origin。
        assert!(guard_local(&req(Some("127.0.0.1:8421"), None)).is_ok());
        assert!(guard_local(&req(Some("localhost:8421"), Some("http://localhost:8421"))).is_ok());
        // 缺 Host = 无法确认来源 → 拒。
        assert!(guard_local(&req(None, None)).is_err(), "缺 Host 必须拒");
        // DNS rebinding 的形状。
        let err = guard_local(&req(Some("evil.example:8421"), None)).unwrap_err();
        assert!(err.contains("Host"), "消息要点名 Host：{err}");
        // 跨站页面（含 Origin: null）。
        for origin in ["http://evil.example", "https://127.0.0.1.attacker.example", "null"] {
            let err = guard_local(&req(Some("127.0.0.1:8421"), Some(origin))).unwrap_err();
            assert!(err.contains("Origin"), "消息要点名 Origin：{err}");
        }
    }
}
