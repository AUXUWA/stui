//! CLI 模式：面向脚本与 AI 调用的非交互子命令。
//!
//! 与 TUI 共用 `fmt` / `port` 模块，保证解析语义完全一致。
//! 输出保持纯文本：无 ANSI 颜色、无进度条；`--json` 时所有结果（含错误）
//! 都是 stdout 上的单行 JSON，文本模式下正常信息走 stdout、错误走 stderr。
//!
//! `--port sim` 使用进程内回环虚拟设备，无需真实硬件即可自测收发。
//!
//! 子命令一览：
//! - `list` / `ports`：枚举串口（`ports` 是 `list` 的别名）
//! - `send`：发送一段载荷（`--dry-run` 只解析不发送，`--expect` 校验回显，
//!   多个期望用竖线分隔，如 `--expect "41 42|FF"`）
//! - `listen`：收满指定字节数或超时（`--format ndjson` 逐块流式输出，
//!   `--save` 把收到的字节原样落盘）
//! - `watch`：长时监听，NDJSON 流式输出
//! - `record`：把收到的数据原样落盘（`--stdout` 再原样冲一份到 stdout）
//! - `selftest`：用内置 sim 设备自检整条链路
//! - `sim`：进程内回环虚拟设备

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::fmt;
use crate::port::{self, Bits, Flow, Parity, PortCfg, RxMsg, Stops};

/// 退出码：0 成功，1 运行错误，2 参数/解析错误，3 串口打开失败，4 接收超时，
/// 5 期望响应不匹配（`send --expect` 没能等到期望的字节序列）。
const EX_OK: i32 = 0;
const EX_RUN: i32 = 1;
const EX_USAGE: i32 = 2;
const EX_OPEN: i32 = 3;
const EX_TIMEOUT: i32 = 4;
/// 新增退出码：发送成功但响应与 `--expect` 给的目标不符。
///
/// 单独给一个码而不是复用 1：脚本要能区分「操作失败」（1）
/// 与「操作成功但结果不符合预期」（5）——后者往往说明设备在回别的内容，
/// 是调参问题而不是环境问题。
const EX_EXPECT: i32 = 5;

// ---------------------------------------------------------------- JSON 输出

/// 极简 JSON 对象构造器：字段按加入顺序拼接，值类型由调用方显式指定。
struct Obj {
    buf: String,
}

impl Obj {
    fn new() -> Self {
        Obj { buf: String::from("{") }
    }

    /// 写字段名；除首个字段外自动补逗号。
    fn key(&mut self, k: &str) {
        if !self.buf.ends_with('{') {
            self.buf.push(',');
        }
        self.buf.push_str(&escape(k));
        self.buf.push(':');
    }

    fn set(&mut self, k: &str, v: &str) {
        self.key(k);
        self.buf.push_str(&escape(v));
    }

    fn num(&mut self, k: &str, v: impl std::fmt::Display) {
        self.key(k);
        self.buf.push_str(&v.to_string());
    }

    fn flag(&mut self, k: &str, v: bool) {
        self.key(k);
        self.buf.push_str(if v { "true" } else { "false" });
    }

    /// 直接塞入已构造好的 JSON 片段（嵌套对象或数组）。
    fn raw(&mut self, k: &str, v: &str) {
        self.key(k);
        self.buf.push_str(v);
    }

    fn json(self) -> String {
        self.buf + "}"
    }
}

/// 字符串转义：`"` 与 `\` 反转义，控制字符一律写成 `\u00XX`。
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// JSON 用的紧凑十六进制（`fmt::hex_bytes` 面向人眼，带空格分隔）。
///
/// 走 [`fmt::hex_compact`]：那边是查表拼的，
/// 逐字节 `format!` 会为每个字节分配一次临时字符串，
/// 而 listen/send 的接收缓冲区可以到几十 MB。
fn hex_compact(data: &[u8]) -> String {
    fmt::hex_compact(data)
}

/// JSON 结果一律打到 stdout 并立即 flush，便于脚本管道读取。
fn emit(obj: Obj) {
    println!("{}", obj.json());
    let _ = io::stdout().flush();
}

/// 输出一行已经拼好的 JSON 文本（NDJSON 流式输出用）。
///
/// watch 每收到一块数据就要立刻冲一行出去，管道下游才能边收边处理；
/// 这里不能走 `Obj`，因为行内容由专用构造函数决定。
fn emit_raw(line: &str) {
    println!("{line}");
    let _ = io::stdout().flush();
}

/// Unix 纪元起的毫秒数：NDJSON 的 `t` 字段。
///
/// 用绝对时间戳而不是「启动后第几毫秒」：多个来源的流合并时，
/// 只有绝对时间才能对齐先后顺序。
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 失败出口：`--json` 写 stdout 单行 JSON，否则写 stderr。
fn fail(json: bool, code: i32, msg: &str) -> i32 {
    if json {
        let mut o = Obj::new();
        o.flag("ok", false);
        o.set("error", msg);
        emit(o);
    } else {
        eprintln!("错误：{msg}");
    }
    code
}

// ---------------------------------------------------------------- 参数解析

type Opts = HashMap<String, String>;

/// 参数表：`(名称, 是否需要取值)`，不在表内的 `--xxx` 一律报错。
type Spec = &'static [(&'static str, bool)];

const SPEC_LIST: Spec = &[("kind", true), ("verbose", false), ("json", false)];

const SPEC_SEND: Spec = &[
    ("port", true),
    ("baud", true),
    ("bits", true),
    ("parity", true),
    ("stops", true),
    ("flow", true),
    ("text", true),
    ("hex", true),
    ("file", true),
    ("repeat", true),
    ("interval", true),
    ("wait", true),
    ("bytes", true),
    ("expect", true),
    ("expect-timeout", true),
    ("dry-run", false),
    ("json", false),
];

const SPEC_LISTEN: Spec = &[
    ("port", true),
    ("baud", true),
    ("timeout", true),
    ("bytes", true),
    ("idle", true),
    ("format", true),
    // `--save`：把收到的字节原样落盘（listen 自己的落盘参数，不与 record 的 --out 混用）
    ("save", true),
    ("json", false),
];

const SPEC_WATCH: Spec = &[
    ("port", true),
    ("baud", true),
    ("timeout", true),
    ("interval", true),
    ("json", false),
];

const SPEC_RECORD: Spec = &[
    ("port", true),
    ("baud", true),
    ("out", true),
    ("duration", true),
    ("format", true),
    // `--stdout`：把收到的字节再原样冲一份到 stdout（与 --json 互斥）
    ("stdout", false),
    ("json", false),
];

const SPEC_SELFTEST: Spec = &[("json", false)];

const SPEC_SIM: Spec = &[
    ("pattern", true),
    ("count", true),
    ("interval", true),
    ("duration", true),
    ("json", false),
];

/// `--kind` 的合法取值：与 [`port_kind`] 的分类一一对应。
const PORT_KINDS: [&str; 3] = ["usb", "bluetooth", "pci"];

/// 先扫一遍原始参数：即使后续解析失败，也能按 `--json` 决定错误输出格式。
///
/// 只用于**报错**时的输出格式判断；正常输出以解析结果里有没有 `json` 标志为准
/// （否则 `--text --json` 这种漏写取值的写法会既发出内容又切成 JSON 模式）。
fn wants_json(args: &[String]) -> bool {
    args.iter().any(|a| a == "--json")
}

/// 极简解析器：只认 `--key` 与 `--key value` 两种形式。
fn parse_args(args: &[String], spec: Spec) -> Result<Opts, String> {
    let mut out = Opts::new();
    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];
        let key = raw.strip_prefix("--").ok_or_else(|| format!("未知参数：{raw}（参数需以 -- 开头）"))?;
        let (name, takes) =
            spec.iter().find(|(n, _)| *n == key).ok_or_else(|| format!("未知参数：{raw}"))?;
        if *takes {
            // 取值不能是另一个选项：漏写取值时（`--text --json`），
            // 把下一个选项名当内容发给设备是更糟的结果。
            let v = args
                .get(i + 1)
                .filter(|v| !v.starts_with("--"))
                .ok_or_else(|| format!("--{name} 需要一个取值"))?;
            out.insert((*name).to_string(), v.clone());
            i += 2;
        } else {
            out.insert((*name).to_string(), "1".to_string());
            i += 1;
        }
    }
    Ok(out)
}

/// 取非负整数参数，缺省用默认值。
fn num_opt(opts: &Opts, key: &str, default: u64) -> Result<u64, String> {
    match opts.get(key) {
        None => Ok(default),
        Some(v) => v.parse::<u64>().map_err(|_| format!("--{key} 需要非负整数，收到：{v}")),
    }
}

/// 必填的字符串参数。
fn str_req(opts: &Opts, key: &str) -> Result<String, String> {
    opts.get(key)
        .cloned()
        .ok_or_else(|| format!("缺少必填参数 --{key}"))
}

fn parse_bits(v: &str) -> Result<Bits, String> {
    match v {
        "5" => Ok(Bits::Five),
        "6" => Ok(Bits::Six),
        "7" => Ok(Bits::Seven),
        "8" => Ok(Bits::Eight),
        _ => Err(format!("--bits 只能是 5/6/7/8，收到：{v}")),
    }
}

fn parse_parity(v: &str) -> Result<Parity, String> {
    match v.to_ascii_lowercase().as_str() {
        "none" => Ok(Parity::None),
        "odd" => Ok(Parity::Odd),
        "even" => Ok(Parity::Even),
        _ => Err(format!("--parity 只能是 none/odd/even，收到：{v}")),
    }
}

fn parse_stops(v: &str) -> Result<Stops, String> {
    match v {
        "1" => Ok(Stops::One),
        "2" => Ok(Stops::Two),
        _ => Err(format!("--stops 只能是 1/2，收到：{v}")),
    }
}

fn parse_flow(v: &str) -> Result<Flow, String> {
    match v.to_ascii_lowercase().as_str() {
        "none" => Ok(Flow::None),
        "hardware" => Ok(Flow::Hardware),
        "software" => Ok(Flow::Software),
        _ => Err(format!("--flow 只能是 none/hardware/software，收到：{v}")),
    }
}

/// `--format` 的两种输出形状。
///
/// `text` 是面向人的可读行，`ndjson` 是面向脚本的逐行 JSON。
/// 两者都只影响「每一块数据怎么写」，不影响收包语义。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutFormat {
    Text,
    Ndjson,
}

/// 解析 `--format`：只认 `text` 与 `ndjson`。
///
/// 非法值直接报错而不是悄悄回退到 text：脚本拼错了格式名却拿到文本输出，
/// 比直接失败更难排查。
fn parse_format(v: &str) -> Result<OutFormat, String> {
    match v.to_ascii_lowercase().as_str() {
        "text" => Ok(OutFormat::Text),
        "ndjson" => Ok(OutFormat::Ndjson),
        _ => Err(format!("--format 只能是 text / ndjson，收到：{v}")),
    }
}

/// 解析一段期望目标：复用既有的 [`fmt::parse_send_hex`]。
///
/// 「既有规则」意味着 `--hex` 与 `--expect` 用的是同一套写法
/// （`41 42`、`0x41-0x42` 都认），调用方不必记两套语法。
///
/// 这一层只管**一段**：多段由 [`parse_expects`] 按 `|` 拆开后再逐段送来。
fn parse_expect(segment: &str) -> Result<Vec<u8>, String> {
    let bytes = fmt::parse_send_hex(segment).map_err(|e| format!("--expect 解析失败：{e}"))?;
    // 空目标等于「什么都算匹配」：这种校验没有意义，直接当参数错误。
    if bytes.is_empty() {
        return Err("--expect 不能为空：没有可校验的字节序列".to_string());
    }
    Ok(bytes)
}

/// 解析 `--expect` 的一个或多个期望目标。
///
/// 多个期望用**竖线** `|` 分隔：`--expect "41 42|FF|00"` 表示「等三段字节，
/// 收到的数据必须**同时**包含这三段」才算通过。
///
/// 分隔符为什么不是逗号：逗号本来就在 [`fmt::parse_send_hex`] 的分隔符集合里，
/// 它在那里分隔的是**一段之内的字节**。再拿它当「段与段」的分隔符，
/// 同一个字符就有了两种含义 —— `--expect "41,42"` 从「一根连续的 2 字节针脚」
/// 悄悄变成「两根互相独立的 1 字节针脚」（任意顺序、任意位置命中即可），
/// 校验被**静默放宽**：原本判为不匹配的响应会变成通过。
/// 竖线不在那套分隔符里，两种含义不会打架，
/// 旧写法（`--expect "41,42"` 仍是一根 2 字节针脚）一字未变。
///
/// 每一段各自走既有的十六进制规则，因此单期望的写法一字未变；
/// 有一段不合法就是整个参数不合法（退出码 2）——「校验目标」这件事
/// 不能只校验一半，另一半错了脚本还以为验过了。
fn parse_expects(v: &str) -> Result<Vec<Vec<u8>>, String> {
    let segments: Vec<&str> = v.split('|').collect();
    let last = segments.len() - 1;
    let mut out: Vec<Vec<u8>> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        if segment.trim().is_empty() {
            if i == last {
                // 尾随分隔符（`--expect "41|"`）：不算一段，忽略掉。
                // 与 `fmt::parse_send_hex` 跳过空标记的老行为一致。
                continue;
            }
            // 中间的空段（`--expect "41||42"`）是笔误，要指出来。
            return Err(format!(
                "--expect 第 {} 段为空（两段之间多写了分隔符 |）",
                i + 1
            ));
        }
        // 错误信息带上「第几段」：一长串期望里出了岔子，光看字节值定位不到。
        let bytes = parse_expect(segment).map_err(|e| format!("{e}（第 {} 段）", i + 1))?;
        out.push(bytes);
    }
    if out.is_empty() {
        // `split` 至少给一段，这里只是兜底：空目标没有可校验的内容。
        return Err("--expect 不能为空：没有可校验的字节序列".to_string());
    }
    Ok(out)
}

