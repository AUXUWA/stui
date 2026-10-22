//! stui —— 终端串口工具。
//!
//! 用法：`stui [--port COM3] [--baud 115200] [--open] [--no-config]`

mod app;
mod cli;
mod config;
mod fmt;
mod port;
mod ui;

use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, Dir};
use config::Config;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// 把 Windows 控制台切到 UTF-8 代码页（65001）。
///
/// Rust 的输出本身始终是 UTF-8 字节，但在旧版 conhost（936/437）下会显示成乱码。
/// 只改输出代码页与 VT 模式，不动输入代码页，避免影响命令行里输入中文参数；
/// stdout 被重定向到管道/文件时（正是 AI / 脚本调用场景）直接跳过。
#[cfg(windows)]
fn ensure_utf8_console() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn GetStdHandle(which: i32) -> *mut c_void;
    }

    const STD_OUTPUT_HANDLE: i32 = -11;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() {
            return;
        }
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return; // stdout 不是控制台（重定向到文件/管道）
        }
        SetConsoleOutputCP(65001);
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

#[cfg(not(windows))]
fn ensure_utf8_console() {}

/// 终端守卫：任何提前退出（含 panic）都会恢复原终端状态。
///
/// 恢复的东西有三样：raw mode、备用屏、鼠标捕获。
/// 三样都放在这一个守卫里，是为了让「进终端」和「还原终端」永远成对出现 ——
/// 少还原一样，用户就会拿到一个不能用的终端（光标不见了、鼠标还开着、
/// 键盘还在乱码状态），而这种残留比程序崩溃更难收拾。
struct TuiGuard {
    /// 终端句柄；`None` 表示终端根本没建起来（进入失败）。
    term: Option<Tui>,
    /// 鼠标捕获是否成功打开。
    ///
    /// 只有打开过才需要在退出时关掉：对着一个没开过捕获的终端发关闭序列
    /// 没有意义，还可能把终端状态弄得更怪。
    mouse: bool,
}

impl TuiGuard {
    fn enter() -> io::Result<TuiGuard> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        // 鼠标捕获失败**不能**挡住启动：老 conhost、某些 SSH / 网页终端
        // 根本不支持鼠标上报，这时就当没有鼠标 —— 程序照常跑，
        // 只是滚轮不可用。比「起不来」好得多。
        let mouse = execute!(stdout, EnableMouseCapture).is_ok();
        let mut t = Terminal::new(CrosstermBackend::new(stdout))?;
        t.clear()?;
        Ok(TuiGuard { term: Some(t), mouse })
    }

    fn term(&mut self) -> &mut Tui {
        self.term.as_mut().expect("终端尚未初始化")
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        if let Some(t) = self.term.as_mut() {
            let _ = disable_raw_mode();
            // 鼠标捕获与 raw mode / 备用屏在同一个 drop 里还原：
            // 异常退出（panic）走的是同一条路，终端一样能回到原样。
            // 顺序上先关鼠标：鼠标事件要是在备用屏里还开着，
            // 终端会把后续的移动事件塞给已经退出的程序或下一个程序。
            if self.mouse {
                let _ = execute!(t.backend_mut(), DisableMouseCapture);
            }
            let _ = execute!(t.backend_mut(), LeaveAlternateScreen);
            let _ = t.show_cursor();
        }
    }
}

/// 命令行解析结果。
#[derive(Debug, PartialEq)]
struct Args {
    port: Option<String>,
    baud: Option<u32>,
    auto_open: bool,
    fps: Option<u32>,
    /// `--no-config`：跳过配置文件的读取与写入，用默认设置跑一次。
    no_config: bool,
}

/// `--help` 的文案。
///
/// 抽成常量有两个好处：排版里那些 `\x20` 缩进写在函数体里几乎没法读；
/// 另外测试可以直接断言文案里确实列出了新的子命令与参数。
///
/// 原有条目一个都不删，新增的子命令与参数要么另起一行追加，
/// 要么接在原行末尾 —— 这样老文档里的每一行仍然逐字对得上
/// （原来的行仍是新行的前缀）。
const HELP: &str = "\
stui · 终端串口工具\n\
\n\
通用：\n\
\x20 -V, --version      打印版本号\n\
\x20 -h, --help        打印这份说明\n\
\n\
交互界面：\n\
\x20 stui [-p <端口>] [-b <波特率>] [-o] [--fps <帧率>] [--no-config]\n\
\x20 -p, --port <名>   预选端口（如 COM3、/dev/ttyUSB0）\n\
\x20 -b, --baud <数值>  波特率，默认 115200\n\
\x20 -o, --open        启动后立即打开串口\n\
\x20     --fps <数值>  界面重绘上限（默认 20），远程/容器终端卡顿时调小\n\
\x20     --no-config  不读也不写配置文件，只用默认设置\n\
\n\
非交互（脚本 / AI 调用，加 --json 输出 JSON）：\n\
\x20 stui list   [--json] [--kind <类型>] [--verbose]\n\
\x20 stui ports  [--json] [--kind <类型>] [--verbose]\n\
\x20 stui send   --port <名> [--baud n] [--text <串>] [--hex <十六进制>]\n\
\x20                   [--file <路径>] [--repeat n] [--interval ms]\n\
\x20                   [--wait ms] [--json] [--dry-run]\n\
\x20                   [--expect <十六进制>] [--expect-timeout ms]\n\
\x20                   [--bytes n] [--bits 5-8] [--parity none|odd|even]\n\
\x20                   [--stops 1|2] [--flow none|hardware|software]\n\
\x20 stui listen --port <名> [--baud n] [--timeout ms] [--bytes n] [--json]\n\
\x20                   [--format ndjson|text] [--idle ms] [--save <路径>]\n\
\x20 stui watch  --port <名> [--baud n] [--timeout ms] [--interval ms] [--json]\n\
\x20 stui record --port <名> [--baud n] [--out <路径>] [--duration ms]\n\
\x20                   [--format ndjson|text] [--stdout] [--json]\n\
\x20 stui selftest [--json]\n\
\x20 stui sim    [--pattern <十六进制>] [--count n] [--duration ms] [--interval ms]\n\
\x20                   [--json]\n\
\x20（--port sim 使用内置回环虚拟设备，无需真实硬件）\n\
\x20（--dry-run 只解析不发送、不打开串口；--kind 取 usb / bluetooth / pci）\n\
\x20（--verbose 在端口列表后多显示厂商与产品两列）\n\
\x20（--expect 校验设备回包，--expect-timeout 指定等回包的时限）\n\
\x20（--expect 可给多段，用竖线分隔，如 --expect \"41 42|FF\"，要同时出现才算通过）\n\
\x20（--format ndjson 逐块流式输出，text 是给人看的整块输出）\n\
\x20（record 把收到的数据原样落盘到 --out 指定的文件）\n\
\x20（--save 把 listen 收到的字节原样落盘到指定文件）\n\
\x20（--stdout 让 record 的原始字节也原样冲一份到 stdout）\n\
\x20（--idle 多少毫秒没数据就收工；--count 是 sim 重复发 pattern 的次数）\n\
\x20（--bits/--parity/--stops/--flow 指定数据位、校验位、停止位与流控）\n\
\n\
退出码：\n\
\x200 成功；1 运行错误；2 参数错误；3 串口打开失败；4 接收超时；\n\
\x205 响应与期望不符（send --expect 没能等到期望的字节序列）\n\
\n\
界面内按 F1 查看完整按键说明。\n\
\x20（界面内支持鼠标：滚轮上下滚动数据区，滚到底就停住）";

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args { port: None, baud: None, auto_open: false, fps: None, no_config: false };
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" | "-p" => {
                args.port = Some(it.next().cloned().ok_or("--port 需要端口名，例如 COM3 或 /dev/ttyUSB0")?);
            }
            "--baud" | "-b" => {
                let v = it.next().ok_or("--baud 需要数值，例如 115200")?;
                args.baud = Some(v.parse().map_err(|_| format!("无效波特率：{v}"))?);
            }
            "--open" | "-o" => args.auto_open = true,
            "--fps" => {
                let v = it.next().ok_or("--fps 需要数值，例如 10")?;
                let n: u32 = v.parse().map_err(|_| format!("无效帧率：{v}"))?;
                if !(1..=120).contains(&n) {
                    return Err(format!("帧率需在 1~120 之间：{v}"));
                }
                args.fps = Some(n);
            }
            "--no-config" => args.no_config = true,
            // 版本号从 Cargo.toml 来（CARGO_PKG_VERSION），不用手写常量 ——
            // 手写的版本号迟早和发布出去的二进制对不上。
            "-V" | "--version" => {
                println!("stui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(args)
}

/// CLI 子命令：命中即走非交互模式，便于脚本 / AI 调用。
///
/// 这份清单是「哪些词算子命令」的唯一判据：不在表里的第一个参数会被
/// 当成 TUI 参数报错。cli::run 里真正能跑的分派以那里的为准，
/// 两边必须同步 —— 多一个 `record` 少一个，都会让脚本撞上莫名其妙的错误。
const SUBCOMMANDS: [&str; 8] =
    ["list", "ports", "send", "listen", "watch", "record", "selftest", "sim"];

// ------------------------------------------------------------------
// 配置持久化
// ------------------------------------------------------------------

/// 配置文件里出现过的顶层键名。
///
/// `Config` 里的开关是裸 `bool`，**分不出「用户没写过」与「用户写了 false」**。
/// 但这两者对界面的含义恰好相反：
/// - `auto_ports` 在界面上的默认值是 true，文件里没有这个键（老版本写的
///   配置、手写的配置）时应当继续 true，不能因为「没写」就把它关掉；
/// - 用户在界面上关掉它之后要能存回来，并在下次启动时保持关闭 ——
///   所以「文件里确实写了 false」必须被当成「用户明确关掉了」。
///
/// 因此这里另记一份键名清单，把这两种情况分开。值的解析仍以 `Config` 为准，
/// 这里只回答「这个键在不在文件里」。
#[derive(Debug, Default, PartialEq)]
struct WrittenKeys {
    keys: Vec<String>,
    /// 顶层布尔键的取值：`("filter_ci", true)` 这样的二元组。
    ///
    /// 有些开关压根不在 `Config` 结构里（配置文件归 config 模块管，
    /// 那边没加字段这边就没法从 `Config` 读到值），
    /// 想知道「用户写的是 true 还是 false」只能回到原文里问。
    bools: Vec<(String, bool)>,
}

impl WrittenKeys {
    /// 文件里是否出现过这个键。
    fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }

    /// 这个键在文件里是个布尔开关吗？是的话它的值是多少。
    ///
    /// 返回 `None` 表示「文件里没这个键」—— 调用方据此保留界面自己的默认值。
    /// 这正是「没写」与「写了 false」的分界：前者是 `None`，
    /// 后者是 `Some(false)`。
    fn bool_of(&self, key: &str) -> Option<bool> {
        self.bools.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    /// 从配置文件原文里收集出现过的顶层键名。
    ///
    /// 读不出来（文件不存在、被删、无权限）就返回空集合：
    /// 拿不准时一律当成「没写」，保留界面自己的默认值总比误关一个开关好。
    fn read_from(path: &std::path::Path) -> WrittenKeys {
        // 配置文件正常只有几百字节；这里的上限只是防止误指到一个巨大的文件
        // 时把它整个读进内存（config 模块自己也有一道 64 KiB 的上限）。
        const MAX_SCAN_LEN: u64 = 1024 * 1024;
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > MAX_SCAN_LEN => return WrittenKeys::default(),
            _ => {}
        }
        match std::fs::read_to_string(path) {
            Ok(text) => WrittenKeys {
                keys: top_level_keys(&text),
                bools: top_level_bools(&text),
            },
            Err(_) => WrittenKeys::default(),
        }
    }
}