/// 组装串口参数：除端口与波特率外都用默认值。
fn build_cfg(opts: &Opts, port: &str) -> Result<PortCfg, String> {
    // 波特率按 u32 解析：早先按 u64 解析再 `as u32`，越界值会被悄悄截断成
    // 别的波特率（`--baud 4294967297` 变成 1），而不是报参数错误。
    let baud = match opts.get("baud") {
        Some(v) => v
            .parse::<u32>()
            .map_err(|_| format!("--baud 需要 0~4294967295 之间的整数，收到：{v}"))?,
        None => 115_200,
    };
    Ok(PortCfg {
        port: port.to_string(),
        baud,
        bits: match opts.get("bits") {
            Some(v) => parse_bits(v)?,
            None => Bits::Eight,
        },
        parity: match opts.get("parity") {
            Some(v) => parse_parity(v)?,
            None => Parity::None,
        },
        stops: match opts.get("stops") {
            Some(v) => parse_stops(v)?,
            None => Stops::One,
        },
        flow: match opts.get("flow") {
            Some(v) => parse_flow(v)?,
            None => Flow::None,
        },
    })
}

// ---------------------------------------------------------------- 传输层

/// 虚拟回环设备：写进 `tx` 的字节由后台线程原样送回 `rx`。
struct SimLink {
    rx: Receiver<Vec<u8>>,
    tx: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
}

impl SimLink {
    fn open() -> io::Result<SimLink> {
        let (tx, rx_in) = mpsc::channel::<Vec<u8>>(); // 主流程 → 回环线程
        let (tx_loop, rx) = mpsc::channel::<Vec<u8>>(); // 回环线程 → 主流程
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);

        // 线程只依赖通道：主流程结束后 tx 被丢弃，recv 断开即自然退出。
        thread::Builder::new()
            .name("sim-loop".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match rx_in.recv_timeout(Duration::from_millis(100)) {
                        Ok(data) => {
                            if tx_loop.send(data).is_err() {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;

        Ok(SimLink { rx, tx, stop })
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.tx
            .send(data.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "回环通道已关闭"))
    }
}

impl Drop for SimLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 一块收上来的数据，或「暂时没有」/「对端已关闭」。
enum Chunk {
    Data(Vec<u8>),
    Idle,
    Closed,
}

/// 真实串口与虚拟回环的统一封装，上层收发逻辑只写一份。
enum Link {
    Real(port::Link),
    Sim(SimLink),
}

impl Link {
    /// `--port sim` 走进程内回环，其余走真实串口。
    fn open(name: &str, cfg: PortCfg) -> io::Result<Link> {
        if name.eq_ignore_ascii_case("sim") {
            Ok(Link::Sim(SimLink::open()?))
        } else {
            Ok(Link::Real(port::Link::open(cfg)?))
        }
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Link::Real(l) => l.write(data),
            Link::Sim(l) => l.write(data),
        }
    }

    /// 最多等 `timeout` 取一块数据；读错误作为运行错误上抛。
    fn recv(&self, timeout: Duration) -> Result<Chunk, String> {
        match self {
            Link::Real(l) => {
                let rx = l.rx.lock().map_err(|_| "接收队列已损坏".to_string())?;
                match rx.recv_timeout(timeout) {
                    Ok(RxMsg::Data(d)) => Ok(Chunk::Data(d)),
                    Ok(RxMsg::Err(e)) => Err(e),
                    Ok(RxMsg::Closed) => Ok(Chunk::Closed),
                    Err(RecvTimeoutError::Timeout) => Ok(Chunk::Idle),
                    Err(RecvTimeoutError::Disconnected) => Ok(Chunk::Closed),
                }
            }
            Link::Sim(l) => Ok(match l.rx.recv_timeout(timeout) {
                Ok(d) => Chunk::Data(d),
                Err(RecvTimeoutError::Timeout) => Chunk::Idle,
                Err(RecvTimeoutError::Disconnected) => Chunk::Closed,
            }),
        }
    }

    /// 仅虚拟回环可用：取一份发送句柄给 `sim` 的 pattern 线程。
    fn echo_tx(&self) -> Option<Sender<Vec<u8>>> {
        match self {
            Link::Sim(l) => Some(l.tx.clone()),
            Link::Real(_) => None,
        }
    }
}

/// 统一收包循环：到总超时、收满 `want`、空闲超过 `idle` 或对端关闭为止。
fn collect(
    link: &Link,
    deadline: Instant,
    want: Option<usize>,
    idle: Option<Duration>,
    buf: &mut Vec<u8>,
) -> Result<(), String> {
    collect_streaming(link, deadline, want, idle, buf, |_| {})
}

/// 与 [`collect`] 完全相同的收包语义，但每收到一块就把数据交给回调。
///
/// 回调存在的理由是「流式输出」：NDJSON 要求来一块就冲一行出去，
/// 而 `collect` 只把数据攒进缓冲区，直到最后才交给调用方。
/// `collect` 本身实现成这个函数的一次调用，保证两条路径的收包行为不会漂移。
fn collect_streaming(
    link: &Link,
    deadline: Instant,
    want: Option<usize>,
    idle: Option<Duration>,
    buf: &mut Vec<u8>,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<(), String> {
    const SLOT: Duration = Duration::from_millis(20);
    let mut last = Instant::now();
    loop {
        if want.is_some_and(|n| buf.len() >= n) {
            break;
        }
        let mut left = deadline.saturating_duration_since(Instant::now());
        if let Some(lim) = idle {
            left = left.min(lim.saturating_sub(last.elapsed()));
        }
        if left.is_zero() {
            break;
        }
        match link.recv(left.min(SLOT))? {
            Chunk::Data(d) => {
                buf.extend_from_slice(&d);
                last = Instant::now();
                on_chunk(&d);
            }
            Chunk::Idle => {}
            Chunk::Closed => break,
        }
    }
    Ok(())
}

/// 子串查找：期望序列是否出现在已收到的字节里。
///
/// 期望序列可能跨块边界（前半截在上一块、后半截在这一块），
/// 所以要在**整个**缓冲区上找，而不是逐块比对。
fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// 在剩余时限内继续接收，直到**全部**期望序列都出现或到点。
///
/// 返回是否等齐了全部期望；收到的数据继续追加到 `buf` 里，
/// 这样报告出去的 `received` / `rx_hex` 是完整的。
///
/// 多期望时是「与」的关系：必须同时包含每一段才算通过——
/// 设备只回了其中一段，说明它还没说完整的话。
fn await_expects(
    link: &Link,
    deadline: Instant,
    buf: &mut Vec<u8>,
    needles: &[Vec<u8>],
) -> Result<bool, String> {
    const SLOT: Duration = Duration::from_millis(20);
    loop {
        // 先看已有数据：最后一次收到的块里可能刚好凑齐了期望序列。
        if needles.iter().all(|n| contains_bytes(buf, n)) {
            return Ok(true);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        match link.recv(left.min(SLOT))? {
            Chunk::Data(d) => buf.extend_from_slice(&d),
            Chunk::Idle => {}
            // 对端关闭：不会再有数据了，等不到就是等不到。
            Chunk::Closed => return Ok(false),
        }
    }
}

/// 载入发送载荷：按 `--file` → `--hex` → `--text` 顺序拼接；三者至少给一个。
///
/// 顺序固定为 file → hex → text：同一组参数重复执行时字节序列必须完全一致，
/// 脚本才能把「解析结果」和「实际发出的内容」对得上。
fn build_payload(opts: &Opts) -> Result<Vec<u8>, String> {
    if !opts.contains_key("file") && !opts.contains_key("hex") && !opts.contains_key("text") {
        return Err("没有指定要发送的内容（--text/--hex/--file）".to_string());
    }
    let mut data = Vec::new();
    if let Some(p) = opts.get("file") {
        data.extend_from_slice(&std::fs::read(p).map_err(|e| format!("读取文件失败 {p}：{e}"))?);
    }
    if let Some(h) = opts.get("hex") {
        data.extend_from_slice(&fmt::parse_send_hex(h)?);
    }
    if let Some(t) = opts.get("text") {
        data.extend_from_slice(&fmt::parse_send_text(t)?);
    }
    // 载荷为空属于用法错误：发 0 字节对调用方毫无意义，
    // 静默成功只会让脚本以为自己已经把数据送出去了。
    if data.is_empty() {
        return Err("要发送的内容为空（--text/--hex/--file 都没有产生字节）".to_string());
    }
    Ok(data)
}

// ---------------------------------------------------------------- 子命令

/// `--expect` 的目标与校验结论。
struct Expect {
    /// 期望在收到的数据里出现的字节序列（一个或多个）。
    ///
    /// 多期望时是「与」的关系：全部出现才算通过。
    needles: Vec<Vec<u8>>,
    /// 是否真的等到了**全部**期望。
    ok: bool,
    /// 是否**真的校验过**。
    ///
    /// `--dry-run` 什么都没发、也就什么都没校验：这时 `ok` 只是「没有失败」，
    /// 拿它当「设备回包已校验通过」会误判，所以另给一个字段说清楚。
    checked: bool,
    /// 没命中的期望（校验过、且确实有缺失时才有内容）。
    missing: Vec<Vec<u8>>,
}

/// `expect_hex` 字段的内容：各期望目标的紧凑十六进制，逗号分隔。
///
/// 单期望时与旧字段一字不差（`"4142"`）；多期望时是 `"4142,FF"`。
/// 保持这个字段「一段一串」的形状，是为了让既有的单期望脚本不受影响。
fn expect_hex_text(needles: &[Vec<u8>]) -> String {
    needles
        .iter()
        .map(|n| hex_compact(n))
        .collect::<Vec<_>>()
        .join(",")
}

/// `expect_missing` 字段的内容：没命中的期望（紧凑十六进制组成的数组）。
///
/// 全部命中时是空数组。字段恒定存在，脚本按固定 schema 取值，
/// 不用先判断「这一轮到底有没有用 `--expect`」。
fn expect_missing_json(missing: &[Vec<u8>]) -> String {
    let items: Vec<String> = missing.iter().map(|n| escape(&hex_compact(n))).collect();
    format!("[{}]", items.join(","))
}

/// `send` 一次调用的结果汇总。
///
/// 用结构体而不是一长串位置参数：字段已经超过八个，
/// 位置参数一旦写错顺序，JSON 里就会出现「波特率写进了 sent」这种难查的错。
struct SendReport<'a> {
    port: &'a str,
    baud: u32,
    sent: u64,
    rx: &'a [u8],
    elapsed: u128,
    /// `--dry-run` 时给出「将要发送」的字节。
    planned: Option<&'a [u8]>,
    /// `--expect` 的目标与结论。
    expect: Option<&'a Expect>,
    /// 整体是否成功（期望不匹配时为 false）。
    ok: bool,
    /// 失败原因：只在失败时出现。
    error: Option<&'a str>,
}

/// 组装 `send` / `send --dry-run` 的 JSON 结果体。
///
/// `planned` 只在 `--dry-run` 时出现：它描述「将要发送」的字节数与内容，
/// 而 `sent` 恒为 0（dry-run 根本没打开串口）。
fn send_json(r: &SendReport) -> String {
    let mut o = Obj::new();
    o.flag("ok", r.ok);
    if let Some(e) = r.error {
        o.set("error", e);
    }
    o.set("port", r.port);
    o.num("baud", r.baud);
    o.num("sent", r.sent);
    if let Some(p) = r.planned {
        o.num("planned", p.len());
        o.set("planned_hex", &hex_compact(p));
    }
    o.num("received", r.rx.len());
    o.set("rx_hex", &hex_compact(r.rx));
    o.set("rx_text", &String::from_utf8_lossy(r.rx));
    o.num("elapsed_ms", r.elapsed);
    // 期望字段恒定出现：脚本按固定 schema 取值，
    // 不用先判断「这一轮到底有没有用 --expect」。
    o.flag("expect_ok", r.expect.map(|e| e.ok).unwrap_or(true));
    o.set("expect_hex", &r.expect.map(|e| expect_hex_text(&e.needles)).unwrap_or_default());
    // `expect_ok` 只说「有没有失败」，`expect_checked` 才说「有没有校验过」：
    // `--dry-run` 根本没发也没校验，这时 `expect_checked` 恒为 false。
    o.flag("expect_checked", r.expect.map(|e| e.checked).unwrap_or(false));
    // 差在哪一段：多期望时 `expect_ok=false` 只说「没齐」，这个字段说清「缺哪几段」。
    let missing: &[Vec<u8>] = r.expect.map(|e| e.missing.as_slice()).unwrap_or(&[]);
    o.raw("expect_missing", &expect_missing_json(missing));
    o.json()
}

/// `watch` 每收到一块数据输出的一行（NDJSON）。
fn watch_chunk_json(t: u128, data: &[u8]) -> String {
    let mut o = Obj::new();
    o.flag("ok", true);
    o.num("t", t);
    o.num("len", data.len());
    o.set("rx_hex", &hex_compact(data));
    o.set("rx_text", &String::from_utf8_lossy(data));
    o.json()
}

/// `watch` 结束时的汇总行（NDJSON）。
fn watch_summary_json(total: u64, chunks: u64, elapsed: u128) -> String {
    let mut o = Obj::new();
    o.flag("ok", true);
    o.flag("done", true);
    o.num("total", total);
    o.num("chunks", chunks);
    o.num("elapsed_ms", elapsed);
    o.json()
}

/// `listen --format ndjson` 收满时的汇总行。
///
/// 字段口径与 `watch` 的汇总行一致（total / chunks / elapsed_ms），
/// 另补上 `listen` 一直以来的 received / rx_hex / rx_text：
/// 同一份脚本无论读 watch 还是 listen 的 NDJSON，取值习惯都一致。
/// 给了 `--save` 时多一个 `out`（保存到的路径）。
fn listen_ndjson_summary(
    port: &str,
    out: Option<&str>,
    rx: &[u8],
    chunks: u64,
    elapsed: u128,
) -> String {
    let mut o = Obj::new();
    o.flag("ok", true);
    o.flag("done", true);
    o.set("port", port);
    if let Some(p) = out {
        o.set("out", p);
    }
    o.num("total", rx.len());
    o.num("chunks", chunks);
    o.num("received", rx.len());
    o.set("rx_hex", &hex_compact(rx));
    o.set("rx_text", &String::from_utf8_lossy(rx));
    o.num("elapsed_ms", elapsed);
    o.json()
}

/// `listen --format ndjson` 没收满时的收尾行。
///
/// 与成功行同形状，只是 `ok` 为 false 并带上 `error`：
/// 流式消费方读到最后一行就能知道这一轮是收齐了还是超时了。
fn listen_ndjson_timeout(
    port: &str,
    out: Option<&str>,
    rx: &[u8],
    chunks: u64,
    elapsed: u128,
) -> String {
    let mut o = Obj::new();
    o.flag("ok", false);
    o.flag("done", true);
    o.set("error", "timeout");
    o.set("port", port);
    if let Some(p) = out {
        o.set("out", p);
    }
    o.num("total", rx.len());
    o.num("chunks", chunks);
    o.num("received", rx.len());
    o.set("rx_hex", &hex_compact(rx));
    o.set("rx_text", &String::from_utf8_lossy(rx));
    o.num("elapsed_ms", elapsed);
    o.json()
}

/// `record` 每收到一块数据输出的一行（NDJSON）。
fn record_chunk_json(t: u128, data: &[u8]) -> String {
    let mut o = Obj::new();
    o.flag("ok", true);
    o.num("t", t);
    o.num("len", data.len());
    o.set("rx_hex", &hex_compact(data));
    o.json()
}

/// `record` 结束时的汇总行。
fn record_summary_json(port: &str, out: &str, bytes: u64, chunks: u64, elapsed: u128) -> String {
    let mut o = Obj::new();
    o.flag("ok", true);
    o.set("port", port);
    o.set("out", out);
    o.num("bytes", bytes);
    o.num("chunks", chunks);
    o.num("elapsed_ms", elapsed);
    o.json()
}

/// `record` 的默认文件名：`stui-record-YYYYMMDD-HHMMSS.mmm.log`。
///
/// 时间戳取自 `fmt::stamp`（与项目其它地方的时间显示同一套口径）。
///
/// **精确到毫秒**：只到秒的话，同一秒里跑两次 record 会撞成同一个文件名，
/// 而落盘用的是 `File::create`（会截断），上一份留档会被直接抹掉。
fn record_default_name(stamp: &fmt::Stamp) -> String {
    format!("stui-record-{}.log", stamp.compact_ms())
}

/// 记录文件的完整路径：没给 `--out` 时落在当前目录（文件名是相对路径）。
fn record_out_path(out: Option<&str>, stamp: &fmt::Stamp) -> String {
    match out {
        Some(p) => p.to_string(),
        None => record_default_name(stamp),
    }
}

/// 端口类型：描述含「蓝牙」判为蓝牙，含「PCI」判为 PCI，其余按 USB 归类。
fn port_kind(desc: &str) -> &'static str {
    if desc.contains("蓝牙") {
        "bluetooth"
    } else if desc.contains("PCI") {
        "pci"
    } else {
        "usb"
    }
}

/// `--kind` 过滤：给定类型时只保留该类端口，`None` 表示不过滤。
fn kind_matches(desc: &str, kind: Option<&str>) -> bool {
    match kind {
        None => true,
        Some(k) => port_kind(desc) == k,
    }
}

fn cmd_list(args: &[String]) -> i32 {
    // 解析失败时还不知道有没有 `--json`，先按原始参数判断错误输出格式。
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_LIST) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    // 解析成功后才认 `--json` 这个标志：它不能是被当成取值的那个 `--json`。
    let json = opts.contains_key("json");
    // `--verbose` 只影响人看的输出：JSON 里本来就给全了厂商与产品。
    let verbose = opts.contains_key("verbose");

    // `--kind` 过滤：分类沿用 port_kind，不另立一套规则。
    let kind = match opts.get("kind") {
        Some(k) => {
            let k = k.to_ascii_lowercase();
            if !PORT_KINDS.contains(&k.as_str()) {
                return fail(
                    json,
                    EX_USAGE,
                    &format!("--kind 只能是 usb/bluetooth/pci，收到：{k}"),
                );
            }
            Some(k)
        }
        None => None,
    };

    // 用带厂商 / 产品信息的枚举结果：只有它才有 manufacturer / product 两列。
    let all = match port::list_ports_detailed() {
        Ok(p) => p,
        Err(e) => return fail(json, EX_OPEN, &format!("枚举串口失败：{e}")),
    };
    let ports: Vec<&port::PortDetail> =
        all.iter().filter(|p| kind_matches(&p.desc, kind.as_deref())).collect();

    if json {
        let items: Vec<String> = ports
            .iter()
            .map(|p| {
                let mut o = Obj::new();
                o.set("port", &p.port);
                o.set("desc", &p.desc);
                o.set("kind", port_kind(&p.desc));
                // 厂商 / 产品可能拿不到（系统没给、或不是 USB 端口）：
                // 缺就给空串，字段本身必须存在，脚本才能固定取值。
                o.set("manufacturer", &p.manufacturer);
                o.set("product", &p.product);
                o.json()
            })
            .collect();
        let mut o = Obj::new();
        o.flag("ok", true);
        o.num("count", ports.len());
        o.raw("ports", &format!("[{}]", items.join(",")));
        emit(o);
    } else if ports.is_empty() {
        if kind.is_some() {
            println!("（没有匹配的串口）");
        } else {
            println!("（未发现串口）");
        }
    } else {
        for p in &ports {
            if verbose {
                // 多出厂商与产品两列。用 " | " 分隔而不是靠列宽对齐：
                // 描述文本长度不固定，靠宽度对齐迟早会把两列粘在一起。
                println!(
                    "{:<12}{} | 厂商: {} | 产品: {}",
                    p.port, p.desc, p.manufacturer, p.product
                );
            } else {
                println!("{:<12}{}", p.port, p.desc);
            }
        }
    }
    EX_OK
}

fn cmd_send(args: &[String]) -> i32 {
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_SEND) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    let json = opts.contains_key("json");

    // `--dry-run` 只解析不发送，因此不打开串口：端口名可有可无。
    let dry_run = opts.contains_key("dry-run");
    let name = match opts.get("port") {
        Some(v) => v.clone(),
        None if dry_run => String::new(),
        None => return fail(json, EX_USAGE, "缺少必填参数 --port"),
    };
    let cfg = match build_cfg(&opts, &name) {
        Ok(c) => c,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let payload = match build_payload(&opts) {
        Ok(p) => p,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // `--expect` 的目标先解析：即使后面是 dry-run，非法目标也必须报参数错误，
    // 否则「校验目标」这件事在 dry-run 下就形同虚设。
    // 多个期望用竖线分隔（`--expect "41 42|FF"`）：收到的数据必须同时包含它们。
    // 逗号不作段分隔符：它在 `--hex` 那套写法里已经分隔「一段之内的字节」了，
    // 两种含义混在一起会把旧写法（`--expect "41,42"` 是一根 2 字节针脚）
    // 悄悄变成两根独立针脚，校验被静默放宽。
    let expect_needles = match opts.get("expect") {
        Some(v) => match parse_expects(v) {
            Ok(b) => Some(b),
            Err(e) => return fail(json, EX_USAGE, &e),
        },
        None => None,
    };
    // `--expect-timeout` 没有 `--expect` 就是一句没有主语的指令：
    // 直接报出来，好过让人以为「超时」真的生效了。
    if opts.contains_key("expect-timeout") && expect_needles.is_none() {
        return fail(json, EX_USAGE, "--expect-timeout 需要与 --expect 一起使用");
    }

    // 发送相关的参数**先校验再干活**：dry-run 只跳过「发送 / 等待」这个动作，
    // 不该跳过「参数写得对不对」—— 否则同一份参数在 dry-run 下退出码是 0、
    // 真跑的时候是 2，脚本没法用退出码判断。
    let repeat = match num_opt(&opts, "repeat", 1) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // `--repeat 0` 等于一次都没发：和「空载荷」一样是用法错误，
    // 不能返回 ok=true 让脚本以为数据已经送出去了。
    if repeat == 0 {
        return fail(json, EX_USAGE, "--repeat 不能为 0（至少要发一次）");
    }
    let interval = match num_opt(&opts, "interval", 100) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let wait = match num_opt(&opts, "wait", 500) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let want = match opts.get("bytes") {
        Some(v) => match v.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => return fail(json, EX_USAGE, &format!("--bytes 需要非负整数，收到：{v}")),
        },
        None => None,
    };
    // 期望响应的时限：没给 `--expect-timeout` 就复用 `--wait`，
    // 「等多久算超时」只有一个来源，不会出现两个参数各说各话。
    // 这个时限同时是**收包窗口的上限**与**命中判定的截止时刻**：
    // 收包只收到它为止，之后才到的字节不算命中。
    let expect_timeout = match opts.get("expect-timeout") {
        Some(_) => match num_opt(&opts, "expect-timeout", wait) {
            Ok(v) => v,
            Err(e) => return fail(json, EX_USAGE, &e),
        },
        None => wait,
    };

    // dry-run 到这里就收工：不建连接、不写字节，只报告「将要发什么」。
    if dry_run {
        let elapsed = 0u128;
        // dry-run 不发送也不等待，因此不存在「响应与期望不符」这回事：
        // expect_ok 只是「没有失败」，而 `checked` 为 false 说清楚「根本没校验」。
        let expect = expect_needles.as_ref().map(|ns| Expect {
            needles: ns.clone(),
            ok: true,
            checked: false,
            missing: Vec::new(),
        });
        if json {
            emit_raw(&send_json(&SendReport {
                port: &name,
                baud: cfg.baud,
                sent: 0,
                rx: &[],
                elapsed,
                planned: Some(&payload),
                expect: expect.as_ref(),
                ok: true,
                error: None,
            }));
        } else {
            println!("将发送 {}（dry-run，未实际发送）", fmt::human_bytes(payload.len() as u64));
            println!("planned_hex: {}", fmt::hex_bytes(&payload));
            println!("planned_text: {}", fmt::preview_text(&payload));
            if let Some(ns) = &expect_needles {
                println!(
                    "将校验期望响应：{}（dry-run 未实际校验）",
                    expect_hex_text(ns)
                );
            }
        }
        return EX_OK;
    }

    let start = Instant::now();
    let mut link = match Link::open(&name, cfg.clone()) {
        Ok(l) => l,
        Err(e) => return fail(json, EX_OPEN, &format!("打开 {name} 失败：{e}")),
    };

    // 逐轮发送：轮与轮之间等 `--interval`，最后一轮后不再等待。
    let mut sent = 0u64;
    for i in 0..repeat {
        if let Err(e) = link.write(&payload) {
            return fail(json, EX_RUN, &format!("写入 {name} 失败：{e}"));
        }
        sent += payload.len() as u64;
        if i + 1 < repeat && interval > 0 {
            thread::sleep(Duration::from_millis(interval));
        }
    }
    // 发送结束的时刻：期望响应的时限从这一刻起算，
    // 「多久之内必须回包」才有明确的起点。
    let sent_at = Instant::now();
    // 期望响应的时限：**一个**来源，同时管两件事 ——
    // 收包到什么时候为止、哪些字节算命中。
    // 不给 `--expect-timeout` 时它就是 `--wait`（见上面的解析）。
    let expect_deadline = sent_at + Duration::from_millis(expect_timeout);

    // 回显等待：`--wait` 到点、`--bytes` 收满或对端关闭为止。
    let mut rx = Vec::new();
    if wait > 0 {
        // 收包窗口不能越过期望时限：晚到的字节不算命中。
        // 否则 `--expect-timeout 100 --wait 3000` 会等满 3 秒，
        // 「等回包的时限」被 `--wait` 架空了。
        let deadline = (Instant::now() + Duration::from_millis(wait)).min(expect_deadline);
        if let Err(e) = collect(&link, deadline, want, None, &mut rx) {
            return fail(json, EX_RUN, &format!("接收 {name} 失败：{e}"));
        }
    }

    // 期望校验：收到的字节里**同时包含**全部期望序列才算成功。
    //
    // 收到的字节都在时限之内（收包窗口不超过时限），所以这里直接判「全齐没有」；
    // 没齐就继续等到时限为止 —— 全程用同一个时限，不会两个参数各说各话。
    let mut expect: Option<Expect> = None;
    if let Some(needles) = expect_needles {
        let ok = if needles.iter().all(|n| contains_bytes(&rx, n)) {
            true
        } else {
            match await_expects(&link, expect_deadline, &mut rx, &needles) {
                Ok(v) => v,
                Err(e) => return fail(json, EX_RUN, &format!("接收 {name} 失败：{e}")),
            }
        };
        // 缺哪几段：等完之后按最终缓冲区重新判定，
        // 免得「等超时了」与「只差一段」这两种情况在报告里长得一模一样。
        let missing: Vec<Vec<u8>> = needles
            .iter()
            .filter(|n| !contains_bytes(&rx, n))
            .cloned()
            .collect();
        expect = Some(Expect {
            needles,
            ok,
            checked: true,
            missing,
        });
    }
    let elapsed = start.elapsed().as_millis();

    // 期望不匹配：单独用退出码 5，与「运行错误（1）」区分开。
    if let Some(e) = &expect {
        if !e.ok {
            if json {
                emit_raw(&send_json(&SendReport {
                    port: &name,
                    baud: cfg.baud,
                    sent,
                    rx: &rx,
                    elapsed,
                    planned: None,
                    expect: Some(e),
                    ok: false,
                    error: Some("期望的响应未出现"),
                }));
            } else {
                eprintln!(
                    "错误：未收到期望的响应（期望 {}，实际收到 {} 字节）",
                    expect_hex_text(&e.needles),
                    rx.len()
                );
                if !e.missing.is_empty() {
                    eprintln!("未命中：{}", expect_hex_text(&e.missing));
                }
                if !rx.is_empty() {
                    eprintln!("rx_hex: {}", fmt::hex_bytes(&rx));
                }
            }
            return EX_EXPECT;
        }
    }


    if json {
        emit_raw(&send_json(&SendReport {
            port: &name,
            baud: cfg.baud,
            sent,
            rx: &rx,
            elapsed,
            planned: None,
            expect: expect.as_ref(),
            ok: true,
            error: None,
        }));
    } else {
        println!(
            "已发送 {}，收到 {}",
            fmt::human_bytes(sent),
            fmt::human_bytes(rx.len() as u64)
        );
        if !rx.is_empty() {
            println!("rx_hex: {}", fmt::hex_bytes(&rx));
            println!("rx_text: {}", fmt::preview_text(&rx));
        }
        if let Some(e) = &expect {
            println!("期望响应已匹配：{}", expect_hex_text(&e.needles));
        }
        println!("耗时 {elapsed} ms");
    }
    EX_OK
}