/// 扫出一段 JSON 文本里出现过的**顶层**键名。
///
/// 只做「键名在不在」这一件事，值不关心 —— 值由 `Config` 解析。
/// 自己扫一遍而不是给 `Config` 加字段，是因为配置文件归 config 模块管，
/// 这里的约定（开关分不出「没写」）不该逼着那个模块改结构。
///
/// 顶层之外的键不算数：嵌套对象里的同名键与顶层开关无关。
fn top_level_keys(text: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut depth = 0usize;
    // 下一个字符串是不是键名：对象开始与逗号之后是，冒号之后不是（那是值）。
    let mut expect_key = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                // 读出整个字符串字面量（含转义）。
                let mut s = String::new();
                let mut escaped = false;
                for c2 in chars.by_ref() {
                    if escaped {
                        s.push(c2);
                        escaped = false;
                        continue;
                    }
                    match c2 {
                        '\\' => escaped = true,
                        '"' => break,
                        _ => s.push(c2),
                    }
                }
                if expect_key && depth == 1 {
                    keys.push(s);
                    expect_key = false;
                }
            }
            '{' => {
                depth += 1;
                expect_key = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                expect_key = false;
            }
            ',' => {
                if depth == 1 {
                    expect_key = true;
                }
            }
            ':' => expect_key = false,
            _ => {}
        }
    }
    keys
}