/// `listen` 收包过程中的两类失败。
///
/// 分开是因为它们的含义不同：收包失败是链路问题，写文件失败是「数据没落下去」——
/// 后者更严重，绝不能被当成「链路抖动」一笔带过。
#[derive(Debug)]
enum ListenErr {
    /// 收包本身出错（链路问题）。
    Recv(String),
    /// 写保存文件出错（数据没能落盘）。
    Save(String),
}

/// `listen` 的收包 + 落盘：返回 `(收到的字节, 块数)`。
///
/// 拆出来是为了能在单元测试里用 sim 回环当数据源直接驱动，
/// 验证「收到的字节确实原样写进了 `--save` 指定的文件」——这正是 `--save` 的全部意义。
///
/// `--save` 为 `None` 时不落任何盘；给了文件就**边收边写**：
/// 超时退出时已经收到的数据也不会丢。
fn listen_collect(
    link: &Link,
    deadline: Instant,
    want: usize,
    idle: Option<Duration>,
    format: OutFormat,
    mut save: Option<&mut std::fs::File>,
) -> Result<(Vec<u8>, u64), ListenErr> {
    let mut rx = Vec::new();
    let mut chunks = 0u64;
    let mut save_err: Option<String> = None;
    collect_streaming(link, deadline, Some(want), idle, &mut rx, |d| {
        if let Some(f) = save.as_mut() {
            // 第一次写失败就记住它，后面不再写：磁盘已经出问题了，
            // 继续写只会刷一屏一样的错误。
            if save_err.is_none() {
                if let Err(e) = f.write_all(d) {
                    save_err = Some(format!("写入保存文件失败：{e}"));
                }
            }
        }
        if format == OutFormat::Ndjson {
            // 流式：每收到一块就冲一行 JSON，管道下游才能边收边处理。
            chunks += 1;
            emit_raw(&watch_chunk_json(now_millis(), d));
        }
    })
    .map_err(ListenErr::Recv)?;
    if let Some(e) = save_err {
        return Err(ListenErr::Save(e));
    }
    Ok((rx, chunks))
}

/// `listen` 收满 / 超时时的那一行 JSON（text 格式下的最终结果）。
///
/// `save` 给了才带 `out` 字段：不给 `--save` 时结果与以前一字不差。
fn listen_result_json(
    ok: bool,
    error: Option<&str>,
    port: &str,
    save: Option<&str>,
    rx: &[u8],
    elapsed: u128,
) -> String {
    let mut o = Obj::new();
    o.flag("ok", ok);
    if let Some(e) = error {
        o.set("error", e);
    }
    o.set("port", port);
    if let Some(p) = save {
        o.set("out", p);
    }
    o.num("received", rx.len());
    o.set("rx_hex", &hex_compact(rx));
    o.set("rx_text", &String::from_utf8_lossy(rx));
    o.num("elapsed_ms", elapsed);
    o.json()
}

fn cmd_listen(args: &[String]) -> i32 {
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_LISTEN) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    let json = opts.contains_key("json");

    let name = match str_req(&opts, "port") {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let cfg = match build_cfg(&opts, &name) {
        Ok(c) => c,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let timeout = match num_opt(&opts, "timeout", 2000) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let want = match num_opt(&opts, "bytes", 64) {
        Ok(v) => v as usize,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let idle = match opts.get("idle") {
        Some(_) => match num_opt(&opts, "idle", 0) {
            Ok(v) => Some(Duration::from_millis(v)),
            Err(e) => return fail(json, EX_USAGE, &e),
        },
        None => None,
    };
    // `--format text`（默认）：一次收完再输出，与既有行为一个字都不差。
    // `--format ndjson`：来一块输一块，最后再输一行带 done 的汇总。
    let format = match opts.get("format") {
        Some(v) => match parse_format(v) {
            Ok(f) => f,
            Err(e) => return fail(json, EX_USAGE, &e),
        },
        None => OutFormat::Text,
    };

    let start = Instant::now();
    let link = match Link::open(&name, cfg) {
        Ok(l) => l,
        Err(e) => return fail(json, EX_OPEN, &format!("打开 {name} 失败：{e}")),
    };

    // `--save`：把收到的字节**原样**写进这个文件（与 record 的落盘语义一致：
    // 不解码、不转义、不补换行）。
    // 不给 `--save` 时一个字节都不落盘，行为与以前一模一样。
    let save_path = opts.get("save").cloned();
    let mut save_file = match &save_path {
        Some(p) => match std::fs::File::create(p) {
            Ok(f) => Some(f),
            // 文件打不开属于运行错误（退出码 1）：提前失败好过收完才发现没写进去。
            Err(e) => return fail(json, EX_RUN, &format!("创建保存文件 {p} 失败：{e}")),
        },
        None => None,
    };

    let deadline = start + Duration::from_millis(timeout);
    let (rx, chunks) =
        match listen_collect(&link, deadline, want, idle, format, save_file.as_mut()) {
            Ok(v) => v,
            Err(ListenErr::Recv(e)) => return fail(json, EX_RUN, &format!("接收 {name} 失败：{e}")),
            Err(ListenErr::Save(e)) => {
                return fail(
                    json,
                    EX_RUN,
                    &format!("{e}（保存文件 {}）", save_path.as_deref().unwrap_or("")),
                )
            }
        };
    // 收尾前先 flush：进程被杀时缓冲区里的数据会丢，
    // 「原样落盘」的承诺就打了折扣。
    if let Some(f) = save_file.as_mut() {
        if let Err(e) = f.flush() {
            return fail(
                json,
                EX_RUN,
                &format!("刷新保存文件 {} 失败：{e}", save_path.as_deref().unwrap_or("")),
            );
        }
    }
    let elapsed = start.elapsed().as_millis();

    if rx.len() >= want {
        if format == OutFormat::Ndjson {
            emit_raw(&listen_ndjson_summary(
                &name,
                save_path.as_deref(),
                &rx,
                chunks,
                elapsed,
            ));
        } else if json {
            emit_raw(&listen_result_json(
                true,
                None,
                &name,
                save_path.as_deref(),
                &rx,
                elapsed,
            ));
        } else {
            println!("收到 {}", fmt::human_bytes(rx.len() as u64));
            println!("rx_hex: {}", fmt::hex_bytes(&rx));
            println!("rx_text: {}", fmt::preview_text(&rx));
            if let Some(p) = &save_path {
                println!("已保存到 {p}");
            }
            println!("耗时 {elapsed} ms");
        }
        return EX_OK;
    }

    // 未收满：按约定以退出码 4 报超时，但已收到的数据照样回给调用方。
    if format == OutFormat::Ndjson {
        emit_raw(&listen_ndjson_timeout(
            &name,
            save_path.as_deref(),
            &rx,
            chunks,
            elapsed,
        ));
    } else if json {
        emit_raw(&listen_result_json(
            false,
            Some("timeout"),
            &name,
            save_path.as_deref(),
            &rx,
            elapsed,
        ));
    } else {
        eprintln!("错误：接收超时（{elapsed} ms，收到 {} 字节）", rx.len());
        if !rx.is_empty() {
            eprintln!("rx_hex: {}", fmt::hex_bytes(&rx));
        }
        if let Some(p) = &save_path {
            eprintln!("已保存到 {p}");
        }
    }
    EX_TIMEOUT
}

/// `watch` 的收包循环：每收到一块数据就交给 `on_chunk`，直到总时长到点或对端关闭。
///
/// 打印放在调用方的回调里（而不是循环里），是为了让这段逻辑能在单元测试里
/// 被直接驱动：测试用回调把数据收进数组，不必去捕获 stdout。
/// 返回 `(总字节数, 块数)`。
fn watch_loop(
    link: &Link,
    deadline: Option<Instant>,
    poll: Duration,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<(u64, u64), String> {
    let mut total = 0u64;
    let mut chunks = 0u64;
    loop {
        // 到点就收尾；没设截止时间时这一段永远不成立。
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        // 单次等待不超过统计窗口：既能及时醒来查截止时间，
        // 又不会因为窗口太大而迟迟发现对端已经关闭。
        let wait = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()).min(poll),
            None => poll,
        };
        if wait.is_zero() {
            break;
        }
        match link.recv(wait)? {
            Chunk::Data(d) => {
                total += d.len() as u64;
                chunks += 1;
                on_chunk(&d);
            }
            Chunk::Idle => {}
            Chunk::Closed => break,
        }
    }
    Ok((total, chunks))
}

/// `watch`：长时监听，来一块输一块（NDJSON 流式输出）。
///
/// 与 `listen` 的区别是「不设收满条件」：`listen` 等够 `--bytes` 或超时就收工，
/// `watch` 只在总时长到点（`--timeout`）或对端关闭时才结束，
/// 适合「挂在那里一直看」的场景。收包语义与 `listen` 共用同一套 [`collect`]。
fn cmd_watch(args: &[String]) -> i32 {
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_WATCH) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    let json = opts.contains_key("json");

    let name = match str_req(&opts, "port") {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let cfg = match build_cfg(&opts, &name) {
        Ok(c) => c,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // timeout 为 0 或省略 = 不设截止时间，一直监听到进程被结束。
    let timeout = match num_opt(&opts, "timeout", 0) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // 统计窗口：窗口内的数据照收不误，但统计按这个粒度刷新。
    // 之所以不拿它做「窗口内聚合」：聚合会把数据压到窗口末尾才吐出来，
    // 流式消费方要的恰恰是「一到就出」。
    let interval = match num_opt(&opts, "interval", 1000) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // 轮询粒度下限 1ms：0 会退化成忙等，白白吃满一个核。
    let poll = Duration::from_millis(interval.max(1));

    let start = Instant::now();
    let link = match Link::open(&name, cfg) {
        Ok(l) => l,
        Err(e) => return fail(json, EX_OPEN, &format!("打开 {name} 失败：{e}")),
    };

    let deadline = if timeout > 0 {
        Some(start + Duration::from_millis(timeout))
    } else {
        None
    };

    // 每来一块就立刻冲一行出去：管道下游才能边收边处理。
    let (total, chunks) = match watch_loop(&link, deadline, poll, |d| {
        if json {
            emit_raw(&watch_chunk_json(now_millis(), d));
        } else {
            println!("rx_hex: {}", fmt::hex_bytes(d));
            println!("rx_text: {}", fmt::preview_text(d));
        }
    }) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_RUN, &format!("接收 {name} 失败：{e}")),
    };

    let elapsed = start.elapsed().as_millis();
    if json {
        emit_raw(&watch_summary_json(total, chunks, elapsed));
    } else {
        println!("合计 {total} 字节 / {chunks} 块 / 耗时 {elapsed} ms");
    }
    EX_OK
}

/// `record` 的逐块可读输出写到哪里。
///
/// `--stdout` 打开后 stdout 被原始字节占住，可读输出只能改走 stderr：
/// 否则 `stui record --port COM3 --stdout > raw.bin` 得到的文件里
/// 会混进几行 `rx_hex: ...`，那就不是原始报文了。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkOut {
    Stdout,
    Stderr,
}

/// `record` 的收包落盘循环：收到一块写一块，返回 `(总字节数, 块数)`。
///
/// 拆出来是为了能在单元测试里用 sim 回环当数据源直接驱动，
/// 从而验证「收到的字节确实原样写进了文件」——这正是 record 的全部意义。
/// 写文件失败一律上抛：静默丢数据比停下来更糟。
fn record_loop(
    link: &Link,
    file: &mut std::fs::File,
    format: OutFormat,
    deadline: Option<Instant>,
) -> Result<(u64, u64), String> {
    let mut noop = |_: &[u8]| -> Result<(), String> { Ok(()) };
    record_loop_with(link, file, format, ChunkOut::Stdout, &mut noop, deadline)
}

/// [`record_loop`] 的完整版本：额外的两个参数决定「可读输出走哪」与
/// 「原始字节要不要再冲一份到别处」。
///
/// `tee` 是 `--stdout` 的注入点：传一个空回调就是旧行为（只落盘），
/// 传真回调就把同一批字节原样再写一遍（单元测试里用数组收集，
/// 命令行里写 stdout）。
fn record_loop_with(
    link: &Link,
    file: &mut std::fs::File,
    format: OutFormat,
    out: ChunkOut,
    tee: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    deadline: Option<Instant>,
) -> Result<(u64, u64), String> {
    let mut total = 0u64;
    let mut chunks = 0u64;
    loop {
        // 到点就收工；没给时限时一直收到对端关闭或进程被结束。
        let wait = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                left.min(Duration::from_millis(20))
            }
            None => Duration::from_millis(20),
        };
        match link.recv(wait)? {
            Chunk::Data(d) => {
                // 原样落盘：不做解码、不做转义，字节级一致。
                file.write_all(&d)
                    .map_err(|e| format!("写入记录文件失败：{e}"))?;
                // 第二个去处（`--stdout`）：同样字节级原样，不加任何东西。
                tee(&d)?;
                total += d.len() as u64;
                chunks += 1;
                match (out, format) {
                    (ChunkOut::Stdout, OutFormat::Text) => println!("rx_hex: {}", fmt::hex_bytes(&d)),
                    (ChunkOut::Stdout, OutFormat::Ndjson) => {
                        emit_raw(&record_chunk_json(now_millis(), &d))
                    }
                    (ChunkOut::Stderr, OutFormat::Text) => eprintln!("rx_hex: {}", fmt::hex_bytes(&d)),
                    (ChunkOut::Stderr, OutFormat::Ndjson) => {
                        eprintln!("{}", record_chunk_json(now_millis(), &d))
                    }
                }
            }
            Chunk::Idle => {}
            Chunk::Closed => break,
        }
    }
    Ok((total, chunks))
}

/// `record`：把收到的数据原样落盘。
///
/// 与 `watch` 的区别是「多一个目的地」：watch 只往 stdout 吐，
/// record 同时往文件里写。文件里的是**字节级原样**的数据 ——
/// 不解码、不转义、不补换行，事后能直接拿去比对原始报文。
fn cmd_record(args: &[String]) -> i32 {
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_RECORD) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    let json = opts.contains_key("json");

    let name = match str_req(&opts, "port") {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let cfg = match build_cfg(&opts, &name) {
        Ok(c) => c,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // 总时长：0 表示不设上限，一直录到进程被结束。
    let duration = match num_opt(&opts, "duration", 5000) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // `--format` 决定「每块数据」的输出形状，`--json` 决定汇总行的形状。
    // 只给 `--json` 时按 NDJSON 输出：这样「--json ⇒ stdout 全是 JSON 行」
    // 这条约定不会被破坏。
    let format = match opts.get("format") {
        Some(v) => match parse_format(v) {
            Ok(f) => f,
            Err(e) => return fail(json, EX_USAGE, &e),
        },
        None if json => OutFormat::Ndjson,
        None => OutFormat::Text,
    };
    // `--json` ⇒ stdout 全是 JSON 行：`--format text` 的每块数据是人看的文本，
    // 混在 JSON 行里会让按行解析的下游崩掉。
    // 两个参数互相矛盾，直接报用法错误（退出码 2），不猜调用方想要哪个。
    if json && format == OutFormat::Text {
        return fail(
            json,
            EX_USAGE,
            "--json 与 --format text 不能同时使用：--json 时 stdout 全是 JSON 行（每块数据用 --format ndjson）",
        );
    }
    // `--stdout`：除了落盘，还把收到的数据**原样**写到 stdout（字节级，不加任何东西）。
    // 这样脚本就能用 `stui record --port COM3 --stdout > raw.bin` 直接拿到原始报文。
    let to_stdout = opts.contains_key("stdout");
    // `--stdout` 与 `--json` 互相矛盾：stdout 会被原始字节占住，
    // JSON 汇总再写进去就混在一起了。两个参数不能猜调用方想要哪个，
    // 直接报用法错误（退出码 2）。
    if json && to_stdout {
        return fail(
            json,
            EX_USAGE,
            "--stdout 与 --json 不能同时使用：--stdout 时 stdout 只放原始字节，JSON 汇总请去掉 --stdout（数据仍会原样落盘到 --out）",
        );
    }
    // 没给 --out 时用默认文件名，落在当前目录。
    let out_path = record_out_path(
        opts.get("out").map(String::as_str),
        &fmt::stamp(SystemTime::now()),
    );

    let start = Instant::now();
    let link = match Link::open(&name, cfg) {
        Ok(l) => l,
        Err(e) => return fail(json, EX_OPEN, &format!("打开 {name} 失败：{e}")),
    };
    // 输出文件打不开属于运行错误：路径写错、目录不存在、磁盘满……
    // 提前失败好过录了半天才发现没写进去。
    let mut file = match std::fs::File::create(&out_path) {
        Ok(f) => f,
        Err(e) => return fail(json, EX_RUN, &format!("创建记录文件 {out_path} 失败：{e}")),
    };

    let deadline = if duration > 0 {
        Some(start + Duration::from_millis(duration))
    } else {
        None
    };

    // `--stdout` 的写入端：原样字节，一块一块冲出去（管道下游才能边收边处理）。
    let mut stdout_sink = |bytes: &[u8]| -> Result<(), String> {
        let mut so = io::stdout().lock();
        so.write_all(bytes)
            .map_err(|e| format!("写入 stdout 失败：{e}"))?;
        so.flush().map_err(|e| format!("刷新 stdout 失败：{e}"))
    };
    // stdout 被原始字节占住时，逐块的可读输出改走 stderr。
    let chunk_out = if to_stdout {
        ChunkOut::Stderr
    } else {
        ChunkOut::Stdout
    };
    let mut noop_sink = |_: &[u8]| -> Result<(), String> { Ok(()) };
    let tee: &mut dyn FnMut(&[u8]) -> Result<(), String> = if to_stdout {
        &mut stdout_sink
    } else {
        &mut noop_sink
    };

    // 不用 `--stdout` 时走旧的那条路径：逐块可读输出照旧进 stdout，
    // 行为与以前一字不差。
    let res = if to_stdout {
        record_loop_with(&link, &mut file, format, chunk_out, tee, deadline)
    } else {
        record_loop(&link, &mut file, format, deadline)
    };
    let (total, chunks) = match res {
        Ok(v) => v,
        Err(e) => return fail(json, EX_RUN, &format!("{e}（记录文件 {out_path}）")),
    };
    // 收尾前先 flush：进程被杀时缓冲区里的数据会丢，
    // 落盘的承诺就打了折扣。
    if let Err(e) = file.flush() {
        return fail(json, EX_RUN, &format!("刷新记录文件 {out_path} 失败：{e}"));
    }
    let elapsed = start.elapsed().as_millis();

    if json {
        emit_raw(&record_summary_json(&name, &out_path, total, chunks, elapsed));
    } else if to_stdout {
        // stdout 归原始字节所有：汇总行只能走 stderr。
        eprintln!(
            "已记录 {} / {} 块 → {out_path}，耗时 {elapsed} ms",
            fmt::human_bytes(total),
            chunks
        );
    } else {
        println!(
            "已记录 {} / {} 块 → {out_path}，耗时 {elapsed} ms",
            fmt::human_bytes(total),
            chunks
        );
    }
    EX_OK
}

/// 一条自检结论：名字 + 是否通过 + 给人看的说明。
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

/// 发一段载荷再收回同样长度的数据（走 sim 回环）。
///
/// 复用 [`collect`]：自检要验证的是「链路通不通」，
/// 收包语义必须和 `listen` / `watch` 完全一致，否则测的是另一条路径。
fn echo_roundtrip(link: &mut Link, payload: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    link.write(payload).map_err(|e| format!("写入失败：{e}"))?;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    collect(link, deadline, Some(payload.len()), None, &mut buf)?;
    Ok(buf)
}

/// 跑一遍自检检查项，返回各项结论。
///
/// 拆出来是为了能在单元测试里直接驱动（不需要捕获 stdout）：
/// 检查项本身才是自检的价值所在，输出只是它的呈现。
fn selftest_checks() -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();

    let cfg = PortCfg {
        port: "sim".to_string(),
        baud: 115_200,
        bits: Bits::Eight,
        parity: Parity::None,
        stops: Stops::One,
        flow: Flow::None,
    };
    let mut link = match Link::open("sim", cfg) {
        Ok(l) => l,
        Err(e) => {
            checks.push(Check {
                name: "sim-open",
                ok: false,
                detail: format!("启动回环虚拟设备失败：{e}"),
            });
            return checks;
        }
    };
    checks.push(Check {
        name: "sim-open",
        ok: true,
        detail: "已打开内置回环虚拟设备".to_string(),
    });

    // 两段载荷刻意长度不同、内容不同：长度不同才能暴露「上一段的残留字节」。
    const FIRST: &[u8] = b"stui-selftest-A";
    const SECOND: &[u8] = b"\x00\x01\x02STB";

    let first = echo_roundtrip(&mut link, FIRST, Duration::from_millis(2000));
    let first_ok = matches!(&first, Ok(v) if v.as_slice() == FIRST);
    checks.push(Check {
        name: "echo-match",
        ok: first_ok,
        detail: match &first {
            Ok(v) if v.as_slice() == FIRST => format!("回显 {} 字节，与发送完全一致", v.len()),
            Ok(v) => format!(
                "回显 {} 字节（期望 {}，实际 {}）",
                v.len(),
                hex_compact(FIRST),
                hex_compact(v)
            ),
            Err(e) => format!("回显失败：{e}"),
        },
    });

    let second = echo_roundtrip(&mut link, SECOND, Duration::from_millis(2000));
    let second_ok = matches!(&second, Ok(v) if v.as_slice() == SECOND);
    checks.push(Check {
        name: "no-crosstalk",
        ok: second_ok,
        detail: match &second {
            Ok(v) if v.as_slice() == SECOND => {
                format!("第二段回显 {} 字节，与第一段不串味", v.len())
            }
            Ok(v) => format!(
                "第二段回显 {} 字节（期望 {}，实际 {}）",
                v.len(),
                hex_compact(SECOND),
                hex_compact(v)
            ),
            Err(e) => format!("第二段回显失败：{e}"),
        },
    });

    checks
}

/// `selftest`：用内置 sim 回环自检整条链路（不开串口、不 spawn 子进程）。
///
/// 检查项：
/// 1. 回环设备能打开；
/// 2. 第一段已知报文的回显与发送逐字节一致；
/// 3. 再发一段不同的报文，回显仍然是第二段（不串味）。
fn cmd_selftest(args: &[String]) -> i32 {
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_SELFTEST) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    let json = opts.contains_key("json");
    let _ = opts;

    let start = Instant::now();
    let checks = selftest_checks();
    let all_ok = checks.iter().all(|c| c.ok);
    let elapsed = start.elapsed().as_millis();

    if json {
        let items: Vec<String> = checks
            .iter()
            .map(|c| {
                let mut o = Obj::new();
                o.set("name", c.name);
                o.flag("ok", c.ok);
                o.set("detail", &c.detail);
                o.json()
            })
            .collect();
        let mut o = Obj::new();
        o.flag("ok", all_ok);
        o.raw("checks", &format!("[{}]", items.join(",")));
        o.num("elapsed_ms", elapsed);
        emit(o);
    } else {
        for c in &checks {
            println!("{} {}：{}", if c.ok { "✓" } else { "✗" }, c.name, c.detail);
        }
        if all_ok {
            println!("自检通过，耗时 {elapsed} ms");
        } else {
            eprintln!("错误：自检未通过");
        }
    }
    if all_ok { EX_OK } else { EX_RUN }
}

fn cmd_sim(args: &[String]) -> i32 {
    let json_err = wants_json(args);
    let opts = match parse_args(args, SPEC_SIM) {
        Ok(o) => o,
        Err(e) => return fail(json_err, EX_USAGE, &e),
    };
    let json = opts.contains_key("json");

    let pattern = match opts.get("pattern") {
        Some(p) => match fmt::parse_send_hex(p) {
            Ok(v) => v,
            Err(e) => return fail(json, EX_USAGE, &e),
        },
        None => Vec::new(),
    };
    let count = match num_opt(&opts, "count", 1) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let interval = match num_opt(&opts, "interval", 500) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    let duration = match num_opt(&opts, "duration", 5000) {
        Ok(v) => v,
        Err(e) => return fail(json, EX_USAGE, &e),
    };
    // 回显是回环设备与生俱来的行为，没有开关：
    // 之前参数表里挂着 `--echo`，可它既没有第二个取值（`--no-echo` 不存在），
    // 写不写都是回显 —— 与其留一个不生效的参数让人以为它管点什么，
    // 不如从参数表里删掉：误用会立刻报「未知参数」。

    let link = match Link::open(
        "sim",
        PortCfg {
            port: "sim".to_string(),
            baud: 115_200,
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
        },
    ) {
        Ok(l) => l,
        Err(e) => return fail(json, EX_OPEN, &format!("启动回环虚拟设备失败：{e}")),
    };

    // 就绪信号写 stderr 并 flush，stdout 只在结束时输出统计。
    eprintln!("stui-sim ready");
    let _ = io::stderr().flush();

    let start = Instant::now();
    let deadline = start + Duration::from_millis(duration);
    let patterns = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));

    // pattern 线程：启动后立即发第一帧，之后按 `--interval` 重复。
    if !pattern.is_empty() && count > 0 {
        let Some(tx) = link.echo_tx() else {
            return fail(json, EX_RUN, "虚拟设备不支持主动输出");
        };
        let sent = Arc::clone(&patterns);
        let stop = Arc::clone(&finished);
        if let Err(e) = thread::Builder::new()
            .name("sim-pattern".into())
            .spawn(move || {
                for _ in 0..count {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if tx.send(pattern.clone()).is_err() {
                        break;
                    }
                    sent.fetch_add(1, Ordering::Relaxed);
                    if interval > 0 {
                        thread::sleep(Duration::from_millis(interval));
                    }
                }
            })
        {
            return fail(json, EX_RUN, &format!("启动 pattern 线程失败：{e}"));
        }
    }

    // 主循环：收到即按 `--echo` 原样回送，直到 `--duration` 到点。
    let mut echoed_rx = 0u64;
    let mut echoed_tx = 0u64;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match link.recv(left.min(Duration::from_millis(20))) {
            Ok(Chunk::Data(d)) => {
                echoed_rx += d.len() as u64;
                // 回环设备的 TX 接的是对端 RX，而对端不在本进程内，
                // 所以回显只在此记账：若再写回自己的 rx 会自激成死循环。
                echoed_tx += d.len() as u64;
            }
            Ok(Chunk::Idle) => {}
            Ok(Chunk::Closed) => break,
            Err(e) => return fail(json, EX_RUN, &format!("回环接收失败：{e}")),
        }
    }
    finished.store(true, Ordering::Relaxed);

    let rounds = patterns.load(Ordering::Relaxed);
    if json {
        let mut o = Obj::new();
        o.flag("ok", true);
        o.set("mode", "sim");
        o.num("echoed_rx", echoed_rx);
        o.num("echoed_tx", echoed_tx);
        o.num("patterns", rounds);
        o.num("duration_ms", duration);
        emit(o);
    } else {
        println!(
            "回环虚拟设备已停止：收到 {}，回显 {}，pattern {} 次，时长 {duration} ms",
            fmt::human_bytes(echoed_rx),
            fmt::human_bytes(echoed_tx),
            rounds
        );
    }
    EX_OK
}