/// 扫出一段 JSON 文本里出现过的**顶层布尔值**。
///
/// 只认 `true` / `false` 两个字面量：别的类型的值（数字、字符串、数组、
/// 对象）一律不收 —— 收进来也没有开关用得上。
///
/// 与 [`top_level_keys`] 是同一个道理：配置文件归 config 模块管，
/// 那边的 `Config` 结构里没有过滤开关这两个字段，
/// 「用户写的是 true 还是 false」只能自己回原文里问。
fn top_level_bools(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut depth = 0usize;
    // 下一个字符串是不是键名：对象开始与逗号之后是，冒号之后不是（那是值）。
    let mut expect_key = false;
    // 刚读到的键名，等着它的值：只有顶层键的值才是开关。
    let mut pending_key: Option<String> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                // 读出整个字符串字面量（含转义）。
                let mut s = String::new();
                let mut escaped = false;
                for c2 in chars.by_ref() {
                    if escaped {
                        s.push(c2);
                        escaped = false;
                        continue;
                    }
                    match c2 {
                        '\\' => escaped = true,
                        '"' => break,
                        _ => s.push(c2),
                    }
                }
                if expect_key && depth == 1 {
                    pending_key = Some(s);
                    expect_key = false;
                }
            }
            '{' => {
                depth += 1;
                expect_key = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                expect_key = false;
                pending_key = None;
            }
            ',' => {
                if depth == 1 {
                    expect_key = true;
                }
            }
            ':' => {
                expect_key = false;
                if depth == 1 {
                    if let Some(key) = pending_key.take() {
                        // 值就在冒号后面：跳过空白再看是不是 true / false。
                        // 这里只**窥探**不消费，真正的消费还在主循环里，
                        // 免得同一个字符被读两次。
                        let mut look = chars.clone();
                        while let Some(&c) = look.peek() {
                            if c.is_whitespace() {
                                look.next();
                            } else {
                                break;
                            }
                        }
                        let mut word = String::new();
                        while let Some(&c) = look.peek() {
                            if c.is_ascii_alphabetic() {
                                word.push(c);
                                look.next();
                            } else {
                                break;
                            }
                        }
                        match word.as_str() {
                            "true" => out.push((key, true)),
                            "false" => out.push((key, false)),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 把持久化的配置套到 App 上。
///
/// 原则与 `config` 模块一致：**坏配置不能让程序起不来**。
/// 每一项取不到就保留 App 的默认值，端口对不上就记一条 SYS 日志，
/// 剩下的设置照常恢复。
///
/// `cfg` 里的端口名按不区分大小写匹配枚举到的端口：Windows 上用户可能
/// 写的是 `com3`，而系统枚举出来的是 `COM3`。
///
/// `from_file` 表示这份配置是真的从磁盘上读来的。
/// 首次启动时磁盘上没有配置文件，`Config::load()` 返回的是一份全默认值，
/// 里面 `show_ms = false` 而界面的默认值是 `true`（毫秒时间戳）；
/// 无条件赋值会让"第一次启动"的时间戳精度从毫秒变成秒 —— 那是行为倒退，
/// 不是恢复配置。所以两个开关只在真有配置文件时才覆盖。
/// 配置文件存在但内容坏掉时同样不算「有配置」：它退回的是默认值，
/// 不是用户的偏好。
///
/// `written` 是文件里真正出现过的键名，用来区分开关的「没写」与「写了 false」
/// （理由见 [`WrittenKeys`]）。
fn apply_config(app: &mut App, cfg: &Config, from_file: bool, written: &WrittenKeys) {
    // 端口名：匹配到就选中它。
    if let Some(name) = &cfg.port {
        match app.ports.iter().position(|e| e.port.eq_ignore_ascii_case(name)) {
            Some(i) => app.port_idx = i,
            None => app.push_sys(format!("配置里的端口 {name} 当前不存在，保留默认端口")),
        }
    }
    // 波特率：必须在快捷列表里，否则保留默认值。
    if let Some(b) = cfg.baud {
        match port::BAUDS.iter().position(|&x| x == b) {
            Some(i) => app.baud_idx = i,
            None => app.push_sys(format!("配置里的波特率 {b} 不在快捷列表中，保留默认波特率")),
        }
    }
    // 数据位 / 停止位：配置里存的是数字，编码表在 port.rs 里，
    // 这里用同一套 from_code 解析，避免 main.rs 另写一份字符串对照表。
    if let Some(bits) = cfg.bits {
        match port::bits_from_code(&bits.to_string()) {
            Some(v) => app.bits = v,
            None => app.push_sys(format!("配置里的数据位 {bits} 无法识别，保留默认数据位")),
        }
    }
    if let Some(parity) = &cfg.parity {
        match port::parity_from_code(parity) {
            Some(v) => app.parity = v,
            None => app.push_sys(format!("配置里的校验位 {parity} 无法识别，保留默认校验位")),
        }
    }
    if let Some(stops) = cfg.stops {
        match port::stops_from_code(&stops.to_string()) {
            Some(v) => app.stops = v,
            None => app.push_sys(format!("配置里的停止位 {stops} 无法识别，保留默认停止位")),
        }
    }
    if let Some(flow) = &cfg.flow {
        match port::flow_from_code(flow) {
            Some(v) => app.flow = v,
            None => app.push_sys(format!("配置里的流控 {flow} 无法识别，保留默认流控")),
        }
    }
    if let Some(view) = &cfg.view {
        match port::view_from_code(view) {
            Some(v) => app.view = v,
            None => app.push_sys(format!("配置里的视图 {view} 无法识别，保留默认视图")),
        }
    }
    if let Some(v) = cfg.interval_ms {
        // 间隔越界会让循环发送变成「每帧都发一次」：界面里的调整也夹在
        // 50~60000 之间，配置同理，越界就保留默认值。
        if (50..=60_000).contains(&v) {
            app.interval_ms = v;
        } else {
            app.push_sys(format!("配置里的循环间隔 {v} 不在 50~60000 之间，保留默认间隔"));
        }
    }
    if let Some(v) = cfg.fps {
        // 帧率越界会让主循环算出错误的等待间隔，这里直接退回默认值。
        if (1..=120).contains(&v) {
            app.fps = v;
        } else {
            app.push_sys(format!("配置里的帧率 {v} 不在 1~120 之间，保留默认帧率"));
        }
    }
    // 两个开关直接赋值（没有配置文件时除外，理由见 apply_config 的说明）。
    if from_file {
        app.auto_reconnect = cfg.auto_reconnect;
        app.show_ms = cfg.show_ms;
        // wrap：界面上的默认值就是 false，文件里没写与写了 false 等价，
        // 所以直接赋值不会误伤默认值。
        app.wrap = cfg.wrap;
        // auto_ports：界面上的默认值是 true，与 wrap 正好相反 ——
        // 文件里没这个键（老版本或手写的配置）时必须继续 true，
        // 只有文件里确实写了 auto_ports 才照文件里的值来：
        // 写了 false 就是用户明确把它关掉了，那得关。
        if written.contains("auto_ports") {
            app.auto_ports = cfg.auto_ports;
        }
        // 过滤的两个开关（忽略大小写 / 排除匹配）。
        //
        // 这两个字段不在 `Config` 结构里（配置文件归 config 模块管），
        // 所以值是从配置原文里取的：`written.bool_of` 返回 `None`
        // 表示「文件里没写过」，这时保留界面自己的默认值；
        // 返回 `Some(false)` 表示「用户明确写了 false」，那得照办。
        // 分不清这两者的话，用户在界面上关掉的开关每次启动都会被打开。
        if let Some(v) = written.bool_of("filter_ci") {
            app.filter_ci = v;
        }
        if let Some(v) = written.bool_of("filter_invert") {
            app.filter_invert = v;
        }
    }
}

/// 按当前界面上的设置拼一份配置。
///
/// 端口名取当前选中的端口；波特率取 `BAUDS` 里的那一档。
/// 编码统一走 port.rs 的 code 函数：写进去的字符串与读出来时
/// from_code 认得的字符串必须是同一套。
fn config_from_app(app: &App) -> Config {
    Config {
        // 没选中端口时留空，下次启动仍按默认走。
        port: app.ports.get(app.port_idx).map(|e| e.port.clone()),
        baud: Some(port::BAUDS[app.baud_idx]),
        bits: port::bits_code(app.bits).parse::<u8>().ok(),
        parity: Some(port::parity_code(app.parity).to_string()),
        stops: port::stops_code(app.stops).parse::<u8>().ok(),
        flow: Some(port::flow_code(app.flow).to_string()),
        view: Some(port::view_code(app.view).to_string()),
        interval_ms: Some(app.interval_ms),
        fps: Some(app.fps),
        auto_reconnect: app.auto_reconnect,
        show_ms: app.show_ms,
        // 两个新开关照当前界面上的取值存回去：写 false 才能把用户的
        // 选择真正保存下来（下次启动时靠 written 里的键名区分
        // 「明确关了」与「没写过」）。
        wrap: app.wrap,
        auto_ports: app.auto_ports,
    }
}

/// 退出前把当前设置写回配置文件。
///
/// 写失败只记一条 SYS 日志（并把同一句话返回给调用方，由它打到 stderr）：
/// 配置存不上不该影响退出码，也不该让用户看到一屏堆栈。
fn save_config(app: &mut App) -> Option<String> {
    match config_from_app(app).save() {
        Ok(()) => {
            // 过滤的两个开关不在 `Config` 结构里，得自己补写 ——
            // 不写回去就谈不上「持久化」，下次启动又要重新设一遍。
            if let Err(e) = write_filter_switches(&Config::path(), app.filter_ci, app.filter_invert) {
                let msg = format!("保存过滤开关失败：{e}");
                app.push_sys(msg.clone());
                return Some(msg);
            }
            None
        }
        Err(e) => {
            let msg = format!("保存配置失败：{e}");
            app.push_sys(msg.clone());
            Some(msg)
        }
    }
}

/// 把两个过滤开关补写进配置文件。
///
/// `Config` 结构里没有这两个字段（配置文件归 config 模块管，改结构不是
/// 这边能做的），于是走「顶层键追加」的路子：在配置写好之后，
/// 把 `filter_ci` / `filter_invert` 插进 JSON 对象的末尾。
/// config 模块读配置时只认自己认识的键，多出来的键会被忽略，
/// 因此这两行对它是透明的。
///
/// 两点讲究：
/// 1. **文件里已经有这个键就不动它** —— 将来 config 模块自己管起这两个字段时，
///    由它写出来的值才是权威的，这里再插一遍就成了重复键；
/// 2. **先写临时文件再改名** —— 与 config 模块的写法一致，
///    写到一半断电不会把用户的配置变成半截 JSON。
fn write_filter_switches(
    path: &std::path::Path,
    filter_ci: bool,
    filter_invert: bool,
) -> io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let trimmed = text.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "配置文件不是 JSON 对象"));
    }
    let mut entries: Vec<String> = Vec::new();
    for (key, value) in [("filter_ci", filter_ci), ("filter_invert", filter_invert)] {
        // 已经有人写过这个键了：不重复插。
        if text.contains(&format!("\"{key}\"")) {
            continue;
        }
        entries.push(format!("\"{key}\":{value}"));
    }
    if entries.is_empty() {
        return Ok(());
    }
    // 去掉对象最后的 '}'，把新键插进去，再补回来。
    let body = trimmed.strip_suffix('}').unwrap_or(trimmed);
    // 空对象 `{}` 后面不能直接跟逗号。
    let sep = if body.trim_end().ends_with('{') { "" } else { "," };
    let new_text = format!("{body}{sep}{}}}", entries.join(","));

    // 原子替换：先写同目录下的临时文件再改名。
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.json".to_string());
    // 临时文件的名字与 config 模块的错开：万一两边同时落盘，
    // 也不会互相把对方的临时文件顶掉。
    let tmp = dir.join(format!("{name}.{}.filter.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(new_text.as_bytes())?;
        f.flush()?;
        let _ = f.sync_all();
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 改名失败时不要留下半截临时文件。
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 以统一格式报错并退出：带 `--json` 时输出单行 JSON，否则写 stderr。
fn fail_json(msg: &str, code: i32) -> ! {
    // JSON 字符串转义：复用 `fmt::escape_json`（小于 0x20 的控制字符写成
    // \u00XX），别在这里另写一份 —— 早先只处理 \\ " \n \r \t，
    // 其余控制字符（如 0x0C）会原样落进字符串，输出就不再是合法 JSON。
    let escaped = fmt::escape_json(msg);
    println!("{{\"ok\":false,\"error\":\"{escaped}\"}}");
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn fail(msg: &str, json: bool, code: i32) -> io::Result<()> {
    if json {
        fail_json(msg, code);
    }
    eprintln!("参数错误：{msg}\n（stui --help 查看用法）");
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn main() -> io::Result<()> {
    ensure_utf8_console();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let wants_json = argv.iter().any(|a| a == "--json");

    if let Some(first) = argv.first() {
        if SUBCOMMANDS.contains(&first.as_str()) {
            std::process::exit(cli::run(&argv));
        }
        // 看起来是想用子命令却拼错了：走 CLI 的错误通道，保持输出格式一致
        if !first.starts_with('-') {
            return fail(first, wants_json, 2);
        }
    }

    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            if wants_json {
                fail_json(&e, 2);
            } else {
                eprintln!("参数错误：{e}\n（stui --help 查看用法）");
                std::process::exit(2);
            }
        }
    };

    let mut app = App::new();
    app.refresh_ports();

    // 先恢复上次用过的设置，再让命令行参数覆盖它：
    // 命令行是"这一次"的明确意图，优先级高于配置文件里留下的偏好。
    //
    // `--no-config` 时完全不碰配置文件：既不读也不写，
    // 用户明确说了"这次别动我的设置"。
    if !args.no_config {
        // `Config::load()` 会报告这份配置是不是真的从磁盘上读来的：
        // 文件存在但内容坏掉时，它退回全默认值，此时不能当作「有配置」，
        // 否则两个开关会被默认值覆盖（时间戳精度从毫秒降到秒的倒退）。
        let (cfg, from_file) = Config::load();
        // 键名清单要单独看一次原文：`Config` 里的开关是裸 bool，
        // 分不出「没写」与「写了 false」，而这两者对 auto_ports 的
        // 含义正好相反（详见 WrittenKeys）。
        let written = WrittenKeys::read_from(&Config::path());
        apply_config(&mut app, &cfg, from_file, &written);
    }

    if let Some(b) = args.baud {
        if let Some(i) = port::BAUDS.iter().position(|&x| x == b) {
            app.baud_idx = i;
        } else {
            // 报实际保留下来的那一档：没有配置文件时就是默认的 115200，
            // 有配置文件时是配置里那档（命令行覆盖不了它，就得以它为准）。
            app.push_sys(format!(
                "波特率 {b} 不在快捷列表中，保留 {}",
                port::BAUDS[app.baud_idx]
            ));
        }
    }
    if let Some(p) = &args.port {
        match app.ports.iter().position(|e| e.port.eq_ignore_ascii_case(p)) {
            Some(i) => app.port_idx = i,
            None => {
                app.push_sys(format!("未找到端口 {p}，请在界面中按 r 刷新"));
            }
        }
    }
    app.push_sys("stui 已启动 · F1 帮助 · q 退出");
    if let Some(n) = args.fps {
        app.fps = n;
        app.push_sys(format!("界面重绘上限设为 {n} 帧/秒"));
    }
    if args.auto_open {
        app.open_link();
    }

    let mut guard = TuiGuard::enter()?;
    if !guard.mouse {
        // 记一条而不是只留在脑子里：用户滚不动的时候，
        // 第一反应是「程序坏了」，而不是「你的终端不支持鼠标」。
        app.push_sys("终端不支持鼠标捕获，滚轮滚动不可用");
    }
    let res = run(guard.term(), &mut app);
    // 退出流程的顺序是有讲究的：
    // 1. 先保存配置 —— 必须在 close_link 之前，存下来的才是用户最后看到的
    //    串口参数，而不是被自动重连改过的中间状态；
    // 2. 再断开串口；
    // 3. 最后恢复终端 —— 保存失败的提示这时才打在普通屏幕上，
    //    而不是被留在备用屏里跟着一起消失。
    let save_err = if args.no_config { None } else { save_config(&mut app) };
    app.close_link("退出程序");
    drop(guard);
    // 保存失败也要让用户看见：同一句话，既进 SYS 日志，也打到 stderr。
    if let Some(msg) = save_err {
        eprintln!("{msg}");
    }
    res
}

/// 滚轮一格滚动多少行。
///
/// 三行是个中间值：滚一下就能看清上下文，又不至于一格滚过头。
/// 触控板一格常常连发好几个滚轮事件，行数太小的话会滚得飞快。
const MOUSE_SCROLL_LINES: usize = 3;

/// 非终端页（日志 / 统计 / 帮助）的滚动上限兜底值。
///
/// 统计页与帮助页是拼出来的静态文本，能有多长只有渲染层知道；
/// 这里给一个宽松的上限，防止数值失控。真正的夹紧由渲染层每帧完成
/// （ui.rs 绘制时会把 log_scroll 夹到实际可滚的最大值）。
const LOG_SCROLL_LIMIT_FALLBACK: usize = 4096;

/// 处理一个鼠标事件，返回界面状态有没有变。
///
/// 只认滚轮：点击、移动、拖拽一律忽略 —— 界面上没有鼠标交互，
/// 自己给这些事件编一套行为只会让用户误操作（误触一下就跳焦点、
/// 拖个框就清数据，是最难排查的一类问题）。
///
/// 滚动的语义与方向键完全一致：
/// - 终端页滚数据区（`scroll_by` 内部会关掉自动跟随 —— 手动滚动就该停住，
///   别让新数据把用户从正在看的位置拽走）；
/// - 日志 / 统计 / 帮助页滚 `log_scroll`，上下界自己夹住。
fn on_mouse(app: &mut App, m: MouseEvent) -> bool {
    let dir: isize = match m.kind {
        MouseEventKind::ScrollUp => -(MOUSE_SCROLL_LINES as isize),
        MouseEventKind::ScrollDown => MOUSE_SCROLL_LINES as isize,
        _ => return false,
    };
    // 弹窗打开时鼠标归弹窗：这时去滚底下的数据区，用户什么也看不见，
    // 还会顺手把自动跟随关掉（scroll_by 的副作用），得不偿失。
    if app.modal.is_some() {
        return false;
    }
    let before = (app.scroll_top, app.auto_scroll, app.log_scroll);
    if app.tab == 0 {
        // 终端页：数据区。
        app.scroll_by(dir);
    } else {
        // 日志 / 统计 / 帮助页：共用同一个滚动位置。
        if dir < 0 {
            app.log_scroll = app.log_scroll.saturating_sub(dir.unsigned_abs());
        } else {
            app.log_scroll = (app.log_scroll + dir as usize).min(log_scroll_limit(app));
        }
    }
    (app.scroll_top, app.auto_scroll, app.log_scroll) != before
}

/// 非终端页能滚到哪儿。
///
/// 日志页能滚的距离就是 SYS 记录的条数，这里数得出来。
/// 统计页与帮助页的行数只有渲染层知道，用一个宽松的上限兜底。
fn log_scroll_limit(app: &App) -> usize {
    match app.tab {
        1 => app.records.iter().filter(|r| r.dir == Dir::Sys).count(),
        _ => LOG_SCROLL_LIMIT_FALLBACK,
    }
}

/// 事件主循环。
///
/// 两道限速，弱链路终端（SSH / 容器 / 网页终端）上的卡顿主要就出在这里：
/// 1. **脏标记**：只有界面可见状态变了才重绘，否则一帧都不画；
/// 2. **帧率上限**（`app.fps`）：即使一直在收数据，也最多画这么多帧。
/// 收数、封行、循环发送都在 draw 之外推进，降低帧率不会漏数据。
///
/// 鼠标事件走的是同一条路：处理完之后照样由上面两道限速决定要不要重绘，
/// `event::poll` 的等待时间与帧率上限的关系一个字都没变 ——
/// 开了鼠标捕获只是多了一种事件来源，不是多了一轮等待。
fn run(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    let mut last_tick = Instant::now();
    let mut last_draw = Instant::now();
    let mut sig = app.ui_signature();
    let mut forced = true; // 首帧必画
    let mut instant = false; // 按键引起的重绘不受帧率上限约束
    loop {
        // 两档之间的时间用来等事件，不空转
        let gap = Duration::from_millis(1000 / app.fps.clamp(1, 120) as u64);
        let wait = gap.saturating_sub(last_draw.elapsed()).min(Duration::from_millis(40));
        if event::poll(wait.max(Duration::from_millis(4)))? {
            match event::read()? {
                Event::Key(k) => {
                    // 过滤自动重复：所有键位都用按下沿，避免长按导致连发。
                    if k.kind == KeyEventKind::Press {
                        app.on_key(k);
                        forced = true;
                        // 按键要立刻有回显：帧率调到 5 也不能让按键等 200 ms。
                        // 键位已过滤自动重复，一秒几十次按键封顶，不会把限速绕穿。
                        instant = true;
                    }
                }
                // 窗口尺寸变化：终端会把旧行按新宽度重新折行，
                // 残影会和重绘叠在一起，所以这里强制清屏做一次完整重绘。
                Event::Resize(_, _) => {
                    terminal.autoresize()?;
                    terminal.clear()?;
                    forced = true;
                    instant = true;
                }
                // 鼠标：只认滚轮。点击、拖拽、移动一律忽略 ——
                // 界面里没有「点哪儿跳哪儿」的交互，自己给鼠标编一套行为
                // 只会让用户误操作。
                Event::Mouse(m) => {
                    // 滚轮要立刻有回显：和按键一样不等下一帧。
                    // 但只有真的滚动了才重绘 —— 鼠标移动事件非常密集，
                    // 每个都触发一次重绘等于白烧 CPU。
                    if on_mouse(app, m) {
                        forced = true;
                        instant = true;
                    }
                }
                _ => {}
            }
        }

        // 按下 q 之后立刻退出：否则还会再走一次 drain/tick，
        // 循环发送开着时可能又往串口写了一轮数据。
        if app.should_quit {
            return Ok(());
        }

        app.drain_serial();

        if last_tick.elapsed() >= Duration::from_millis(100) {
            last_tick = Instant::now();
            app.tick();
        }

        let now_sig = app.ui_signature();
        if now_sig != sig {
            sig = now_sig;
            forced = true;
        }
        // 指纹没变且不到下一帧时间就跳过：这一次循环不产生任何终端输出
        if forced && (instant || last_draw.elapsed() >= gap) {
            terminal.draw(|f| ui::draw(f, app))?;
            last_draw = Instant::now();
            forced = false;
            instant = false;
        }
    }
}

// ------------------------------------------------------------------
// 测试
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 把字面量数组转成 `parse_args` 要的 `&[String]`。
    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 造一个「文件里写过这些键」的清单。
    fn written(keys: &[&str]) -> WrittenKeys {
        WrittenKeys { keys: keys.iter().map(|k| k.to_string()).collect(), ..Default::default() }
    }

    /// 造一个「文件里写过这些开关，且写了这些值」的清单。
    fn written_with_bools(pairs: &[(&str, bool)]) -> WrittenKeys {
        WrittenKeys {
            keys: pairs.iter().map(|(k, _)| k.to_string()).collect(),
            bools: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    /// 造一个鼠标事件。
    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent { kind, column: 0, row: 0, modifiers: crossterm::event::KeyModifiers::NONE }
    }

    /// 既有参数必须原样解析出来：一个都不能少。
    ///
    /// 这里刻意不测 `-h` / `--help`：那两条分支会 `std::process::exit`，
    /// 在测试进程里会直接把测试二进制干掉。
    #[test]
    fn parses_existing_options() {
        let a = parse_args(&argv(&["-p", "COM3", "-b", "115200", "-o", "--fps", "30"])).unwrap();
        assert_eq!(a.port.as_deref(), Some("COM3"));
        assert_eq!(a.baud, Some(115200));
        assert!(a.auto_open, "-o 应当打开串口");
        assert_eq!(a.fps, Some(30));
        assert!(!a.no_config, "默认要走配置持久化");
    }

    /// 长选项与短选项等价，`--port` / `--baud` 也一样认。
    #[test]
    fn parses_long_forms() {
        let a = parse_args(&argv(&["--port", "/dev/ttyUSB0", "--baud", "9600", "--open"])).unwrap();
        assert_eq!(a.port.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(a.baud, Some(9600));
        assert!(a.auto_open);
    }

    /// 不带任何参数时全部是默认值。
    #[test]
    fn defaults_are_empty() {
        let a = parse_args(&argv(&[])).unwrap();
        assert_eq!(a.port, None);
        assert_eq!(a.baud, None);
        assert!(!a.auto_open);
        assert_eq!(a.fps, None);
        assert!(!a.no_config);
    }

    /// `--no-config` 必须被认出来：它决定既不读也不写配置文件。
    #[test]
    fn recognizes_no_config() {
        let a = parse_args(&argv(&["--no-config"])).unwrap();
        assert!(a.no_config);
        // 与其它参数混用时也不能互相干扰。
        let b = parse_args(&argv(&["--no-config", "-p", "COM5"])).unwrap();
        assert!(b.no_config);
        assert_eq!(b.port.as_deref(), Some("COM5"));
    }

    /// 未知参数必须报错，而不是被默默忽略。
    #[test]
    fn unknown_option_is_an_error() {
        let e = parse_args(&argv(&["--nope"])).unwrap_err();
        assert!(e.contains("未知参数"), "错误信息里应说明是未知参数：{e}");
    }

    /// `--fps` 越界必须报错：帧率上限直接决定主循环的等待间隔，
    /// 0 会变成除零式的忙等，>120 则没有意义。
    #[test]
    fn fps_out_of_range_is_an_error() {
        assert!(parse_args(&argv(&["--fps", "0"])).is_err(), "0 应当被拒绝");
        assert!(parse_args(&argv(&["--fps", "121"])).is_err(), "121 应当被拒绝");
        assert!(parse_args(&argv(&["--fps", "abc"])).is_err(), "非数字应当被拒绝");
        // 边界值合法。
        assert_eq!(parse_args(&argv(&["--fps", "1"])).unwrap().fps, Some(1));
        assert_eq!(parse_args(&argv(&["--fps", "120"])).unwrap().fps, Some(120));
    }

    /// 缺取值时也要报错，而不是把下一个参数当成取值吞掉。
    #[test]
    fn missing_value_is_an_error() {
        assert!(parse_args(&argv(&["--port"])).is_err());
        assert!(parse_args(&argv(&["--baud"])).is_err());
        assert!(parse_args(&argv(&["--fps"])).is_err());
    }

    /// `--help` 文案必须列出全部子命令与新参数。
    ///
    /// 直接断言常量 `HELP`，不去跑 `parse_args`：`-h` 分支会退出进程。
    #[test]
    fn help_lists_subcommands_and_flags() {
        for token in [
            "list",
            "ports",
            "send",
            "listen",
            "watch",
            "selftest",
            "sim",
            "--json",
            "--dry-run",
            "--kind",
            "--no-config",
        ] {
            assert!(HELP.contains(token), "--help 文案缺少 {token}");
        }
    }

    /// 新增的子命令与参数必须出现在 `--help` 里。
    ///
    /// 脚本 / AI 是照着 `--help` 拼命令行的：文案里没有的参数，
    /// 调用方根本不会知道它存在。
    #[test]
    fn help_lists_new_subcommand_and_flags() {
        for token in [
            "record",       // 新子命令
            "--expect",     // send 的期望响应校验
            "--expect-timeout",
            "--format",     // listen / record 的输出形状
            "--out",        // record 的落盘路径
            "--duration",   // record 的录制时长
            "--verbose",    // list 的厂商 / 产品两列
            "退出码",       // 退出码表
            "期望",         // 退出码 5 的说明
        ] {
            assert!(HELP.contains(token), "--help 文案缺少 {token}");
        }
        // 退出码 5 必须写清楚它代表什么。
        assert!(HELP.contains('5'), "--help 文案里应当写明退出码 5");
        assert!(
            HELP.contains("响应与期望不符"),
            "退出码 5 的含义应当写明是「响应与期望不符」"
        );
    }

    /// `record` 必须注册成子命令。
    ///
    /// 不在 `SUBCOMMANDS` 里的话，`stui record ...` 会被当成 TUI 参数，
    /// 报一个莫名其妙的「未知参数」—— 脚本根本不知道有 record 这个功能。
    #[test]
    fn record_is_a_registered_subcommand() {
        assert!(SUBCOMMANDS.contains(&"record"), "record 应当是子命令");
        assert_eq!(SUBCOMMANDS.len(), 8, "子命令应当是 8 个");
        // cli 模块也要能分派到它：缺 --port 时应当报用法错误（退出码 2），
        // 而不是「未知子命令」。
        assert_eq!(cli::run(&argv(&["record"])), 2, "record 缺参数时应当报用法错误");
    }

    /// 每个注册在案的子命令都要能被 cli::run 真正分派到。
    ///
    /// 只看退出码不够分：子命令缺参数时也是 2，与「未知子命令」的 2 一样。
    /// 所以这里给每个子命令一套能跑通的最小参数 ——
    /// 拿到 2 以外的退出码，就说明它被分派到了自己的实现，
    /// 而不是掉进「未知子命令」那一档。
    #[test]
    fn every_registered_subcommand_is_dispatched() {
        // list / ports：没有必填参数，直接枚举。
        assert_ne!(cli::run(&argv(&["list"])), 2, "list 应当被分派");
        assert_ne!(cli::run(&argv(&["ports"])), 2, "ports 应当被分派");
        // selftest：内置回环自检，不碰真实串口。
        assert_ne!(cli::run(&argv(&["selftest"])), 2, "selftest 应当被分派");
        // sim：时长给 0，起来就停。
        assert_ne!(cli::run(&argv(&["sim", "--duration", "0"])), 2, "sim 应当被分派");
        // send：dry-run 只解析不发送。
        assert_eq!(
            cli::run(&argv(&["send", "--port", "sim", "--text", "hi", "--dry-run"])),
            0,
            "send 应当被分派"
        );
        // listen：sim 回环没有数据，200 ms 后按超时收工（退出码 4）。
        assert_ne!(
            cli::run(&argv(&["listen", "--port", "sim", "--bytes", "1", "--timeout", "200"])),
            2,
            "listen 应当被分派"
        );
        // watch：挂 100 ms 就收工。
        assert_ne!(
            cli::run(&argv(&["watch", "--port", "sim", "--timeout", "100"])),
            2,
            "watch 应当被分派"
        );
        // record：录 100 ms 到临时文件里。
        let out = std::env::temp_dir().join(format!("stui-main-record-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&out);
        assert_ne!(
            cli::run(&argv(&[
                "record",
                "--port",
                "sim",
                "--duration",
                "100",
                "--out",
                out.to_str().unwrap(),
            ])),
            2,
            "record 应当被分派"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// 既有参数一个都不能少：全给在一起，也要原样解析出来。
    #[test]
    fn all_existing_options_still_parse() {
        let a = parse_args(&argv(&[
            "-p", "COM3", "-b", "9600", "-o", "--fps", "15", "--no-config",
        ]))
        .unwrap();
        assert_eq!(a.port.as_deref(), Some("COM3"));
        assert_eq!(a.baud, Some(9600));
        assert!(a.auto_open, "-o 应当打开串口");
        assert_eq!(a.fps, Some(15));
        assert!(a.no_config, "--no-config 应当被认出来");
    }

    /// `--help` 里必须写清新参数与鼠标支持。
    ///
    /// 脚本 / AI 只能照着 `--help` 学怎么调：文案里没有的能力，
    /// 调用方根本不会知道它存在。
    #[test]
    fn help_lists_new_cli_flags_and_mouse_support() {
        for token in [
            "--save",   // listen 落盘
            "--stdout", // record 原始字节冲一份到 stdout
            "--idle",   // listen 空闲收工
            "--count",  // sim 重复发送次数
            "--bits",
            "--parity",
            "--stops",
            "--flow",
            "--version", // 版本号
            "鼠标",     // 鼠标支持
            "滚轮",     // 滚轮滚动数据区
        ] {
            assert!(HELP.contains(token), "--help 文案缺少 {token}");
        }
    }

    // --------------------------------------------------------------
    // 鼠标
    // --------------------------------------------------------------

    /// 终端页：滚轮上下滚动数据区，并且和方向键一样关掉自动跟随。
    #[test]
    fn mouse_wheel_scrolls_the_data_area() {
        let mut app = App::new();
        // 先脱离跟随才能自己设定位置（跟随时位置由渲染层决定）。
        app.auto_scroll = false;
        app.scroll_top = 10;
        app.clamp_scroll(100); // 记录可滚上限 100

        assert!(on_mouse(&mut app, mouse(MouseEventKind::ScrollUp)), "滚轮上应当改变状态");
        assert_eq!(app.scroll_top, 7, "滚轮上应当上滚 3 行");
        assert!(!app.auto_scroll, "手动滚动应当关掉自动跟随");

        assert!(on_mouse(&mut app, mouse(MouseEventKind::ScrollDown)), "滚轮下应当改变状态");
        assert_eq!(app.scroll_top, 10, "滚轮下应当下滚 3 行");
    }

    /// 日志 / 统计 / 帮助页：滚轮改的是 log_scroll，且上下界夹得住。
    #[test]
    fn mouse_wheel_scrolls_log_pages() {
        let mut app = App::new();
        app.tab = 1;
        // 日志页能滚的距离取决于 SYS 记录的条数，先造一些出来。
        for i in 0..20 {
            app.push_sys(format!("消息 {i}"));
        }
        app.log_scroll = 10;

        assert!(on_mouse(&mut app, mouse(MouseEventKind::ScrollUp)));
        assert_eq!(app.log_scroll, 7, "滚轮上应当上滚 3 行");
        assert!(on_mouse(&mut app, mouse(MouseEventKind::ScrollDown)));
        assert_eq!(app.log_scroll, 10, "滚轮下应当下滚 3 行");

        // 上界：不能超过日志的总条数。
        app.log_scroll = 19;
        on_mouse(&mut app, mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.log_scroll, 20, "滚轮下应当被日志条数夹住");
        // 下界：不能变成负数。
        app.log_scroll = 1;
        on_mouse(&mut app, mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.log_scroll, 0, "滚轮上应当夹在 0");
    }

    /// 点击、移动、拖拽一律忽略：界面里没有鼠标交互，
    /// 自己给这些事件编一套行为只会让用户误操作。
    #[test]
    fn other_mouse_events_are_ignored() {
        let mut app = App::new();
        app.push_sys("一条日志");
        let before = (app.scroll_top, app.log_scroll, app.auto_scroll, app.sel, app.focus);
        for kind in [
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            MouseEventKind::Moved,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert!(!on_mouse(&mut app, mouse(kind)), "{kind:?} 不该改变界面");
        }
        assert_eq!(
            (app.scroll_top, app.log_scroll, app.auto_scroll, app.sel, app.focus),
            before,
            "非滚轮事件不许碰界面状态"
        );
    }

    /// 弹窗开着时滚轮不生效：这时鼠标归弹窗，
    /// 去滚底下的数据区用户看不见，还会顺手把自动跟随关掉。
    #[test]
    fn mouse_wheel_is_ignored_while_a_modal_is_open() {
        let mut app = App::new();
        app.modal = Some(app::Modal::Filter(String::new()));
        app.scroll_top = 10;
        assert!(!on_mouse(&mut app, mouse(MouseEventKind::ScrollUp)));
        assert_eq!(app.scroll_top, 10, "弹窗开着时不该滚数据区");
    }

    // --------------------------------------------------------------
    // 过滤开关的持久化
    // --------------------------------------------------------------

    /// 过滤开关：文件里写了才生效，没写就保留界面默认值。
    ///
    /// 「没写」与「写了 false」对这两个开关的含义正好相反，
    /// 分不清的话，用户在界面上关掉的开关每次启动都会被打开。
    #[test]
    fn config_applies_filter_switches_only_when_written() {
        let mut app = app_with_ports(&["COM1"]);
        apply_config(&mut app, &Config::default(), true, &written(&["port"]));
        assert!(!app.filter_ci, "没写 filter_ci 时应当保留默认值");
        assert!(!app.filter_invert, "没写 filter_invert 时应当保留默认值");

        let mut app2 = app_with_ports(&["COM1"]);
        apply_config(
            &mut app2,
            &Config::default(),
            true,
            &written_with_bools(&[("filter_ci", false), ("filter_invert", true)]),
        );
        assert!(!app2.filter_ci, "写了 false 应当照办");
        assert!(app2.filter_invert, "写了 true 应当生效");
    }

    /// 没有配置文件时，过滤开关一律不生效（与其它设置同一条规矩）。
    #[test]
    fn config_filter_switches_are_ignored_without_file() {
        let mut app = app_with_ports(&["COM1"]);
        apply_config(
            &mut app,
            &Config::default(),
            false,
            &written_with_bools(&[("filter_ci", true), ("filter_invert", true)]),
        );
        assert!(!app.filter_ci, "没有配置文件时应当保留 App 默认值");
        assert!(!app.filter_invert, "没有配置文件时应当保留 App 默认值");
    }

    /// 顶层布尔值只认顶层：嵌套对象里的同名键不算数。
    #[test]
    fn top_level_bools_reads_only_toplevel_boolean_values() {
        let text = r#"{"port":"COM3","filter_ci":true,"filter_invert":false,"n":1,"s":"true","nested":{"filter_ci":true}}"#;
        let v = top_level_bools(text);
        assert!(v.contains(&("filter_ci".to_string(), true)), "应当认出 filter_ci=true：{v:?}");
        assert!(v.contains(&("filter_invert".to_string(), false)), "应当认出 filter_invert=false：{v:?}");
        // 嵌套里的同名键不算数，字符串 "true" 也不是布尔值。
        assert_eq!(v.iter().filter(|(k, _)| k == "filter_ci").count(), 1, "{v:?}");
        assert_eq!(v.iter().filter(|(k, _)| k == "s").count(), 0, "{v:?}");
    }

    /// 两个过滤开关要能写回配置文件，并且读回来还是原值。
    #[test]
    fn filter_switches_are_written_into_the_config_file() {
        let path = std::env::temp_dir().join(format!("stui-filter-cfg-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"version":1,"port":"COM3"}"#).unwrap();

        write_filter_switches(&path, true, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"filter_ci\":true"), "文件里应当有 filter_ci：{text}");
        assert!(text.contains("\"filter_invert\":false"), "文件里应当有 filter_invert：{text}");
        // 原来的内容不能被弄丢。
        assert!(text.contains("\"port\":\"COM3\""), "原有字段应当还在：{text}");
        // 花括号要配平：插入不能把 JSON 弄坏。
        assert_eq!(
            text.matches('{').count(),
            text.matches('}').count(),
            "花括号应当配平：{text}"
        );

        // 读回来还是原值。
        let w = WrittenKeys { keys: top_level_keys(&text), bools: top_level_bools(&text) };
        assert_eq!(w.bool_of("filter_ci"), Some(true));
        assert_eq!(w.bool_of("filter_invert"), Some(false));
        // 别的顶层键不能被插坏。
        assert!(w.contains("port"), "原有顶层键应当还在：{text}");
        assert!(w.contains("version"), "原有顶层键应当还在：{text}");

        // 已经写过的键不会被重复插入（将来 config 模块自己管这两个字段时
        // 也不会变成重复键）。
        write_filter_switches(&path, false, true).unwrap();
        let text2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text2.matches("\"filter_ci\"").count(), 1, "{text2}");
        assert!(text2.contains("\"filter_ci\":true"), "已有键的值不该被改写：{text2}");

        let _ = std::fs::remove_file(&path);
    }

    // --------------------------------------------------------------
    // 配置应用
    // --------------------------------------------------------------

    /// 造一个带指定端口的 App（不碰真实串口）。
    fn app_with_ports(names: &[&str]) -> App {
        let mut app = App::new();
        app.ports = names
            .iter()
            .map(|n| port::PortEntry { port: n.to_string(), desc: String::new() })
            .collect();
        app
    }

    /// 记录里是否出现过这段文本。
    fn sys_log_mentions(app: &App, needle: &str) -> bool {
        app.records
            .iter()
            .any(|r| r.data.windows(needle.len()).any(|w| w == needle.as_bytes()))
    }

    /// 端口名按不区分大小写匹配：用户写 `com3`，系统枚举出的是 `COM3`。
    #[test]
    fn config_port_matches_case_insensitively() {
        let mut app = app_with_ports(&["COM1", "COM3"]);
        let cfg = Config { port: Some("com3".to_string()), ..Config::default() };
        apply_config(&mut app, &cfg, true, &WrittenKeys::default());
        assert_eq!(app.port_idx, 1, "应当选中 COM3");
    }

    /// 配置里的端口不存在时保留默认选择，并记一条 SYS 日志。
    #[test]
    fn config_port_missing_keeps_default() {
        let mut app = app_with_ports(&["COM1"]);
        let before = app.port_idx;
        let cfg = Config { port: Some("COM9".to_string()), ..Config::default() };
        apply_config(&mut app, &cfg, true, &WrittenKeys::default());
        assert_eq!(app.port_idx, before, "找不到端口时应当保留默认");
        assert!(sys_log_mentions(&app, "COM9"), "应当记一条提到 COM9 的 SYS 日志");
    }

    /// 波特率必须在快捷列表里，否则保留默认值并记一条日志。
    #[test]
    fn config_baud_must_be_in_bauds() {
        let mut app = app_with_ports(&["COM1"]);
        let default_idx = app.baud_idx;

        let ok = Config { baud: Some(9600), ..Config::default() };
        apply_config(&mut app, &ok, true, &WrittenKeys::default());
        let idx_9600 = port::BAUDS.iter().position(|&b| b == 9600).expect("9600 在 BAUDS 里");
        assert_eq!(app.baud_idx, idx_9600, "9600 在 BAUDS 里，应当被选中");

        let bad = Config { baud: Some(12345), ..Config::default() };
        apply_config(&mut app, &bad, true, &WrittenKeys::default());
        assert_eq!(app.baud_idx, idx_9600, "不在 BAUDS 里的波特率应当保留当前值");
        assert!(sys_log_mentions(&app, "12345"), "应当记一条提到 12345 的 SYS 日志");
        assert_ne!(default_idx, idx_9600, "这条断言只是确认默认值本身不是 9600");
    }

    /// 串口参数用 port.rs 的 from_code 解析，非法值保留默认。
    #[test]
    fn config_applies_port_codes() {
        let mut app = app_with_ports(&["COM1"]);
        let cfg = Config {
            bits: Some(7),
            parity: Some("even".to_string()),
            stops: Some(2),
            flow: Some("hardware".to_string()),
            view: Some("hex".to_string()),
            ..Config::default()
        };
        apply_config(&mut app, &cfg, true, &WrittenKeys::default());
        assert_eq!(app.bits, port::Bits::Seven);
        assert_eq!(app.parity, port::Parity::Even);
        assert_eq!(app.stops, port::Stops::Two);
        assert_eq!(app.flow, port::Flow::Hardware);
        // View 没有实现 Debug，用 assert! 而不是 assert_eq!。
        assert!(app.view == port::View::Hex);
    }

    /// 非法编码不能把界面带进一个坏状态：保留默认值即可。
    #[test]
    fn config_rejects_bad_codes() {
        let mut app = app_with_ports(&["COM1"]);
        let defaults = (app.bits, app.parity, app.stops, app.flow, app.view);
        let cfg = Config {
            bits: Some(9),
            parity: Some("weird".to_string()),
            stops: Some(3),
            flow: Some("rts".to_string()),
            view: Some("bin".to_string()),
            ..Config::default()
        };
        apply_config(&mut app, &cfg, true, &WrittenKeys::default());
        assert_eq!(app.bits, defaults.0);
        assert_eq!(app.parity, defaults.1);
        assert_eq!(app.stops, defaults.2);
        assert_eq!(app.flow, defaults.3);
        assert!(app.view == defaults.4);
    }

    /// interval_ms 直接用；fps 必须在 1~120 之间。
    #[test]
    fn config_applies_interval_and_fps() {
        let mut app = app_with_ports(&["COM1"]);
        let cfg = Config { interval_ms: Some(250), fps: Some(30), ..Config::default() };
        apply_config(&mut app, &cfg, true, &WrittenKeys::default());
        assert_eq!(app.interval_ms, 250);
        assert_eq!(app.fps, 30);

        let mut app2 = app_with_ports(&["COM1"]);
        let default_fps = app2.fps;
        let bad = Config { fps: Some(500), ..Config::default() };
        apply_config(&mut app2, &bad, true, &WrittenKeys::default());
        assert_eq!(app2.fps, default_fps, "越界的帧率应当保留默认值");
    }

    /// 配置文件不存在时，两个开关保留 App 自己的默认值。
    ///
    /// `Config::default()` 里 show_ms 是 false，而 App 的默认是 true；
    /// 无条件赋值会让"第一次启动"的时间戳精度从毫秒变成秒。
    #[test]
    fn switches_follow_app_default_without_config_file() {
        let mut app = app_with_ports(&["COM1"]);
        let app_show_ms = app.show_ms;
        apply_config(&mut app, &Config::default(), false, &WrittenKeys::default());
        assert_eq!(app.show_ms, app_show_ms, "没有配置文件时应当保留 App 默认值");

        let mut app2 = app_with_ports(&["COM1"]);
        let cfg = Config { show_ms: false, auto_reconnect: true, ..Config::default() };
        apply_config(&mut app2, &cfg, true, &WrittenKeys::default());
        assert!(!app2.show_ms, "有配置文件时应当照配置文件来");
        assert!(app2.auto_reconnect);
    }

    /// 写回配置的端口名取当前选中的端口，波特率取 BAUDS 里的那一档。
    #[test]
    fn saved_config_uses_current_selection() {
        let mut app = app_with_ports(&["COM1", "COM3"]);
        app.port_idx = 1;
        app.baud_idx = 0; // 9600
        let cfg = config_from_app(&app);
        assert_eq!(cfg.port.as_deref(), Some("COM3"));
        assert_eq!(cfg.baud, Some(9600));
        // 编码也要能原样读回来：写进去的字符串必须 from_code 认得。
        assert_eq!(port::bits_from_code(&cfg.bits.unwrap().to_string()), Some(app.bits));
        assert_eq!(port::parity_from_code(cfg.parity.as_deref().unwrap()), Some(app.parity));
        assert_eq!(port::stops_from_code(&cfg.stops.unwrap().to_string()), Some(app.stops));
        assert_eq!(port::flow_from_code(cfg.flow.as_deref().unwrap()), Some(app.flow));
        // View 没有实现 Debug，用 assert! 而不是 assert_eq!。
        assert!(port::view_from_code(cfg.view.as_deref().unwrap()) == Some(app.view));
        // 两个新开关也要跟着界面上的取值存回去。
        app.wrap = true;
        app.auto_ports = false;
        let cfg2 = config_from_app(&app);
        assert!(cfg2.wrap, "wrap 应当被存进配置");
        assert!(!cfg2.auto_ports, "auto_ports 应当被存进配置");
    }

    // --------------------------------------------------------------
    // wrap / auto_ports
    // --------------------------------------------------------------

    /// wrap 直接照配置文件来：界面的默认值恰好也是 false，
    /// 所以「没写」与「写了 false」不会打架。
    #[test]
    fn config_applies_wrap() {
        let mut app = app_with_ports(&["COM1"]);
        let cfg = Config { wrap: true, ..Config::default() };
        apply_config(&mut app, &cfg, true, &written(&["wrap"]));
        assert!(app.wrap, "配置里的 wrap=true 应当生效");

        // 没写这个键时保持界面默认值。
        let mut app2 = app_with_ports(&["COM1"]);
        apply_config(&mut app2, &Config::default(), true, &WrittenKeys::default());
        assert!(!app2.wrap, "没写 wrap 时应当保留 App 默认值");
    }

    /// auto_ports 的默认值是 true，不能被「没写」误伤。
    ///
    /// 这是本次改动最容易踩的坑：`Config` 里的开关是裸 bool，
    /// 老版本或手写的配置里根本没有 auto_ports 这个键，
    /// 无条件赋值会把它当成 false —— 用户没关过，却自动被关了。
    #[test]
    fn config_keeps_auto_ports_when_key_absent() {
        let mut app = app_with_ports(&["COM1"]);
        assert!(app.auto_ports, "App 的默认应当是开启自动刷新");
        let cfg = Config { auto_ports: false, ..Config::default() };
        apply_config(&mut app, &cfg, true, &written(&["port", "baud"]));
        assert!(app.auto_ports, "文件里没写 auto_ports 时应当保留 true");
    }

    /// 文件里确实写了 auto_ports=false 时，必须照办：
    /// 用户明确关掉了，下次启动就不该自动开回来。
    #[test]
    fn config_applies_auto_ports_when_key_present() {
        let mut app = app_with_ports(&["COM1"]);
        let cfg = Config { auto_ports: false, ..Config::default() };
        apply_config(&mut app, &cfg, true, &written(&["auto_ports"]));
        assert!(!app.auto_ports, "文件里明确写了 false 就应当关掉");

        let mut app2 = app_with_ports(&["COM1"]);
        let cfg2 = Config { auto_ports: true, ..Config::default() };
        apply_config(&mut app2, &cfg2, true, &written(&["auto_ports"]));
        assert!(app2.auto_ports, "文件里写了 true 应当保持开启");
    }

    /// 没有配置文件时，开关一律不生效（与其它设置同一条规矩）。
    #[test]
    fn config_switches_are_ignored_without_file() {
        let mut app = app_with_ports(&["COM1"]);
        let cfg = Config { wrap: true, auto_ports: false, ..Config::default() };
        apply_config(&mut app, &cfg, false, &written(&["wrap", "auto_ports"]));
        assert!(!app.wrap, "没有配置文件时应当保留 App 默认值");
        assert!(app.auto_ports, "没有配置文件时应当保留 App 默认值");
    }

    // --------------------------------------------------------------
    // 键名清单
    // --------------------------------------------------------------

    /// 顶层键名要能扫出来，嵌套里的同名键不算数。
    #[test]
    fn top_level_keys_finds_only_toplevel_keys() {
        let text = r#"{"port":"COM3","auto_ports":false,"wrap":true,"nested":{"auto_ports":true}}"#;
        let keys = top_level_keys(text);
        assert!(keys.contains(&"auto_ports".to_string()), "应当认出 auto_ports：{keys:?}");
        assert!(keys.contains(&"wrap".to_string()), "应当认出 wrap：{keys:?}");
        // 嵌套对象里的键不参与顶层判断。
        assert_eq!(keys.iter().filter(|k| *k == "auto_ports").count(), 1, "{keys:?}");
    }

    /// 键名里带转义与中文也不能把扫描器带偏。
    #[test]
    fn top_level_keys_handles_escapes() {
        let text = r#"{"port":"C:\\\"x\"","auto_ports":false}"#;
        let keys = top_level_keys(text);
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert_eq!(keys[0], "port");
        assert_eq!(keys[1], "auto_ports");
    }

    /// 文件不存在时清单为空：拿不准就当成「没写」。
    #[test]
    fn written_keys_of_missing_file_is_empty() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("stui-no-such-config-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let w = WrittenKeys::read_from(&path);
        assert!(!w.contains("auto_ports"));
        assert!(!w.contains("wrap"));
    }

    /// 真实文件读出来的清单能认出开关键。
    #[test]
    fn written_keys_reads_real_file() {
        let path = std::env::temp_dir()
            .join(format!("stui-written-keys-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"port":"COM3","auto_ports":false}"#).unwrap();
        let w = WrittenKeys::read_from(&path);
        assert!(w.contains("auto_ports"), "{:?}", w.keys);
        assert!(!w.contains("wrap"), "{:?}", w.keys);
        let _ = std::fs::remove_file(&path);
    }
}