/// 入口：参数为 `stui <子命令> ...`，返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        return fail(
            false,
            EX_USAGE,
            "缺少子命令（list / ports / send / listen / watch / record / selftest / sim）",
        );
    };
    let rest = &args[1..];
    match sub.as_str() {
        "list" | "ports" => cmd_list(rest),
        "send" => cmd_send(rest),
        "listen" => cmd_listen(rest),
        "watch" => cmd_watch(rest),
        "record" => cmd_record(rest),
        "selftest" => cmd_selftest(rest),
        "sim" => cmd_sim(rest),
        other => fail(
            false,
            EX_USAGE,
            &format!(
                "未知子命令：{other}（可用：list / ports / send / listen / watch / record / selftest / sim）"
            ),
        ),
    }
}

// ---------------------------------------------------------------- 单元测试
//
// 这里只测**不需要真实串口**的纯逻辑：参数解析、载荷拼装、端口分类、JSON 拼装。
// 需要真实设备的路径（收包、发送）由 tests/cli_contract.rs 走 sim 回环覆盖。

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份选项表。
    fn opts(pairs: &[(&str, &str)]) -> Opts {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// 造一份命令行参数。
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// 写一个临时文件，返回路径。
    fn temp_file(name: &str, content: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("stui-cli-test-{}-{name}", std::process::id()));
        std::fs::write(&p, content).expect("写临时文件失败");
        p
    }

    // ---------------------------------------------------------- build_payload

    #[test]
    fn payload_from_text() {
        let o = opts(&[("text", "hi\\r\\n")]);
        assert_eq!(build_payload(&o).unwrap(), b"hi\r\n");
    }

    #[test]
    fn payload_from_hex() {
        let o = opts(&[("hex", "41 42")]);
        assert_eq!(build_payload(&o).unwrap(), b"AB");
    }

    #[test]
    fn payload_from_file() {
        let path = temp_file("file.bin", b"\x01\x02\x03");
        let o = opts(&[("file", path.to_str().unwrap())]);
        assert_eq!(build_payload(&o).unwrap(), vec![1, 2, 3]);
        let _ = std::fs::remove_file(&path);
    }

    /// 三种来源同时给定时，顺序必须是 file → hex → text。
    #[test]
    fn payload_combines_in_fixed_order() {
        let path = temp_file("combo.bin", b"\xF1");
        let o = opts(&[
            ("text", "T"),
            ("hex", "4849"),
            ("file", path.to_str().unwrap()),
        ]);
        // 文件在前、HEX 居中、文本在后
        assert_eq!(build_payload(&o).unwrap(), vec![0xF1, 0x48, 0x49, b'T']);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn payload_requires_a_source() {
        let e = build_payload(&opts(&[])).unwrap_err();
        assert!(e.contains("--text/--hex/--file"), "错误信息应指明来源参数：{e}");
    }

    /// 空载荷是用法错误：发 0 字节没有意义。
    #[test]
    fn payload_rejects_empty_result() {
        assert!(build_payload(&opts(&[("text", "")])).is_err());
        assert!(build_payload(&opts(&[("hex", "")])).is_err());
        let path = temp_file("empty.bin", b"");
        assert!(build_payload(&opts(&[("file", path.to_str().unwrap())])).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn payload_reports_bad_hex() {
        let e = build_payload(&opts(&[("hex", "ABC")])).unwrap_err();
        assert!(e.contains("十六进制"), "错误信息应说明位数问题：{e}");
    }

    // -------------------------------------------------------------- parse_args

    #[test]
    fn parse_args_reads_flags_and_values() {
        let o = parse_args(&args(&["--port", "sim", "--json"]), SPEC_SEND).unwrap();
        assert_eq!(o.get("port").map(String::as_str), Some("sim"));
        assert_eq!(o.get("json").map(String::as_str), Some("1"));
        // 没给的参数不出现在表里
        assert!(o.get("baud").is_none());
    }

    #[test]
    fn parse_args_rejects_unknown_option() {
        let e = parse_args(&args(&["--nope"]), SPEC_SEND).unwrap_err();
        assert!(e.contains("未知参数"), "应报未知参数：{e}");
        assert!(e.contains("--nope"), "错误信息应带上参数名：{e}");
    }

    #[test]
    fn parse_args_rejects_missing_value() {
        let e = parse_args(&args(&["--port"]), SPEC_SEND).unwrap_err();
        assert!(e.contains("--port 需要一个取值"), "应报缺值：{e}");
    }

    /// 不以 `--` 开头的裸词同样是未知参数。
    #[test]
    fn parse_args_rejects_bare_word() {
        let e = parse_args(&args(&["sim"]), SPEC_SEND).unwrap_err();
        assert!(e.contains("未知参数"), "应报未知参数：{e}");
    }

    #[test]
    fn num_opt_rejects_negative_and_non_numeric() {
        let o = opts(&[("baud", "-1")]);
        assert!(num_opt(&o, "baud", 0).is_err());
        let o = opts(&[("baud", "abc")]);
        assert!(num_opt(&o, "baud", 0).is_err());
        let o = opts(&[("baud", "9600")]);
        assert_eq!(num_opt(&o, "baud", 115_200).unwrap(), 9600);
        // 缺省用默认值
        assert_eq!(num_opt(&opts(&[]), "baud", 115_200).unwrap(), 115_200);
    }

    #[test]
    fn wants_json_scans_whole_argv() {
        assert!(wants_json(&args(&["--port", "sim", "--json"])));
        assert!(!wants_json(&args(&["--port", "sim"])));
    }

    // -------------------------------------------------------------- port_kind

    #[test]
    fn port_kind_classifies_by_description() {
        assert_eq!(port_kind("蓝牙串口"), "bluetooth");
        assert_eq!(port_kind("PCI 串口"), "pci");
        assert_eq!(port_kind("CH340 USB 串口"), "usb");
        assert_eq!(port_kind(""), "usb");
    }

    #[test]
    fn kind_matches_filters_ports() {
        assert!(kind_matches("蓝牙串口", None));
        assert!(kind_matches("蓝牙串口", Some("bluetooth")));
        assert!(!kind_matches("蓝牙串口", Some("usb")));
        assert!(kind_matches("CH340", Some("usb")));
        assert!(kind_matches("PCI 串口", Some("pci")));
    }

    // ------------------------------------------------------------- dry-run

    /// dry-run 只解析不发送：`sent` 恒为 0，`planned` / `planned_hex` 描述将要发的字节。
    #[test]
    fn dry_run_json_reports_planned_payload() {
        let j = send_json(&SendReport {
            port: "sim",
            baud: 115_200,
            sent: 0,
            rx: &[],
            elapsed: 7,
            planned: Some(&[0x41, 0x42]),
            expect: None,
            ok: true,
            error: None,
        });
        assert_eq!(
            j,
            r#"{"ok":true,"port":"sim","baud":115200,"sent":0,"planned":2,"planned_hex":"4142","received":0,"rx_hex":"","rx_text":"","elapsed_ms":7,"expect_ok":true,"expect_hex":"","expect_checked":false,"expect_missing":[]}"#
        );
    }

    /// 普通 send 的 JSON 形状不能因为抽出 `send_json` 而改变。
    ///
    /// 唯一的变化是末尾多了 expect_ok / expect_hex 两个字段：
    /// 它们是 `--expect` 的固定槽位，没有用 `--expect` 时分别是 true 与空串。
    /// （后来又补了 `expect_missing`：同样是固定槽位，全部命中时是空数组。）
    #[test]
    fn send_json_keeps_original_shape() {
        let j = send_json(&SendReport {
            port: "sim",
            baud: 115_200,
            sent: 4,
            rx: b"hi",
            elapsed: 0,
            planned: None,
            expect: None,
            ok: true,
            error: None,
        });
        assert_eq!(
            j,
            r#"{"ok":true,"port":"sim","baud":115200,"sent":4,"received":2,"rx_hex":"6869","rx_text":"hi","elapsed_ms":0,"expect_ok":true,"expect_hex":"","expect_checked":false,"expect_missing":[]}"#
        );
        assert!(!j.contains("planned"), "普通 send 不应出现 planned 字段");
    }

    #[test]
    fn dry_run_payload_parsing_matches_build_payload() {
        let o = opts(&[("hex", "41 42"), ("text", "C")]);
        let planned = build_payload(&o).unwrap();
        assert_eq!(planned, vec![0x41, 0x42, b'C']);
        assert_eq!(hex_compact(&planned), "414243");
        // 空载荷在 dry-run 下同样是用法错误
        assert!(build_payload(&opts(&[("text", "")])).is_err());
    }

    // ------------------------------------------------------ JSON / 转义格式

    #[test]
    fn escape_handles_quotes_backslash_and_controls() {
        assert_eq!(escape("abc"), "\"abc\"");
        assert_eq!(escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(escape("a\\b"), "\"a\\\\b\"");
        assert_eq!(escape("a\r\n"), "\"a\\u000D\\u000A\"");
        assert_eq!(escape("a\u{7f}"), "\"a\\u007F\"");
        // 中文原样保留（不做 \u 转义）
        assert_eq!(escape("温度"), "\"温度\"");
    }

    #[test]
    fn obj_emits_single_line_json_in_insertion_order() {
        let mut o = Obj::new();
        o.flag("ok", true);
        o.set("port", "sim");
        o.num("n", 3);
        o.flag("f", false);
        o.raw("arr", "[1,2]");
        let json = o.json();
        assert_eq!(json, r#"{"ok":true,"port":"sim","n":3,"f":false,"arr":[1,2]}"#);
        assert!(!json.contains('\n'), "JSON 必须是单行");
    }

    #[test]
    fn hex_compact_is_uppercase_and_contiguous() {
        assert_eq!(hex_compact(&[0x00, 0x0A, 0xFF]), "000AFF");
        assert_eq!(hex_compact(&[]), "");
        // 与面向人眼的带空格版本不同：JSON 里要紧凑
        assert_eq!(fmt::hex_bytes(&[0x41, 0x42]), "41 42");
    }

    #[test]
    fn watch_chunk_line_matches_ndjson_contract() {
        assert_eq!(
            watch_chunk_json(1700000000123, &[0x41, 0x42]),
            r#"{"ok":true,"t":1700000000123,"len":2,"rx_hex":"4142","rx_text":"AB"}"#
        );
    }

    #[test]
    fn watch_summary_line_matches_ndjson_contract() {
        assert_eq!(
            watch_summary_json(12, 3, 250),
            r#"{"ok":true,"done":true,"total":12,"chunks":3,"elapsed_ms":250}"#
        );
    }

    #[test]
    fn selftest_check_json_shape() {
        let checks = [Check {
            name: "echo-match",
            ok: true,
            detail: "回显 4 字节，与发送完全一致".to_string(),
        }];
        let items: Vec<String> = checks
            .iter()
            .map(|c| {
                let mut o = Obj::new();
                o.set("name", c.name);
                o.flag("ok", c.ok);
                o.set("detail", &c.detail);
                o.json()
            })
            .collect();
        assert_eq!(
            items[0],
            r#"{"name":"echo-match","ok":true,"detail":"回显 4 字节，与发送完全一致"}"#
        );
    }

    // ------------------------------------------------------ watch 收包循环

    /// watch 的收包循环：来一块就交一块，统计也对得上。
    ///
    /// 用进程内 sim 回环当数据源：另一个线程往回环里塞一段数据，
    /// 循环应当原样收到并计数。不碰真实串口。
    #[test]
    fn watch_loop_receives_chunks_and_counts_them() {
        let cfg = PortCfg {
            port: "sim".to_string(),
            baud: 115_200,
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
        };
        let link = Link::open("sim", cfg).unwrap();
        let tx = link.echo_tx().expect("sim 设备应支持主动输出");
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = tx.send(vec![0x41, 0x42]);
        });

        let mut got: Vec<Vec<u8>> = Vec::new();
        let deadline = Some(Instant::now() + Duration::from_millis(400));
        let (total, chunks) =
            watch_loop(&link, deadline, Duration::from_millis(20), |d| got.push(d.to_vec()))
                .unwrap();
        feeder.join().unwrap();

        assert_eq!(chunks, 1, "应当只收到一块数据");
        assert_eq!(total, 2, "总字节数应当是 2");
        assert_eq!(got[0], vec![0x41, 0x42], "收到的数据应与送入的一致");
    }

    /// 没有数据源时循环只跑到截止时间，不产出数据也不报错。
    #[test]
    fn watch_loop_without_data_reaches_deadline() {
        let cfg = PortCfg {
            port: "sim".to_string(),
            baud: 115_200,
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
        };
        let link = Link::open("sim", cfg).unwrap();
        let deadline = Some(Instant::now() + Duration::from_millis(120));
        let (total, chunks) =
            watch_loop(&link, deadline, Duration::from_millis(20), |_| unreachable!("不该有数据"))
                .unwrap();
        assert_eq!((total, chunks), (0, 0));
    }

    // ------------------------------------------------------- selftest 检查项

    /// 自检走的是 sim 回环：三项检查都应当通过。
    #[test]
    fn selftest_checks_pass_on_sim_loopback() {
        let checks = selftest_checks();
        let detail: Vec<String> = checks.iter().map(|c| format!("{}: {}", c.name, c.detail)).collect();
        assert_eq!(checks.len(), 3, "应有 3 项检查：{detail:?}");
        assert!(checks.iter().all(|c| c.ok), "自检应全部通过：{detail:?}");
    }

    // ------------------------------------------------------------ 子命令分派

    /// `ports` 是 `list` 的别名，走同一条代码路径。
    #[test]
    fn ports_is_an_alias_of_list() {
        // 两个名字在 run() 里必须落到同一个 cmd_list，退出码完全一致。
        let a = run(&args(&["list", "--json"]));
        let b = run(&args(&["ports", "--json"]));
        assert_eq!(a, b);
        // 枚举串口失败属于环境问题（退出码 3），不是别名行为不一致。
        assert!(a == EX_OK || a == EX_OPEN, "list 的退出码异常：{a}");
    }

    /// 未知子命令是用法错误（退出码 2）。
    #[test]
    fn unknown_subcommand_is_usage_error() {
        assert_eq!(run(&args(&["bogus"])), EX_USAGE);
        assert_eq!(run(&args(&[])), EX_USAGE);
    }

    // -------------------------------------------------------------- --expect

    /// `--expect` 的目标走既有的十六进制规则：`41 42` 与 `0x41-0x42` 都认。
    #[test]
    fn expect_parses_with_the_shared_hex_rules() {
        assert_eq!(parse_expect("41 42").unwrap(), vec![0x41, 0x42]);
        assert_eq!(parse_expect("0x41-0x42").unwrap(), vec![0x41, 0x42]);
        // 偶数位就是合法的一个字节。
        assert_eq!(parse_expect("41").unwrap(), vec![0x41]);
        // 奇数位（"ABC"）必须报错。
        assert!(parse_expect("ABC").is_err(), "奇数位必须报错");
        // 空目标没有可校验的内容。
        assert!(parse_expect("").is_err());
    }

    /// 奇数位的 `--expect`（"ABC"）必须报参数错误。
    #[test]
    fn expect_reports_bad_hex_with_a_precise_error() {
        let e = parse_expect("ABC").unwrap_err();
        assert!(e.contains("ABC"), "错误信息要指出出错的标记：{e}");
        assert!(e.contains("十六进制") || e.contains("奇数"), "错误信息要说明原因：{e}");
        assert!(e.contains("--expect"), "错误信息要指明是哪个参数：{e}");
    }

    /// `--expect` 写错时整条命令以用法错误（退出码 2）收场。
    #[test]
    fn send_rejects_bad_expect_hex() {
        assert_eq!(
            run(&args(&[
                "send", "--port", "sim", "--text", "hi", "--expect", "ABC", "--json"
            ])),
            EX_USAGE
        );
    }

    /// `--expect-timeout` 没有 `--expect` 就是一句没有主语的指令：报用法错误。
    #[test]
    fn send_rejects_expect_timeout_without_expect() {
        assert_eq!(
            run(&args(&[
                "send", "--port", "sim", "--text", "hi", "--expect-timeout", "100", "--json"
            ])),
            EX_USAGE
        );
    }

    /// `--expect` 与 `--dry-run` 同用：只解析并校验目标，不真的发送。
    ///
    /// 证据是「连一个根本不存在的端口都能成功」：dry-run 不打开串口。
    #[test]
    fn dry_run_with_expect_never_opens_the_port() {
        assert_eq!(
            run(&args(&[
                "send",
                "--port",
                "COM_NO_SUCH_PORT_9999",
                "--text",
                "hi",
                "--expect",
                "41 42",
                "--dry-run",
                "--json"
            ])),
            EX_OK
        );
        // 但目标写错时仍然是参数错误：校验目标这件事不能被 dry-run 免掉。
        assert_eq!(
            run(&args(&[
                "send", "--port", "sim", "--text", "hi", "--expect", "ABC", "--dry-run", "--json"
            ])),
            EX_USAGE
        );
    }

    /// send 的 JSON 里恒有 expect_ok / expect_hex 两个字段。
    #[test]
    fn send_json_always_carries_expect_fields() {
        let e = Expect {
            needles: vec![vec![0x41, 0x42]],
            ok: false,
            checked: true,
            missing: vec![vec![0x41, 0x42]],
        };
        let j = send_json(&SendReport {
            port: "sim",
            baud: 115_200,
            sent: 2,
            rx: b"XY",
            elapsed: 5,
            planned: None,
            expect: Some(&e),
            ok: false,
            error: Some("期望的响应未出现"),
        });
        assert!(j.starts_with(r#"{"ok":false,"error":"期望的响应未出现""#), "失败时 ok 应为 false：{j}");
        assert!(j.contains(r#""expect_ok":false"#), "不匹配时 expect_ok 应为 false：{j}");
        assert!(j.contains(r#""expect_hex":"4142""#), "expect_hex 应是目标字节：{j}");
        assert!(j.contains(r#""expect_checked":true"#), "真校验过时应为 true：{j}");
        // 差在哪一段也要说清楚：多期望时脚本才知道该等什么。
        assert!(
            j.contains(r#""expect_missing":["4142"]"#),
            "expect_missing 应列出没命中的期望：{j}"
        );
    }

    /// 期望序列可能跨块边界，所以要在整个缓冲区上找。
    #[test]
    fn contains_bytes_finds_subsequence_across_chunks() {
        assert!(contains_bytes(b"\x00\x41\x42\xFF", b"AB"));
        assert!(!contains_bytes(b"\x00\x41\xFF\x42", b"AB"));
        assert!(!contains_bytes(b"AB", b"ABC"));
        assert!(!contains_bytes(b"", b"AB"));
    }

    // ------------------------------------------------- 既有参数行为不变

    /// `--repeat 0` 仍然是用法错误（退出码 2）。
    #[test]
    fn repeat_zero_stays_a_usage_error() {
        assert_eq!(
            run(&args(&[
                "send", "--port", "sim", "--text", "hi", "--repeat", "0", "--json"
            ])),
            EX_USAGE
        );
    }

    /// dry-run 只跳过「发送 / 等待」，**不跳过参数校验**：
    /// 同一份参数在 dry-run 下也必须是退出码 2。
    #[test]
    fn dry_run_still_rejects_invalid_send_parameters() {
        // --bytes 不是非负整数
        assert_eq!(
            run(&args(&[
                "send", "--dry-run", "--text", "hi", "--bytes", "abc", "--json"
            ])),
            EX_USAGE
        );
        // --repeat 为 0
        assert_eq!(
            run(&args(&[
                "send", "--dry-run", "--text", "hi", "--repeat", "0", "--json"
            ])),
            EX_USAGE
        );
        // --wait 不是数字
        assert_eq!(
            run(&args(&[
                "send", "--dry-run", "--text", "hi", "--wait", "soon", "--json"
            ])),
            EX_USAGE
        );
    }

    /// `--json` ⇒ stdout 全是 JSON 行：`--format text` 与它互相矛盾，报用法错误。
    #[test]
    fn record_rejects_json_with_text_format() {
        assert_eq!(
            run(&args(&[
                "record", "--port", "sim", "--duration", "10", "--format", "text", "--json"
            ])),
            EX_USAGE
        );
    }

    /// `sim` 没有 `--echo` 这个参数（回显是回环设备自带的，没有开关）：
    /// 误用要立刻报「未知参数」，而不是写个不生效的参数。
    #[test]
    fn sim_rejects_the_echo_flag() {
        assert_eq!(run(&args(&["sim", "--echo"])), EX_USAGE);
    }

    /// `--kind` 的非法值仍然是用法错误（退出码 2）。
    #[test]
    fn list_rejects_unknown_kind() {
        assert_eq!(run(&args(&["list", "--kind", "nope", "--json"])), EX_USAGE);
    }

    /// `--baud` 越界仍然是用法错误：不能被悄悄截断成别的波特率。
    #[test]
    fn baud_out_of_range_stays_a_usage_error() {
        assert_eq!(
            run(&args(&[
                "send", "--port", "sim", "--baud", "4294967297", "--text", "hi", "--json"
            ])),
            EX_USAGE
        );
    }

    /// send 的选项表是它对外的契约：既有参数一个都不能少。
    #[test]
    fn send_spec_keeps_all_original_options() {
        for name in [
            "port", "baud", "bits", "parity", "stops", "flow", "text", "hex", "file", "repeat",
            "interval", "wait", "bytes", "dry-run", "json",
        ] {
            assert!(SPEC_SEND.iter().any(|(n, _)| *n == name), "SPEC_SEND 缺少 {name}");
        }
        // 新增的两个参数也在表里。
        assert!(SPEC_SEND.iter().any(|(n, _)| *n == "expect"));
        assert!(SPEC_SEND.iter().any(|(n, _)| *n == "expect-timeout"));
    }

    /// 新参数要被解析器认出来。
    #[test]
    fn parse_args_recognizes_new_options() {
        let o = parse_args(
            &args(&["--port", "sim", "--expect", "41", "--expect-timeout", "100", "--json"]),
            SPEC_SEND,
        )
        .unwrap();
        assert_eq!(o.get("expect").map(String::as_str), Some("41"));
        assert_eq!(o.get("expect-timeout").map(String::as_str), Some("100"));

        let o = parse_args(&args(&["--format", "ndjson", "--json"]), SPEC_LISTEN).unwrap();
        assert_eq!(o.get("format").map(String::as_str), Some("ndjson"));

        let o = parse_args(&args(&["--verbose", "--json"]), SPEC_LIST).unwrap();
        assert!(o.contains_key("verbose"));

        let o = parse_args(
            &args(&["--out", "x.log", "--duration", "0", "--format", "text", "--json"]),
            SPEC_RECORD,
        )
        .unwrap();
        assert_eq!(o.get("out").map(String::as_str), Some("x.log"));
        assert_eq!(o.get("duration").map(String::as_str), Some("0"));
    }

    /// `--expect-timeout` 是**回包的时限**：它比 `--wait` 小的时候不能被架空。
    ///
    /// 断言意图：早先的实现按 `--wait` 收满整个窗口后才判命中，
    /// `--expect-timeout 100 --wait 3000` 实际要等 3 秒 ——
    /// 「等多久算超时」与实际行为对不上，脚本只能干等。
    #[test]
    fn expect_timeout_is_not_overridden_by_wait() {
        let start = Instant::now();
        let code = run(&args(&[
            "send",
            "--port",
            "sim",
            "--hex",
            "41",
            "--expect",
            "FFFF",
            "--expect-timeout",
            "100",
            "--wait",
            "3000",
            "--json",
        ]));
        let elapsed = start.elapsed();
        assert_eq!(code, EX_EXPECT, "等不到期望应当是退出码 5（期望不符）");
        assert!(
            elapsed < Duration::from_millis(1500),
            "--expect-timeout 100 不该被 --wait 3000 架空，实际耗时 {elapsed:?}"
        );
    }

    // -------------------------------------------------------------- --format

    /// `--format` 只认 text 与 ndjson。
    #[test]
    fn format_accepts_text_and_ndjson_only() {
        assert!(matches!(parse_format("text").unwrap(), OutFormat::Text));
        assert!(matches!(parse_format("NDJSON").unwrap(), OutFormat::Ndjson));
        let e = parse_format("json").unwrap_err();
        assert!(e.contains("ndjson"), "错误信息要列出合法取值：{e}");
    }

    // --------------------------------------------------------------- record

    /// record 的默认文件名与默认路径的拼装。
    #[test]
    fn record_default_name_and_path() {
        let stamp = fmt::Stamp { y: 2026, mo: 10, d: 22, h: 1, mi: 2, s: 3, ms: 456 };
        // 默认名精确到毫秒：同一秒里跑两次不会撞成同一个文件。
        assert_eq!(record_default_name(&stamp), "stui-record-20261022-010203.456.log");
        // 没给 --out 时用默认名（当前目录下的相对路径）。
        assert_eq!(record_out_path(None, &stamp), "stui-record-20261022-010203.456.log");
        // 给了 --out 就用给定的路径。
        assert_eq!(record_out_path(Some("a/b.log"), &stamp), "a/b.log");
    }

    /// record 每块数据一行 NDJSON，汇总行是单行 JSON。
    #[test]
    fn record_json_shapes_match_contract() {
        assert_eq!(
            record_chunk_json(1700000000123, &[0x41, 0x42]),
            r#"{"ok":true,"t":1700000000123,"len":2,"rx_hex":"4142"}"#
        );
        assert_eq!(
            record_summary_json("COM3", "out.log", 12, 3, 250),
            r#"{"ok":true,"port":"COM3","out":"out.log","bytes":12,"chunks":3,"elapsed_ms":250}"#
        );
    }

    /// record 落盘的是**原样字节**：不解码、不转义、不补换行。
    ///
    /// 用 sim 回环当数据源：另一个线程往回环里塞一段带 0x00 / 0xFF /
    /// CR / LF 的字节，循环应当逐字节原样写进文件。
    #[test]
    fn record_loop_writes_raw_bytes_to_the_file() {
        let cfg = PortCfg {
            port: "sim".to_string(),
            baud: 115_200,
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
        };
        let link = Link::open("sim", cfg).unwrap();
        let tx = link.echo_tx().expect("sim 设备应支持主动输出");
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = tx.send(vec![0x00, 0xFF, b'A', b'\r', b'\n']);
        });

        let path = temp_file("record-raw.bin", b"");
        let mut file = std::fs::File::create(&path).unwrap();
        let deadline = Some(Instant::now() + Duration::from_millis(400));
        let (total, chunks) = record_loop(&link, &mut file, OutFormat::Text, deadline).unwrap();
        feeder.join().unwrap();

        assert_eq!((total, chunks), (5, 1), "应当收到一块共 5 字节");
        let written = std::fs::read(&path).unwrap();
        assert_eq!(
            written,
            vec![0x00, 0xFF, b'A', b'\r', b'\n'],
            "落盘的字节必须与收到的完全一致"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// record 能跑通整条链路：打开设备、写文件、给汇总行。
    ///
    /// 用 sim 回环当设备：它不会主动发数据，但足以验证「打开 → 收 → 落盘 → 汇总」。
    #[test]
    fn record_runs_end_to_end_on_sim() {
        let out = temp_file("record.log", b"");
        let code = run(&args(&[
            "record",
            "--port",
            "sim",
            "--duration",
            "100",
            "--out",
            out.to_str().unwrap(),
            "--json",
        ]));
        assert_eq!(code, EX_OK, "record 应当正常结束");
        // 文件被创建了（此时是空的：sim 不会主动发数据）。
        assert!(out.exists(), "record 应当创建输出文件");
        let _ = std::fs::remove_file(&out);
    }

    // ---------------------------------------------------- listen --format

    /// listen 的 NDJSON 收尾行带 done 字段；超时时 ok 为 false。
    #[test]
    fn listen_ndjson_summary_has_done() {
        let ok = listen_ndjson_summary("sim", None, b"AB", 1, 10);
        assert!(ok.contains(r#""done":true"#), "成功行应带 done：{ok}");
        assert!(ok.contains(r#""ok":true"#), "成功行应为 ok：{ok}");
        assert!(ok.contains(r#""received":2"#), "成功行应报收到的字节数：{ok}");
        // 没给 --save 时不该出现 out 字段：结果与以前一字不差。
        assert!(!ok.contains(r#""out""#), "没落盘时不应有 out 字段：{ok}");

        let to = listen_ndjson_timeout("sim", None, b"", 0, 300);
        assert!(to.contains(r#""done":true"#), "超时行同样带 done：{to}");
        assert!(to.contains(r#""ok":false"#), "超时行应为 ok:false：{to}");
        assert!(to.contains(r#""error":"timeout""#), "超时行应说明是超时：{to}");
    }

    // ------------------------------------------------ 多期望（--expect "a|b"）

    /// 多个期望用竖线分隔：每一段各自走既有的十六进制规则。
    ///
    /// "41 42" 是一段（两个字节），"FF" 是另一段 —— 拆成两个期望，
    /// 而不是四个字节拼成一个大期望。
    #[test]
    fn expect_splits_multiple_targets_on_pipes() {
        let all = parse_expects("41 42|FF").unwrap();
        assert_eq!(all.len(), 2, "应当拆成两个期望：{all:?}");
        assert_eq!(all[0], vec![0x41, 0x42], "第一段应是两个字节");
        assert_eq!(all[1], vec![0xFF], "第二段应是一个字节");

        // 三段、段内带空格与 0x 前缀，规则与单段完全一致。
        let all = parse_expects("41 42|FF|0x00").unwrap();
        assert_eq!(all.len(), 3, "应当拆成三个期望：{all:?}");
        assert_eq!(all[2], vec![0x00], "0x 前缀仍然认");

        // 单个期望时就是一段：老写法一字未变。
        assert_eq!(parse_expects("41 42").unwrap(), vec![vec![0x41, 0x42]]);
    }

    /// **逗号不作段分隔符**：它本来就是 `--hex` 那套写法里分隔字节的符号。
    ///
    /// 断言意图：`--expect "41,42"` 必须还是**一根连续的 2 字节针脚**，
    /// 不能变成两根互相独立的 1 字节针脚 —— 后者会让「逆序且不连续」的载荷
    /// 也算命中，校验被静默放宽。
    #[test]
    fn expect_keeps_the_legacy_comma_meaning() {
        let all = parse_expects("41,42").unwrap();
        assert_eq!(all.len(), 1, "逗号只是字节分隔符，不该拆成两段：{all:?}");
        assert_eq!(all[0], vec![0x41, 0x42], "应是一根连续的 2 字节针脚");

        // 尾随逗号是老写法里就允许的（parse_send_hex 会跳过空标记），
        // 不能因为分段逻辑的引入就变成硬性报错。
        assert_eq!(parse_expects("41,").unwrap(), vec![vec![0x41]]);
        assert_eq!(parse_expects("41,").unwrap(), parse_expects("41").unwrap());
    }

    /// 尾随分隔符不算一段；中间的空段是笔误，要指出来。
    #[test]
    fn expect_ignores_a_trailing_separator_but_reports_empty_segments() {
        // 尾随的 `|`：忽略掉，不当成「第 2 段」。
        let all = parse_expects("41|").unwrap();
        assert_eq!(all.len(), 1, "尾随分隔符不该多出一段：{all:?}");
        assert_eq!(all[0], vec![0x41]);

        // 中间的空段：报错，并且要指出是哪一段。
        let e = parse_expects("41||42").unwrap_err();
        assert!(e.contains("第 2 段"), "错误信息要指出是哪一段：{e}");
        assert!(e.contains("--expect"), "错误信息要指明是哪个参数：{e}");
    }

    /// 一段非法就是整个参数非法：退出码 2。
    ///
    /// 断言意图：「校验目标」不能只校验一半 —— 另一半写错了脚本还以为验过了。
    #[test]
    fn expect_multi_rejects_an_illegal_segment() {
        let e = parse_expects("41 42|ABC").unwrap_err();
        assert!(e.contains("--expect"), "错误信息要指明是哪个参数：{e}");
        assert!(e.contains("ABC"), "错误信息要指出出错的标记：{e}");
        assert!(
            e.contains("十六进制") || e.contains("奇数"),
            "错误信息要说明原因：{e}"
        );
        assert!(e.contains("2"), "错误信息要说明是第几段：{e}");

        // 命令行层面：退出码 2（参数错误），不是 5（期望不符）。
        assert_eq!(
            run(&args(&[
                "send", "--port", "sim", "--text", "hi", "--expect", "41 42|ABC", "--json"
            ])),
            EX_USAGE
        );
    }

    /// 多期望的 `expect_hex` 是各段紧凑十六进制的逗号连接。
    #[test]
    fn expect_hex_text_joins_multiple_targets() {
        assert_eq!(expect_hex_text(&[vec![0x41, 0x42]]), "4142");
        assert_eq!(
            expect_hex_text(&[vec![0x41, 0x42], vec![0xFF], vec![0x00]]),
            "4142,FF,00"
        );
        assert_eq!(expect_missing_json(&[]), "[]");
        assert_eq!(
            expect_missing_json(&[vec![0xFF], vec![0x41, 0x42]]),
            r#"["FF","4142"]"#
        );
    }

    /// 多期望必须**同时**出现：只命中一段不算通过。
    #[test]
    fn multi_expect_requires_every_target() {
        let needles = parse_expects("4142|FF").unwrap();
        let buf = b"\x41\x42\x00";
        assert!(
            !needles.iter().all(|n| contains_bytes(buf, n)),
            "只命中一段时不应算通过"
        );
        let buf = b"\x41\x42\xFF";
        assert!(
            needles.iter().all(|n| contains_bytes(buf, n)),
            "两段都在时应算通过"
        );
    }

    // -------------------------------------------------- listen --save

    /// listen 的落盘路径就是 `--save` 给的那个路径，原样使用。
    #[test]
    fn listen_save_path_is_taken_verbatim() {
        let o = opts(&[("save", "a/b.bin")]);
        assert_eq!(o.get("save").map(String::as_str), Some("a/b.bin"));
        // 给了 --save 才带 out 字段；没给时结果与以前一字不差。
        let with = listen_result_json(true, None, "sim", Some("a/b.bin"), b"AB", 3);
        assert!(with.contains(r#""out":"a/b.bin""#), "给了 --save 应回显路径：{with}");
        let without = listen_result_json(true, None, "sim", None, b"AB", 3);
        assert!(!without.contains(r#""out""#), "没给 --save 不应有 out 字段：{without}");
    }

    /// listen 的落盘参数是 `--save`，不是 `--out`：
    /// 混用两个名字会让脚本以为「listen 也能用 --out」，实际报「未知参数」。
    #[test]
    fn listen_rejects_the_out_flag() {
        assert_eq!(
            run(&args(&[
                "listen", "--port", "sim", "--timeout", "100", "--save", "x.bin", "--out", "y.bin",
                "--json"
            ])),
            EX_USAGE
        );
        // record 的落盘参数是 --out；反过来也不认 --save。
        assert_eq!(
            run(&args(&[
                "record", "--port", "sim", "--duration", "10", "--save", "x.bin", "--json"
            ])),
            EX_USAGE
        );
    }

    /// listen --save 落盘的是**原样字节**：不解码、不转义、不补换行。
    ///
    /// 用 sim 回环当数据源：另一个线程往回环里塞一段带 0x00 / 0xFF /
    /// CR / LF 的字节，listen 应当逐字节原样写进文件。
    #[test]
    fn listen_collect_writes_raw_bytes_to_the_save_file() {
        let cfg = PortCfg {
            port: "sim".to_string(),
            baud: 115_200,
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
        };
        let link = Link::open("sim", cfg).unwrap();
        let tx = link.echo_tx().expect("sim 设备应支持主动输出");
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = tx.send(vec![0x00, 0xFF, b'A', b'\r', b'\n']);
        });

        let path = temp_file("listen-save.bin", b"");
        let mut file = std::fs::File::create(&path).unwrap();
        let deadline = Instant::now() + Duration::from_millis(400);
        let (rx, _chunks) =
            listen_collect(&link, deadline, 64, None, OutFormat::Text, Some(&mut file)).unwrap();
        feeder.join().unwrap();

        assert_eq!(rx.len(), 5, "应当收到 5 字节");
        let written = std::fs::read(&path).unwrap();
        assert_eq!(
            written,
            vec![0x00, 0xFF, b'A', b'\r', b'\n'],
            "落盘的字节必须与收到的完全一致"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// listen --save 的文件打不开属于运行错误（退出码 1）：不能静默丢数据。
    #[test]
    fn listen_save_to_an_unwritable_path_is_a_run_error() {
        // 目录不存在 → 建文件必然失败。
        let code = run(&args(&[
            "listen",
            "--port",
            "sim",
            "--timeout",
            "100",
            "--save",
            "stui-no-such-dir-xyz/out.bin",
            "--json"
        ]));
        assert_eq!(code, EX_RUN, "保存文件打不开应当是运行错误（1）");
    }

    // ------------------------------------------------ record --stdout

    /// `--stdout` 与 `--json` 互相矛盾：stdout 被原始字节占住，
    /// JSON 汇总再写进去就混在一起了 → 用法错误（退出码 2）。
    #[test]
    fn record_rejects_stdout_with_json() {
        assert_eq!(
            run(&args(&[
                "record", "--port", "sim", "--duration", "10", "--stdout", "--json"
            ])),
            EX_USAGE
        );
    }

    /// `--stdout` 与 `--out` 可以同时用：一个落盘，一个冲 stdout。
    #[test]
    fn record_accepts_stdout_together_with_out() {
        let out = temp_file("record-stdout.log", b"");
        let code = run(&args(&[
            "record",
            "--port",
            "sim",
            "--duration",
            "100",
            "--out",
            out.to_str().unwrap(),
            "--stdout",
        ]));
        assert_eq!(code, EX_OK, "record 应当正常结束");
        assert!(out.exists(), "仍然应当创建输出文件");
        let _ = std::fs::remove_file(&out);
    }

    /// record --stdout 写出去的是**原样字节**：不加任何东西。
    ///
    /// 用注入的收集端代替真正的 stdout：验证的是「同一批字节被原样复制一份」，
    /// 而写 stdout 那一步只是把它接到 `io::stdout()` 上。
    #[test]
    fn record_loop_tees_raw_bytes_to_a_second_sink() {
        let cfg = PortCfg {
            port: "sim".to_string(),
            baud: 115_200,
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
        };
        let link = Link::open("sim", cfg).unwrap();
        let tx = link.echo_tx().expect("sim 设备应支持主动输出");
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = tx.send(vec![0x00, 0xFF, b'A', b'\r', b'\n']);
        });

        let path = temp_file("record-tee.bin", b"");
        let mut file = std::fs::File::create(&path).unwrap();
        let mut tee: Vec<u8> = Vec::new();
        let deadline = Some(Instant::now() + Duration::from_millis(400));
        let (total, chunks) = record_loop_with(
            &link,
            &mut file,
            OutFormat::Text,
            ChunkOut::Stderr,
            &mut |bytes: &[u8]| {
                tee.extend_from_slice(bytes);
                Ok(())
            },
            deadline,
        )
        .unwrap();
        feeder.join().unwrap();

        assert_eq!((total, chunks), (5, 1), "应当收到一块共 5 字节");
        let written = std::fs::read(&path).unwrap();
        assert_eq!(written, tee, "落盘与冲 stdout 的是同一批字节");
        assert_eq!(
            tee,
            vec![0x00, 0xFF, b'A', b'\r', b'\n'],
            "冲出去的字节必须是原样的"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 新参数要被解析器认出来：listen 的 --save 与 record 的 --stdout。
    #[test]
    fn parse_args_recognizes_save_and_stdout() {
        let o = parse_args(&args(&["--port", "sim", "--save", "x.bin", "--json"]), SPEC_LISTEN).unwrap();
        assert_eq!(o.get("save").map(String::as_str), Some("x.bin"));

        let o = parse_args(&args(&["--port", "sim", "--stdout", "--json"]), SPEC_RECORD).unwrap();
        assert!(o.contains_key("stdout"), "record 应当认 --stdout");
    }
}
