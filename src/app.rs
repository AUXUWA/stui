//! 应用状态与交互逻辑：连接管理、收发、记录文件、按键分发。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::fmt::{self, Stamp};
use crate::port::{self, Bits, Flow, Link, Parity, PortCfg, PortEntry, RxMsg, Stops, View, BAUDS};

/// 接收缓冲区上限（滚动丢弃最旧数据，避免无限增长）。
const MAX_RECORDS: usize = 20_000;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 日志默认目录。
const LOG_DIR: &str = "logs";
/// 接收端「行缓冲」的空闲封行时间：这么久没有新字节，就把当前行收尾，
/// 下一次到达的字节另起一行。设备一个字符一个字符地发时，全行只会占一行。
const RX_IDLE_SEAL: Duration = Duration::from_millis(180);
/// 当前行（尚未遇到换行的数据）的硬上限。设备只用 `\r` 刷进度条时，
/// 当前行永远等不到换行与空闲，不设上限就会无限膨胀并拖垮渲染帧率。
const MAX_PENDING: usize = 256 * 1024;
/// 界面重绘上限的默认值（帧/秒）。跑在 SSH / 容器 / 网页终端这类弱链路上时，
/// 用 `--fps` 往下调即可，20 帧已足够看清串口数据。
pub const DEFAULT_FPS: u32 = 20;
/// `Shift+F` 在这几档之间循环。
pub const FPS_STEPS: [u32; 4] = [5, 10, 20, 30];
/// 发送历史最多保留的条数。调设备时来回切几条命令就够了，
/// 再多只会让翻历史变慢，而且没人会翻到 100 条之前。
const MAX_HISTORY: usize = 100;
/// 自动重连的尝试间隔。2 秒足够快，也不至于把设备刷到起不来。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// 自动重连连续失败多少次才记一条日志。
/// 设备没插好时会一直失败，每次都记日志很快就把记录刷满了。
const RECONNECT_LOG_EVERY: u32 = 5;
/// 速率统计的窗口长度：满 1 秒就算一次平均速度。
const RATE_WINDOW: Duration = Duration::from_secs(1);
/// 自动刷新端口列表的间隔。
///
/// 5 秒足够跟上「插上 / 拔掉设备」这种变化，又不至于把枚举的代价摊到每一帧。
const AUTO_PORTS_INTERVAL: Duration = Duration::from_secs(5);

/// 两个端口列表是否不同（按端口名与描述逐项比较）。
///
/// `PortEntry` 没有实现 `PartialEq`，这里自己比：
/// 端口名一样但描述变了（比如换了 USB 描述串）也算有变化。
fn ports_differ(a: &[PortEntry], b: &[PortEntry]) -> bool {
    a.len() != b.len() || a.iter().zip(b).any(|(x, y)| x.port != y.port || x.desc != y.desc)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Rx,
    Tx,
    Sys,
}

impl Dir {
    pub fn tag(self) -> &'static str {
        match self {
            Dir::Rx => "RX",
            Dir::Tx => "TX",
            Dir::Sys => "SYS",
        }
    }
}

pub struct Record {
    pub t: SystemTime,
    pub dir: Dir,
    pub data: Vec<u8>,
    /// `data` 里 `\n` 的个数，随追加增量维护。
    ///
    /// 文本视图每帧都要给每条记录数行数；直接调 `display_lines()` 会让每帧
    /// 产生「记录数」个 Vec 分配，几千条记录就能把 CPU 打满。缓存计数后是 O(1)。
    /// `data` 只在 feed_rx 里追加、在 commit 里截尾，两处都会同步这个计数。
    nl: usize,
    /// 最近一次追加字节的时刻，用于接收端的空闲封行判断。
    pub last_rx: Instant,
    /// 时间戳显示串的缓存：「HH:MM:SS.mmm」。
    ///
    /// 时间戳**永不改变**（合并行取首字节的时刻，后到的字节不刷新它），
    /// 所以这个缓存可以一直用，不必像别的显示缓存那样考虑作废。
    /// 渲染层每帧要为每一行格式化一次时间戳，缓存下来就只剩一次分配。
    stamp_cache: OnceLock<String>,
    /// 完整显示形态的缓存（各行文本 + 各行列宽 + 整段文本）。
    ///
    /// **必须能作废**：正在累积的那条记录（`pending_rx`）每来一批字节就要
    /// 追加一次，缓存若不失效，过滤就会一直按旧内容判命中 ——
    /// 用户看到「命中 N 条」却漏掉了刚到的数据。
    ///
    /// 用 `OnceLock` + `take()` 而不是 `RefCell`：`display_text()` 要返回
    /// `&str`，`RefCell` 的借用守卫没法把字符串安全地借出去；
    /// `OnceLock::take()` 同样能做到「填上之后还能作废」，且不引入运行时检查。
    view: OnceLock<fmt::TextView>,
    /// 折行后占几行的缓存：`(折行宽度, 行数)`，只留一格。
    ///
    /// 界面每帧都要给每条记录算行高。逐条重新折一遍的话，开销随缓冲里的
    /// **内容**线性增长 —— 缓冲越大，每帧越慢。面板宽度很少变，
    /// 一格缓存就足以把「每帧重算」变成「宽度不变时零计算」。
    ///
    /// 用 `RefCell<Option<…>>` 而不是 `OnceLock`：这里要按宽度换值，
    /// `OnceLock` 填上之后就没法换（`take()` 需要 `&mut self`，
    /// 而 `visual_rows` 只拿得到 `&self`）。数据一变（追加 / 截尾）
    /// 就由 [`Record::invalidate`] 作废。
    visual_rows_cache: RefCell<Option<(usize, usize)>>,
}

impl Record {
    pub fn new(dir: Dir, data: &[u8]) -> Record {
        Record::build(dir, SystemTime::now(), data.to_vec(), Instant::now())
    }
    /// 构造一条接收记录：时间戳与「最后到达时刻」由调用方给。
    ///
    /// 合并行取**首字节**的时刻，后到的字节不刷新它；
    /// 「最后到达时刻」则每次追加都要更新（空闲封行靠它判断）。
    fn rx_at(t: SystemTime, data: Vec<u8>, last_rx: Instant) -> Record {
        Record::build(Dir::Rx, t, data, last_rx)
    }
    /// 记录的唯一构造入口：换行计数与各显示缓存都在这里初始化，
    /// 少一处构造就少一处「忘了重算换行数 / 忘了建缓存」的可能。
    fn build(dir: Dir, t: SystemTime, data: Vec<u8>, last_rx: Instant) -> Record {
        Record {
            t,
            dir,
            nl: fmt::count_nl(&data),
            data,
            last_rx,
            stamp_cache: OnceLock::new(),
            view: OnceLock::new(),
            visual_rows_cache: RefCell::new(None),
        }
    }
    pub fn stamp(&self) -> Stamp {
        fmt::stamp(self.t)
    }
    /// 这条记录在界面上显示的文本（按真实换行分行，与高亮用的是同一份）。
    ///
    /// 语义与从前完全一致：各逻辑行的显示文本用 `\n` 连接。
    /// 内部改走 [`Self::text_view`]：显示形态只算一次，过滤与渲染共用同一份，
    /// 不会出现「过滤按一套文本判命中、画面画的是另一套」。
    #[allow(dead_code)] // 过滤判据改成按逻辑行判之后，运行时没人读整段文本了
    pub fn display_text(&self) -> &str {
        &self.text_view().text
    }
    /// 文本模式下一条记录占用的终端行数：只按数据里的真实换行计算，
    /// 没有换行的长数据不会被硬折成多行。
    ///
    /// 用缓存的换行计数，结果与 `display_lines().len()` 一致但不分配内存。
    pub fn text_lines(&self) -> usize {
        fmt::line_count_from_nl(self.nl, self.data.last() == Some(&b'\n'))
    }
    pub fn hex_lines(&self, per_line: usize) -> usize {
        if per_line == 0 {
            1
        } else {
            self.data.len().div_ceil(per_line).max(1)
        }
    }
    /// 每条逻辑行的显示列宽（终端列，不是字节数）。
    ///
    /// 与显示形态是同一份数据（列宽就是解码时算出来的那一份），
    /// 所以直接取 [`Self::text_view`] 的列宽，不再单独缓存一份。
    #[allow(dead_code)] // 界面层接上之前没人读
    pub fn line_cols(&self) -> &[u32] {
        &self.text_view().cols
    }
    /// 时间戳的显示串：「HH:MM:SS.mmm」。
    ///
    /// 时间戳永不改变，所以这个缓存永久有效。
    #[allow(dead_code)] // 界面层接上之前没人读
    pub fn stamp_text(&self) -> &str {
        self.stamp_cache.get_or_init(|| self.stamp().hms_ms())
    }
    /// 完整显示形态：各行文本、各行列宽、整段文本。
    ///
    /// 惰性算一次；数据变了（追加 / 截尾）缓存会作废。
    pub fn text_view(&self) -> &fmt::TextView {
        self.view.get_or_init(|| fmt::build_text_view(&self.data))
    }
    /// 这条记录在折行宽度 `width` 下占几行。
    ///
    /// 等于各条逻辑行的 [`fmt::wrap_rows`] 之和：
    /// 逻辑行按数据里真实的换行切分，逐条折行后求和 ——
    /// 与界面画出来的东西同口径，行数才不会和画面对不上。
    ///
    /// 结果按 `(宽度, 行数)` 缓存一格：面板宽度很少变，
    /// 于是「每帧重算」变成「宽度不变时零计算」，每帧的开销不再
    /// 随缓冲里的**内容**增长。数据一变（追加 / 截尾）缓存即作废。
    #[allow(dead_code)] // 界面层接上之前没人读
    pub fn visual_rows(&self, width: usize) -> usize {
        if let Some((w, rows)) = *self.visual_rows_cache.borrow() {
            if w == width {
                return rows;
            }
        }
        let rows = self.compute_visual_rows(width);
        *self.visual_rows_cache.borrow_mut() = Some((width, rows));
        rows
    }
    /// 折行行数的实际计算（缓存未命中时走这里）。
    #[allow(dead_code)] // 随 visual_rows 一起被界面层接上
    fn compute_visual_rows(&self, width: usize) -> usize {
        let view = self.text_view();
        view.lines
            .iter()
            .zip(view.cols.iter())
            .map(|(line, &cols)| fmt::wrap_rows(line, cols, width))
            .sum()
    }
    /// 作废与数据有关的显示缓存。
    ///
    /// 只在数据真的变了时调用（追加字节 / 截尾）。
    /// 时间戳缓存不作废：时间戳永远不会变，留着能省掉每帧一次格式化。
    fn invalidate(&mut self) {
        self.view.take();
        *self.visual_rows_cache.borrow_mut() = None;
    }
}

/// 字符串指纹：脏标记用。
///
/// 不能只比较长度 —— 两条等长的提示（"串口已打开" / "串口已关闭"）
/// 会让界面停在旧的那条上，看起来像没反应。
fn str_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    /// 左侧参数栏
    Params,
    /// 终端页的发送输入框（此时可打印字符都是待发送数据）
    Input,
    /// 终端页的数据区（此时方向键与 v/x/l/o 等命令生效）
    Stream,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Status,
    Port,
    Baud,
    Bits,
    Parity,
    Stops,
    Flow,
    View,
    Interval,
    Toggle,
    Refresh,
}

/// 左栏行顺序，与 `ROW_LABEL` 一一对应（每个标签占 10 个终端列）。
pub const ROWS: [Row; 11] = [
    Row::Status,
    Row::Port,
    Row::Baud,
    Row::Bits,
    Row::Parity,
    Row::Stops,
    Row::Flow,
    Row::View,
    Row::Interval,
    Row::Toggle,
    Row::Refresh,
];

pub const ROW_LABEL: [(&str, &str); 11] = [
    // 第 0 行也要有等宽标签：空标签会让这一行的值比其它行早 8 列开始，
    // 值列就错开了。
    ("状态      ", "状态"),
    ("端口      ", "端口"),
    ("波特率    ", "波特率"),
    ("数据位    ", "数据位"),
    ("校验      ", "校验"),
    ("停止位    ", "停止位"),
    ("流控      ", "流控"),
    ("接收视图  ", "接收视图"),
    ("循环间隔  ", "循环间隔"),
    ("打开串口  ", "打开串口"),
    ("刷新列表  ", "刷新列表"),
];

pub enum Modal {
    FilePath(String),
    /// 过滤关键字输入框。
    Filter(String),
    /// 记录详情弹窗。
    ///
    /// `idx` 是记录在**过滤后**序列里的序号：过滤为空时就是全部记录，
    /// 过滤生效时只在命中的记录之间翻，不会跳到被过滤掉的记录上。
    /// `hex` 为真时内容按 HEX 显示。
    Detail { idx: usize, hex: bool },
}

pub struct Recorder {
    pub path: PathBuf,
    out: BufWriter<File>,
    since_flush: usize,
    /// 累计写失败次数。磁盘满、日志目录被删等情况下，
    /// 界面还显示「⏺ REC」而文件是空的，必须让用户看得见。
    errors: u64,
    /// 首次写失败的原因，用于给用户一句可执行的提示。
    last_err: String,
}

impl Recorder {
    fn start() -> std::io::Result<Recorder> {
        std::fs::create_dir_all(LOG_DIR)?;
        let name = format!("stui-{}.log", fmt::stamp(SystemTime::now()).compact());
        let path = std::path::Path::new(LOG_DIR).join(name);
        let out = BufWriter::new(OpenOptions::new().create(true).append(true).open(&path)?);
        Ok(Recorder { path, out, since_flush: 0, errors: 0, last_err: String::new() })
    }

    fn write(&mut self, rec: &Record) {
        let head = format!("[{}] {:<3}", rec.stamp().full(), rec.dir.tag());
        let body = match rec.dir {
            // 文本内容原样落盘，控制字符统一可视化，保证日志可读。
            Dir::Sys => String::from_utf8_lossy(&rec.data).into_owned(),
            _ => fmt::hex_bytes(&rec.data),
        };
        let ascii = match rec.dir {
            Dir::Sys => String::new(),
            _ => format!("  |{}|", fmt::preview_text(&rec.data)),
        };
        if let Err(e) = writeln!(self.out, "{head} {body}{ascii}") {
            self.errors += 1;
            if self.errors == 1 {
                self.last_err = e.to_string();
            }
        }
        self.since_flush += 1;
    }

    fn tick(&mut self) {
        if self.since_flush > 0 {
            let _ = self.out.flush();
            self.since_flush = 0;
        }
    }

    /// 取出（并清零）自上次检查以来的写失败次数与首个错误原因。
    fn take_errors(&mut self) -> Option<String> {
        if self.errors == 0 {
            return None;
        }
        self.errors = 0;
        Some(std::mem::take(&mut self.last_err))
    }
}

/// 过滤结果的缓存。
///
/// 过滤是每帧都要做的重活（逐条记录、逐逻辑行判命中），而一帧里它会被问好几次：
/// 数据区要一份记录列表，详情弹窗又要一条记录与一个总数。
/// 算一次存下来，后来的人直接拿缓存 —— 一帧只扫一遍。
struct FilterCache {
    /// 算这份缓存时的**数据**版本号。
    data: u64,
    /// 算这份缓存时已封口的记录条数。
    len: usize,
    /// 算这份缓存时正在累积的那条记录的长度（没有累积行就是 `None`）。
    pending: Option<usize>,
    /// 算这份缓存时的关键字。
    keyword: String,
    /// 算这份缓存时的「忽略大小写」开关。
    ci: bool,
    /// 算这份缓存时的「排除匹配」开关。
    invert: bool,
    /// **显示**出来的记录在流里的位置（排除模式下是未命中的那些）。
    ///
    /// 存位置而不是引用：引用没法存在自己身上（自引用），位置可以 ——
    /// 要记录时按位置回查即可。
    shown: Vec<usize>,
    /// 命中关键字的记录数（**不看**排除开关）。
    hits: usize,
}

pub struct App {
    pub ports: Vec<PortEntry>,
    pub port_idx: usize,
    pub baud_idx: usize,
    pub bits: Bits,
    pub parity: Parity,
    pub stops: Stops,
    pub flow: Flow,
    pub view: View,
    pub interval_ms: u64,
    pub sel: usize,
    pub focus: Focus,
    pub tab: usize,

    pub link: Option<Link>,
    pub records: VecDeque<Record>,
    /// 正在累积的接收行（未遇到换行、还没到空闲时间），实时显示在末尾。
    pub pending_rx: Option<Record>,
    /// 当前行里最后一个换行的字节位置，增量维护，避免每次收数都全量扫描。
    pending_last_nl: Option<usize>,
    pub buffered: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,

    pub auto_scroll: bool,
    /// 暂停显示：数据照收不误，只是不再自动跟到最新一行，
    /// 方便停下来盯着某一段数据看。`End` / `a` 可随时恢复跟随。
    pub paused: bool,
    /// 关键字过滤：只显示包含该子串的记录，空串表示不过滤。
    pub filter: String,
    /// 过滤时忽略 ASCII 大小写（"error" 也能命中 "ERROR"）。
    ///
    /// 默认关：串口日志里大小写常常是有意义的（`ERROR` 与 `error` 可能
    /// 来自不同设备），要不要忽略由用户自己决定。
    pub filter_ci: bool,
    /// 反向过滤：**排除**命中关键字的记录。
    ///
    /// 「只要没出错的那些」这种需求很常见，反向过滤比先看全部再挑省事。
    pub filter_invert: bool,
    pub scroll_top: usize,
    pub view_h: usize,
    /// 数据区可滚动的上限（总行数 - 可视行数），由绘制时写入，
    /// 供按键判断「已经滚到底」。
    pub view_max_top: usize,
    pub log_scroll: usize,

    pub input: String,
    pub cursor: usize,
    pub hex_mode: bool,
    pub repeating: bool,
    pub last_sent: Vec<u8>,
    pub last_sent_at: Instant,

    /// 发送历史：最近的一条在**最前面**。
    ///
    /// 存的是用户输入的原文（不是解析后的字节），翻历史时看到的
    /// 应该和当时敲的一模一样。
    pub send_history: Vec<String>,
    /// 正在浏览发送历史的游标，`None` 表示没在浏览。
    ///
    /// 指向 `send_history` 的下标：`Some(0)` 是最新的一条，
    /// 下标越大越旧。
    pub history_idx: Option<usize>,
    /// 翻历史之前输入框里的原始内容。
    ///
    /// 往下翻到最后一条之后靠它把输入框恢复原样，
    /// 不然用户就再也回不到自己没发送的半句话了。
    history_draft: Option<String>,

    /// 掉线后自动重连。手动断开不会关掉它 —— 这正是这个开关的意义。
    pub auto_reconnect: bool,
    /// 上次尝试自动重连的时刻（用来按固定节奏重试）。
    reconnect_mark: Instant,
    /// 自动重连连续失败的次数（用来决定要不要记日志）。
    reconnect_fails: u32,

    /// 时间戳是否显示到毫秒。关掉后更紧凑，看数据更清爽。
    pub show_ms: bool,

    /// 长行自动折行。
    ///
    /// 关闭（默认）时保持既有行为：超宽的行被截断并标 `…`。
    /// 开启后超宽的行按屏宽折成多行，数据一行都不会丢。
    pub wrap: bool,

    /// 数据区当前**第一条可见记录**在过滤后序列里的序号。
    ///
    /// 由渲染层每帧写回：只有渲染层知道每条记录占了多少行，
    /// 也只有它知道屏幕上现在顶着的第一条是哪条。
    /// 详情弹窗默认打开这一条。
    pub top_record: usize,

    /// 过滤命中导航的请求：`Some(1)` = 下一个，`Some(-1)` = 上一个。
    ///
    /// 按键只负责设置它，实际跳转由渲染层在绘制时消费 ——
    /// 「下一条命中」要按行高算，只有渲染层有这个数。
    pub nav_dir: Option<i8>,

    /// 自动刷新端口列表。
    ///
    /// 开着时每 5 秒枚举一次，列表真的变了才记一条日志；
    /// 手动按 r 刷新不受它影响，照旧每次都记。
    pub auto_ports: bool,

    /// 上次枚举端口的时刻（手动刷新与自动刷新都会更新它）。
    ports_mark: Instant,

    /// 最近一个统计窗口内的接收 / 发送速率（字节/秒）。
    pub rx_rate: u64,
    pub tx_rate: u64,
    /// 速率统计窗口的起点。
    rate_mark: Instant,
    /// 本窗口内累计的收发字节数。
    rate_rx_acc: u64,
    rate_tx_acc: u64,

    /// 本次会话的开始时刻（用于显示已运行时长）。
    // 界面层还没接上，先按「暂时没人用」处理，免得并发构建时刷一堆告警。
    #[allow(dead_code)]
    pub started_at: Instant,

    pub recorder: Option<Recorder>,
    pub modal: Option<Modal>,
    pub toast: Option<(String, bool, Instant)>,
    pub should_quit: bool,
    /// 界面重绘上限（帧/秒）。弱链路终端上每一帧都要过网络，降档立竿见影。
    pub fps: u32,

    /// 数据版本号：任何会影响过滤结果的改动都 +1。
    ///
    /// 过滤结果要缓存下来给一帧里的几处地方共用（数据区、详情弹窗），
    /// 缓存靠这个版本号判断新不新：数据一变，版本号对不上，缓存立刻作废。
    data_epoch: u64,
    /// 过滤结果缓存（一帧只算一次）。
    filter_cache: RefCell<Option<FilterCache>>,
    /// 过滤扫描次数（只在测试里用）：一帧应该只扫一遍。
    #[cfg(test)]
    filter_scans: std::cell::Cell<u64>,
}

impl App {
    pub fn new() -> App {
        App {
            ports: Vec::new(),
            port_idx: 0,
            baud_idx: 4, // 115200
            bits: Bits::Eight,
            parity: Parity::None,
            stops: Stops::One,
            flow: Flow::None,
            view: View::Text,
            interval_ms: 1000,
            sel: 1,
            focus: Focus::Params,
            tab: 0,
            link: None,
            records: VecDeque::new(),
            pending_rx: None,
            pending_last_nl: None,
            buffered: 0,
            rx_bytes: 0,
            tx_bytes: 0,
            rx_packets: 0,
            auto_scroll: true,
            paused: false,
            filter: String::new(),
            filter_ci: false,
            filter_invert: false,
            scroll_top: 0,
            view_h: 1,
            view_max_top: 0,
            log_scroll: 0,
            input: String::new(),
            cursor: 0,
            hex_mode: false,
            repeating: false,
            last_sent: Vec::new(),
            last_sent_at: Instant::now(),
            send_history: Vec::new(),
            history_idx: None,
            history_draft: None,
            auto_reconnect: false,
            reconnect_mark: Instant::now(),
            reconnect_fails: 0,
            show_ms: true,
            wrap: false,
            top_record: 0,
            nav_dir: None,
            auto_ports: true,
            ports_mark: Instant::now(),
            rx_rate: 0,
            tx_rate: 0,
            rate_mark: Instant::now(),
            rate_rx_acc: 0,
            rate_tx_acc: 0,
            started_at: Instant::now(),
            recorder: None,
            modal: None,
            toast: None,
            should_quit: false,
            fps: DEFAULT_FPS,
            data_epoch: 0,
            filter_cache: RefCell::new(None),
            #[cfg(test)]
            filter_scans: std::cell::Cell::new(0),
        }
    }

    // ---------- 端口与连接 ----------

    pub fn refresh_ports(&mut self) {
        // 手动刷新也算一次枚举：自动刷新的计时点从这里起算，
        // 免得用户刚按完 r 就又来一次自动枚举。
        self.ports_mark = Instant::now();
        match port::list_ports() {
            Ok(list) => {
                self.ports = list;
                if self.port_idx >= self.ports.len() {
                    self.port_idx = self.ports.len().saturating_sub(1);
                }
                let n = self.ports.len();
                self.push_sys(format!("枚举到 {n} 个串口{}", if n == 0 { "（用 r 刷新可重试）" } else { "" }));
            }
            Err(e) => self.push_sys(format!("枚举串口失败：{e}")),
        }
    }

    /// 自动刷新端口列表：距上次枚举超过 5 秒才枚举一次。
    ///
    /// 与手动刷新最大的区别是**不刷日志**：每 5 秒往事件日志里塞一条
    /// 「枚举到 N 个串口」，跑上几小时日志里就只剩它了。
    /// 只有列表真的变了才记一条，说明「设备插上了 / 拔掉了」。
    fn auto_refresh_ports(&mut self) {
        if !self.auto_ports {
            return;
        }
        if self.ports_mark.elapsed() < AUTO_PORTS_INTERVAL {
            return;
        }
        self.ports_mark = Instant::now();
        // 枚举失败不记日志：手动刷新时已经报过一次，
        // 每 5 秒重复报同一个错只会把日志刷满。
        let Ok(list) = port::list_ports() else { return };
        let changed = ports_differ(&self.ports, &list);
        self.ports = list;
        if self.port_idx >= self.ports.len() {
            self.port_idx = self.ports.len().saturating_sub(1);
        }
        if changed {
            let n = self.ports.len();
            self.push_sys(format!("串口列表有变化（{n} 个串口）"));
        }
    }

    pub fn cfg(&self) -> PortCfg {
        PortCfg {
            port: self
                .ports
                .get(self.port_idx)
                .map(|p| p.port.clone())
                .unwrap_or_else(|| "COM1".to_string()),
            baud: BAUDS[self.baud_idx],
            bits: self.bits,
            parity: self.parity,
            stops: self.stops,
            flow: self.flow,
        }
    }

    /// 当前左栏参数是否与已连接的串口不一致。
    pub fn settings_drift(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.cfg != self.cfg())
    }

    pub fn connected(&self) -> bool {
        self.link.is_some()
    }

    pub fn toggle_link(&mut self) {
        if self.link.is_some() {
            self.close_link("手动断开");
        } else {
            self.open_link();
        }
    }

    pub fn open_link(&mut self) {
        if self.link.is_some() {
            return;
        }
        if self.ports.is_empty() {
            self.toast("没有可用串口，按 r 刷新列表", true);
            return;
        }
        let cfg = self.cfg();
        match Link::open(cfg.clone()) {
            Ok(link) => {
                self.link = Some(link);
                self.push_sys(format!("已打开 {} · {}", cfg.port, cfg.summary()));
                self.toast(format!("已连接 {}", cfg.port), false);
            }
            Err(e) => {
                self.push_sys(format!("打开 {} 失败：{e}", cfg.port));
                self.toast(format!("打开失败：{e}"), true);
            }
        }
    }

    pub fn close_link(&mut self, reason: &str) {
        // 断开后停掉循环发送：否则重连到另一台设备时，
        // 上次的命令会在 100 ms 内被静默自动发出去。
        // 放在 if 外面，避免"没连上却开着循环"这种状态漏网。
        if self.repeating {
            self.repeating = false;
            self.push_sys("连接已断开，循环发送自动关闭");
        }
        if let Some(link) = self.link.take() {
            let name = link.cfg.port.clone();
            drop(link); // 触发 close()：停线程 + 释放句柄
            self.push_sys(format!("已关闭 {name}（{reason}）"));
            // 手动断开不关掉自动重连：这正是这个开关存在的意义。
            // 但必须让用户知道「它还开着」，否则会以为断开就是彻底断开了。
            self.note_auto_reconnect();
        }
    }

    /// 断开之后提醒用户自动重连还开着。
    fn note_auto_reconnect(&mut self) {
        if self.auto_reconnect {
            self.push_sys(format!(
                "已断开，自动重连仍开启（{} 秒后重试）",
                RECONNECT_INTERVAL.as_secs()
            ));
        }
    }

    pub fn select_port(&mut self, delta: isize) {
        if self.ports.is_empty() {
            return;
        }
        let n = self.ports.len() as isize;
        self.port_idx = (((self.port_idx as isize + delta) % n + n) % n) as usize;
    }

    // ---------- 数据 ----------

    /// 提交一条已封口的记录（进缓冲区、写日志文件）。
    fn commit(&mut self, rec: Record) {
        if rec.data.is_empty() {
            return;
        }
        let mut rec = rec;
        // 单条就超过上限时（固件 dump、整包 bin 传输），保留尾部最新的一段，
        // 而不是让它把整个缓冲区连同历史记录一起挤没。
        if rec.data.len() as u64 > MAX_BYTES {
            let keep = rec.data.split_off(rec.data.len() - MAX_BYTES as usize);
            rec.data = keep;
            // 截掉的是头部，换行计数要按保留的尾部重算（只在这一条超上限时发生）
            rec.nl = fmt::count_nl(&rec.data);
        }
        // 数据可能刚被截尾：显示相关的缓存要作废。
        rec.invalidate();
        if let Some(r) = self.recorder.as_mut() {
            r.write(&rec);
        }
        self.buffered += rec.data.len() as u64;
        self.records.push_back(rec);
        // 至少留一条：正常情况下按上限从最旧开始淘汰，
        // 但不能让淘汰把队列清空（否则界面会莫名其妙变空白）。
        while (self.buffered > MAX_BYTES || self.records.len() > MAX_RECORDS)
            && self.records.len() > 1
        {
            match self.records.pop_front() {
                Some(r) => self.buffered -= r.data.len() as u64,
                None => break,
            }
        }
        // 记录变了：过滤缓存作废（否则会按旧内容判命中）。
        self.bump_data();
    }

    /// 把一条正在累积的接收行按换行切开：
    /// 换行符之前的内容封口进缓冲区，剩余部分继续作为「当前行」等待后续字节。
    ///
    /// 换行位置由 `pending_last_nl` 增量维护，不在这里重扫整条数据 ——
    /// 只用 `\r` 的设备（进度条刷新）会让当前行一直很长，每次都全量扫描
    /// 就是 O(n²)，几十秒就能把收数线程拖垮。
    fn seal_rx_line(&mut self) {
        let Some(idx) = self.pending_last_nl.take() else { return };
        let Some(mut rec) = self.pending_rx.take() else { return };
        let tail = rec.data.split_off(idx + 1);
        self.commit(rec);
        if !tail.is_empty() {
            self.pending_rx = Some(Record::rx_at(SystemTime::now(), tail, Instant::now()));
        }
    }

    /// 空闲封行：这么久没收到新字节，就把当前行收尾，下次数据另起一行。
    fn idle_seal(&mut self) {
        if self
            .pending_rx
            .as_ref()
            .is_some_and(|r| r.last_rx.elapsed() > RX_IDLE_SEAL)
        {
            if let Some(rec) = self.pending_rx.take() {
                self.commit(rec);
            }
        }
    }

    /// 接收到的原始字节：并入当前行，再按换行封口。
    pub fn feed_rx(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // 数据变了：过滤缓存作废（正在累积的这一行内容变了）。
        self.bump_data();
        self.rx_bytes += data.len() as u64;
        self.rx_packets += 1;
        // 速率统计只累加原始字节数，解析、分行都不影响它。
        self.rate_rx_acc += data.len() as u64;
        let now = Instant::now();
        let base = self.pending_rx.as_ref().map(|r| r.data.len()).unwrap_or(0);
        match self.pending_rx.as_mut() {
            Some(rec) => {
                rec.data.extend_from_slice(data);
                // 缓存的换行计数跟着追加做增量累计，末尾是否换行留给 text_lines 判断
                rec.nl += fmt::count_nl(data);
                // 数据变了：显示相关的缓存要作废（否则过滤会按旧内容判命中，
                // 正在累积的这一行就再也搜不到新到的字节）
                rec.invalidate();
                rec.last_rx = now;
            }
            None => {
                self.pending_rx = Some(Record::rx_at(SystemTime::now(), data.to_vec(), now));
            }
        }
        // 增量维护「最后一个换行的位置」：只看新到的这段，
        // 没有换行时旧位置不变（数据是往后追加的）。
        if let Some(p) = data.iter().rposition(|&b| b == b'\n') {
            self.pending_last_nl = Some(base + p);
        }
        self.seal_rx_line();
        // 设备只发 `\r`（进度条刷新）时永远等不到换行与空闲阈值，
        // 当前行会无限膨胀，而每帧都要重新解码它，界面随之卡死。
        // 超过阈值就先封口，让它进入有上限的缓冲淘汰流程。
        if self.pending_rx.as_ref().is_some_and(|r| r.data.len() >= MAX_PENDING) {
            self.force_seal_rx();
        }
    }

    /// 不等换行也不等空闲，直接把当前行封口。
    fn force_seal_rx(&mut self) {
        self.pending_last_nl = None;
        if let Some(rec) = self.pending_rx.take() {
            self.commit(rec);
        }
    }

    /// 发送：一次写入就是一条完整记录，立即封口。
    pub fn commit_tx(&mut self, data: &[u8]) {
        self.commit(Record::new(Dir::Tx, data));
    }

    pub fn push_sys(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.commit(Record::new(Dir::Sys, msg.as_bytes()));
    }

    /// 界面上要画的所有记录：已封口的 + 当前正在累积的行（RX/TX 各一条）。
    pub fn stream(&self) -> impl Iterator<Item = &Record> {
        self.records
            .iter()
            .chain(self.pending_rx.iter())
    }

    /// 每次调用都实打实扫一遍的**未缓存**过滤路径。
    ///
    /// 界面层用的是缓存版（[`Self::filtered_records`] / [`Self::filtered_count`] /
    /// [`Self::filtered_record`]）—— 一帧只扫一遍。
    /// 这里保留一条不缓存的路径，是给性能测试量「一帧扫一遍」的代价用的：
    /// 走缓存的话第二次调用就命中缓存，测出来的时间是假的。
    #[allow(dead_code)] // 只有性能测试在用
    pub fn stream_filtered(&self) -> impl Iterator<Item = &Record> {
        self.stream().filter(move |r| self.match_filter(r))
    }

    /// 过滤后**显示**出来的记录（按流里的顺序）。
    ///
    /// 用的是缓存：过滤是每帧都要做的重活，而一帧里它会被问好几次
    /// （数据区要一份列表，详情弹窗又要一条记录与一个总数）。
    pub fn filtered_records(&self) -> Vec<&Record> {
        self.ensure_filter_cache();
        let cache = self.filter_cache.borrow();
        let cache = cache.as_ref().expect("过滤缓存刚算过");
        cache
            .shown
            .iter()
            .filter_map(|&p| self.record_at(p))
            .collect()
    }

    /// 过滤后序列里的第 `idx` 条记录。
    ///
    /// 详情弹窗与界面层都靠它取记录：序号是「过滤后」的，
    /// 过滤一生效，界面上看到的第几条就是这里的第几条。
    pub fn filtered_record(&self, idx: usize) -> Option<&Record> {
        self.ensure_filter_cache();
        let pos = {
            let cache = self.filter_cache.borrow();
            cache.as_ref().and_then(|c| c.shown.get(idx).copied())
        };
        pos.and_then(|p| self.record_at(p))
    }

    /// 过滤后的记录数（过滤为空时就是全部记录）。
    pub fn filtered_count(&self) -> usize {
        self.ensure_filter_cache();
        let cache = self.filter_cache.borrow();
        cache.as_ref().map_or(0, |c| c.shown.len())
    }

    /// 命中关键字的记录数（**不看**排除开关）。
    ///
    /// 排除模式下「命中」与「显示」正好相反，两个数要分开报：
    /// 说「命中 N 条」而 N 指的是没命中的那些，用户会以为程序算错了。
    pub fn filter_hit_count(&self) -> usize {
        self.ensure_filter_cache();
        let cache = self.filter_cache.borrow();
        cache.as_ref().map_or(0, |c| c.hits)
    }

    /// 过滤前的记录总数（不管过滤）。
    pub fn total_record_count(&self) -> usize {
        self.records.len() + usize::from(self.pending_rx.is_some())
    }

    /// 过滤扫描次数（只在测试里用）：一帧应该只扫一遍。
    #[cfg(test)]
    pub fn filter_scan_count(&self) -> u64 {
        self.filter_scans.get()
    }

    /// 流里的第 `pos` 条记录。
    ///
    /// 位置 `0..records.len()` 是已封口的记录；位置 `records.len()` 是
    /// 正在累积的那条（没有累积行时这个位置不存在）。
    fn record_at(&self, pos: usize) -> Option<&Record> {
        if pos < self.records.len() {
            self.records.get(pos)
        } else if pos == self.records.len() {
            self.pending_rx.as_ref()
        } else {
            None
        }
    }

    /// 数据变了：作废过滤缓存。
    fn bump_data(&mut self) {
        self.data_epoch = self.data_epoch.wrapping_add(1);
    }

    /// 过滤缓存是不是还新：新不新看数据与过滤条件这两边的状态。
    ///
    /// 两边都比一遍，而不是只靠版本号：版本号要靠人记得在每个改动点 +1，
    /// 漏一处就是「数据变了还拿旧结果」。这里把**状态本身**也存进缓存，
    /// 谁直接改了公开字段（`filter_ci` 之类）也一样能被发现。
    fn filter_cache_is_fresh(&self) -> bool {
        self.filter_cache.borrow().as_ref().is_some_and(|c| {
            c.data == self.data_epoch
                && c.len == self.records.len()
                && c.pending == self.pending_rx.as_ref().map(|r| r.data.len())
                && c.ci == self.filter_ci
                && c.invert == self.filter_invert
                && c.keyword == self.filter
        })
    }

    /// 过滤缓存新不新：新就留着，旧了就重算一遍。
    ///
    /// 有效性由数据与过滤条件的状态决定：数据变了（追加 / 封口 / 清空）或
    /// 过滤条件变了（关键字 / 两个开关），对不上就重算 ——
    /// 绝不会出现「数据变了还拿旧结果」。
    fn ensure_filter_cache(&self) {
        if self.filter_cache_is_fresh() {
            return;
        }
        let (shown, hits) = self.compute_filter();
        *self.filter_cache.borrow_mut() = Some(FilterCache {
            data: self.data_epoch,
            len: self.records.len(),
            pending: self.pending_rx.as_ref().map(|r| r.data.len()),
            keyword: self.filter.clone(),
            ci: self.filter_ci,
            invert: self.filter_invert,
            shown,
            hits,
        });
    }

    /// 实打实扫一遍：哪些记录显示、命中关键字的有几条。
    fn compute_filter(&self) -> (Vec<usize>, usize) {
        #[cfg(test)]
        self.filter_scans.set(self.filter_scans.get() + 1);
        let mut shown = Vec::new();
        for (pos, rec) in self.stream().enumerate() {
            if self.match_filter(rec) {
                shown.push(pos);
            }
        }
        // 命中关键字的条数：正向模式就是显示出来的那些；
        // 排除模式显示的是**没命中**的，所以命中数 = 总数 - 显示数。
        let total = self.total_record_count();
        let hits = if self.filter.is_empty() {
            total
        } else if self.filter_invert {
            total - shown.len()
        } else {
            shown.len()
        };
        (shown, hits)
    }

    /// 这条记录是否命中当前过滤条件（含排除开关）。
    pub fn match_filter(&self, rec: &Record) -> bool {
        if self.filter.is_empty() {
            // 过滤为空就是不过滤：两个开关都不该改变这一点
            return true;
        }
        let hit = self.match_keyword(rec);
        // 反向过滤：命中的**不显示**（排除模式）
        if self.filter_invert {
            !hit
        } else {
            hit
        }
    }

    /// 这条记录是否**命中关键字**（不看排除开关）。
    fn match_keyword(&self, rec: &Record) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        // 判据用**显示文本**，与高亮用的是同一份表示：
        // 控制符显示成 `^X`、非法字节显示成 `·`、转义序列折叠成 `^[m`，
        // 这些在原始字节里根本不是关键字本身。按原始字节匹配会出现
        // 「命中 N 条却找不到高亮」。
        //
        // 按**逻辑行**判命中（而不是把整条记录拼成一段再判）：
        // 高亮是逐逻辑行算的，判据必须同口径 ——
        // 否则关键字横跨两行时会出现「算命中却一个格子都没高亮」。
        rec.text_view().lines.iter().any(|l| {
            if self.filter_ci {
                // 忽略大小写：逐字节比较，不给整段文本做小写化
                // （一帧要扫几千条记录，复制文本的代价立刻就出来了）
                fmt::contains_ignore_ascii_case(l, &self.filter)
            } else {
                l.contains(&self.filter)
            }
        })
    }

    /// 切换「过滤忽略大小写」。
    pub fn toggle_filter_ci(&mut self) {
        self.filter_ci = !self.filter_ci;
        self.toast(
            if self.filter_ci {
                "过滤已改为忽略大小写"
            } else {
                "过滤恢复区分大小写"
            },
            false,
        );
        // 命中集合变了，滚动位置没有意义（与换关键字时一样）
        self.reset_scroll();
    }

    /// 切换「反向过滤」（排除命中关键字的记录）。
    pub fn toggle_filter_invert(&mut self) {
        self.filter_invert = !self.filter_invert;
        self.toast(
            if self.filter_invert {
                "反向过滤已开启：只显示未命中的记录"
            } else {
                "反向过滤已关闭"
            },
            false,
        );
        self.reset_scroll();
    }

    /// 过滤开关的显示标记：忽略大小写是 `i`，反向（排除）是 `!`，都没有时是空串。
    ///
    /// 界面层把它拼到过滤标题后面，用户才看得见「当前这个过滤是反着的」。
    #[allow(dead_code)] // 界面层接上之前没人读
    pub fn filter_flags(&self) -> String {
        let mut s = String::new();
        if self.filter_ci {
            s.push('i');
        }
        if self.filter_invert {
            s.push('!');
        }
        s
    }

    /// 滚动位置复位：内容换了，之前那个行号就没有意义了。
    fn reset_scroll(&mut self) {
        self.scroll_top = 0;
        self.log_scroll = 0;
    }

    pub fn clear_records(&mut self) {
        self.records.clear();
        self.pending_rx = None;
        self.pending_last_nl = None;
        self.buffered = 0;
        self.reset_scroll();
        // 数据变了：过滤缓存作废。
        self.bump_data();
    }

    /// 只清接收区：删掉所有收到的记录，发送记录与事件日志原样保留。
    ///
    /// 调对端时经常需要「保留我发过什么，只把收到的清干净」，
    /// `clear_records` 会把发送记录一起抹掉，事后没法对照。
    /// 累计统计（状态栏的 RX 字节 / 包数）不清零，它记的是本次连接的总量。
    pub fn clear_rx(&mut self) -> usize {
        let before = self.records.len();
        self.records.retain(|r| r.dir != Dir::Rx);
        let removed = before - self.records.len();
        // 正在累积的那一行也是接收内容，一并丢掉，否则清完立刻又冒出一行
        if self.pending_rx.take().is_some() {
            self.pending_last_nl = None;
        }
        // 被删掉的不再占用缓冲预算，这里按剩下的重算（只在用户主动触发时做）
        self.buffered = self.records.iter().map(|r| r.data.len() as u64).sum();
        self.reset_scroll();
        self.auto_scroll = true;
        // 数据变了：过滤缓存作废。
        self.bump_data();
        removed
    }

    /// 界面可见状态的指纹：前后两次相同，说明这一帧画出来也一样，可以不重绘。
    ///
    /// 弱链路终端（SSH / 容器 / 网页终端）上卡顿的主因之一是「无意义的整屏刷新」——
    /// 数据没动也照发一遍转义序列。把所有会被画出来的状态混进一个 u64，
    /// 主循环只在指纹变化时才 draw。
    ///
    /// 时间只混到「秒」：状态栏右下角有时钟，混到毫秒会让空闲时也一直变，
    /// 脏标记就完全失效了。数据在跑的时候靠 rx_bytes 变化驱动，照样是满帧。
    pub fn ui_signature(&self) -> u64 {
        let parts: [u64; 45] = [
            self.rx_bytes,
            self.rx_packets,
            self.tx_bytes,
            self.records.len() as u64,
            self.pending_rx.as_ref().map_or(0, |r| r.data.len() as u64),
            self.buffered,
            self.ports.len() as u64,
            self.port_idx as u64,
            self.baud_idx as u64,
            self.bits as u64,
            self.parity as u64,
            self.stops as u64,
            self.flow as u64,
            self.view as u64,
            self.interval_ms,
            self.sel as u64,
            self.focus as u64,
            self.tab as u64,
            self.link.is_some() as u64,
            self.scroll_top as u64,
            self.auto_scroll as u64,
            self.paused as u64,
            str_hash(&self.filter),
            self.log_scroll as u64,
            self.input.len() as u64,
            self.cursor as u64,
            self.hex_mode as u64,
            self.repeating as u64,
            self.recorder.is_some() as u64,
            self.modal.is_some() as u64,
            self.toast.as_ref().map_or(0, |t| str_hash(&t.0)),
            self.fps as u64,
            // 新增的可见状态：漏掉任何一项，界面上就会停在旧的样子
            self.auto_reconnect as u64,
            self.show_ms as u64,
            self.send_history.len() as u64,
            // `None` 用 u64::MAX 表示，和 `Some(0)` 区分得开
            self.history_idx.map_or(u64::MAX, |i| i as u64),
            self.rx_rate,
            self.tx_rate,
            // 新增的可见状态：漏掉任何一项，界面上就会停在旧的样子
            self.wrap as u64,
            self.auto_ports as u64,
            // `None` 用 u64::MAX 表示，和 `Some(0)` 区分得开。
            // 方向先转成 u8 再转 u64：直接 `i8 as u64` 会把 -1 变成 u64::MAX，
            // 跟「没有导航请求」撞在一起。
            self.nav_dir.map_or(u64::MAX, |d| d as u8 as u64),
            self.top_record as u64,
            // 弹窗开着时 `modal.is_some()` 一直不变，
            // 但弹窗里翻记录、切 HEX 都会改变画面，得单独混进去
            self.modal_content_key(),
            // 过滤的两个开关会改变「哪些记录显示出来」，
            // 漏掉任何一个，界面都会停在旧的过滤结果上
            self.filter_ci as u64,
            self.filter_invert as u64,
        ];
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for v in parts {
            h ^= v;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
            h ^= h >> 29;
        }
        // 秒级时钟，保证空闲时最多 1 帧/秒（而不是毫秒级一直重绘）
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        h ^ secs.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }

    /// 弹窗内容的指纹：脏标记用。
    ///
    /// 弹窗开着的时候 `modal.is_some()` 一直是不变，
    /// 但详情弹窗里翻记录、切 HEX 都在改画面 —— 只看「有没有弹窗」
    /// 会让界面停在旧的那一条上。
    fn modal_content_key(&self) -> u64 {
        match &self.modal {
            Some(Modal::Detail { idx, hex }) => (*idx as u64) << 1 | u64::from(*hex),
            _ => 0,
        }
    }

    /// `Shift+F`：在几档刷新率之间循环，弱链路终端上用来快速降档。
    pub fn cycle_fps(&mut self) {
        let i = FPS_STEPS.iter().position(|&f| f == self.fps).unwrap_or(0);
        self.fps = FPS_STEPS[(i + 1) % FPS_STEPS.len()];
        self.toast(format!("界面刷新上限 {} 帧/秒", self.fps), false);
    }

    /// 非阻塞收取读线程数据；连接已结束时清理状态。
    pub fn drain_serial(&mut self) {
        let Some(rx) = self.link.as_ref().map(|l| Arc::clone(&l.rx)) else {
            return;
        };
        let mut dead = false;
        loop {
            let msg = {
                let Ok(guard) = rx.lock() else { break };
                match guard.try_recv() {
                    Ok(m) => m,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        dead = true;
                        break;
                    }
                }
            };
            match msg {
                RxMsg::Data(d) => {
                    // 字节数与包数统一在 feed_rx 里累加，这里不要重复计数，
                    // 否则状态栏 / 日志页显示的接收量会是真实值的 2 倍。
                    self.feed_rx(&d);
                }
                RxMsg::Err(e) => {
                    self.push_sys(format!("读取出错：{e}"));
                    dead = true;
                }
                RxMsg::Closed => dead = true,
            }
        }
        if dead && self.link.is_some() {
            let name = self.link.as_ref().map(|l| l.cfg.port.clone()).unwrap_or_default();
            self.close_link(&format!("{name} 连接结束"));
            self.toast("串口已断开", true);
        }
    }

    pub fn tick(&mut self) {
        if let Some(r) = self.recorder.as_mut() {
            r.tick();
            // 记录写失败要能被用户看到：界面还亮着 REC 灯，日志却是空的
            if let Some(err) = r.take_errors() {
                self.toast(format!("日志写入失败：{err}"), true);
            }
        }
        // 空闲封行：设备不换行时，停顿一下就收尾，避免整段数据挤在一行
        self.idle_seal();
        if self.repeating
            && !self.last_sent.is_empty()
            && self.last_sent_at.elapsed() >= Duration::from_millis(self.interval_ms)
        {
            self.write_out(&self.last_sent.clone(), true);
        }
        // 速率统计：窗口满 1 秒才结算一次，避免每帧都抖。
        self.update_rate();
        // 自动重连：没连着的时候按固定节奏重试。
        self.try_reconnect();
        // 自动刷新端口列表：插上 / 拔掉设备时不用手动按 r。
        self.auto_refresh_ports();
        // toast 3 秒后自动消失
        if let Some((_, _, at)) = &self.toast {
            if at.elapsed() > Duration::from_secs(3) {
                self.toast = None;
            }
        }
    }

    /// 结算速率：窗口满 1 秒就用这一秒的累计字节数算出平均速度。
    fn update_rate(&mut self) {
        let elapsed = self.rate_mark.elapsed();
        if elapsed < RATE_WINDOW {
            return;
        }
        let secs = elapsed.as_secs_f64();
        self.rx_rate = (self.rate_rx_acc as f64 / secs).round() as u64;
        self.tx_rate = (self.rate_tx_acc as f64 / secs).round() as u64;
        self.rate_rx_acc = 0;
        self.rate_tx_acc = 0;
        self.rate_mark = Instant::now();
    }

    /// 自动重连：开着、且当前没有连接时，按固定节奏试着把端口重新打开。
    ///
    /// 失败不刷屏 —— 连着失败 5 次才记一条日志，否则设备没插好时
    /// 日志会被每 2 秒一条的失败记录冲掉。
    fn try_reconnect(&mut self) {
        if !self.auto_reconnect || self.link.is_some() {
            return;
        }
        if self.reconnect_mark.elapsed() < RECONNECT_INTERVAL {
            return;
        }
        self.reconnect_mark = Instant::now();
        // 端口列表为空说明还没枚举到设备，不知道该开哪个口，瞎试只会报错。
        if self.ports.is_empty() {
            return;
        }
        let cfg = self.cfg();
        match Link::open(cfg.clone()) {
            Ok(link) => {
                self.link = Some(link);
                self.reconnect_fails = 0;
                self.push_sys(format!("自动重连成功 {} · {}", cfg.port, cfg.summary()));
                self.toast(format!("自动重连成功 {}", cfg.port), false);
            }
            Err(e) => {
                self.reconnect_fails += 1;
                if self.reconnect_fails % RECONNECT_LOG_EVERY == 0 {
                    self.push_sys(format!(
                        "自动重连失败第 {} 次：{e}",
                        self.reconnect_fails
                    ));
                }
            }
        }
    }

    pub fn toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, Instant::now()));
    }

    pub fn toggle_recorder(&mut self) {
        if self.recorder.is_some() {
            if let Some(mut r) = self.recorder.take() {
                let _ = r.out.flush();
                let p = r.path.display().to_string();
                self.push_sys(format!("已停止记录 → {p}"));
                self.toast("已停止记录", false);
            }
        } else {
            match Recorder::start() {
                Ok(r) => {
                    let p = r.path.display().to_string();
                    self.recorder = Some(r);
                    self.push_sys(format!("开始记录 → {p}"));
                    self.toast(format!("记录中 → {p}"), false);
                }
                Err(e) => {
                    self.push_sys(format!("无法创建日志文件：{e}"));
                    self.toast(format!("记录失败：{e}"), true);
                }
            }
        }
    }

    // ---------- 发送 ----------

    pub fn send_input(&mut self) {
        let text = self.input.clone();
        let hex = self.hex_mode;
        match fmt::parse_send(&text, hex) {
            Ok(data) => {
                if data.is_empty() {
                    self.toast("发送内容为空", true);
                    return;
                }
                // 只有真的把字节写出去了才记历史：没连上或写失败时，
                // 往历史里塞一条等于骗用户「这条发过了」。
                if self.write_out(&data, false) {
                    // 存的是用户输入的原文，不是解析后的字节：
                    // 翻历史时看到的应该和当时敲的一模一样。
                    self.push_history(&text);
                }
            }
            Err(e) => self.toast(e, true),
        }
    }

    pub fn send_file(&mut self, path: &str) {
        match std::fs::read(path) {
            Ok(data) if data.is_empty() => {
                self.push_sys(format!("文件为空：{path}"));
                self.toast("文件为空", true);
            }
            Ok(data) => {
                self.push_sys(format!("发送文件 {path}（{} 字节）", data.len()));
                self.write_out(&data, false);
            }
            Err(e) => {
                self.push_sys(format!("读取文件失败 {path}：{e}"));
                self.toast(format!("读取失败：{e}"), true);
            }
        }
    }

    /// 真正落盘发送。`repeat` 为循环重发，不刷新提示条。
    ///
    /// 返回是否真的把字节写出去了：调用方要据此决定「这次发送算不算数」
    /// （比如要不要记进发送历史）。
    pub fn write_out(&mut self, data: &[u8], repeat: bool) -> bool {
        if self.link.is_none() {
            if !repeat {
                self.toast("串口未连接，按 o 打开", true);
            }
            return false;
        }
        let res = self.link.as_mut().unwrap().write(data);
        match res {
            Ok(_) => {
                self.tx_bytes += data.len() as u64;
                self.rate_tx_acc += data.len() as u64;
                self.last_sent = data.to_vec();
                self.last_sent_at = Instant::now();
                self.commit_tx(data);
                true
            }
            Err(e) => {
                let msg = format!("发送失败：{e}");
                self.push_sys(msg.clone());
                self.toast(msg, true);
                self.close_link("发送出错");
                false
            }
        }
    }

    pub fn parsed_len(&self) -> Result<usize, String> {
        fmt::parse_send(&self.input, self.hex_mode).map(|v| v.len())
    }

    // ---------- 发送历史 ----------

    /// 把一条发送内容存进历史。
    ///
    /// 只收非空内容；与上一条（最近一条）内容相同则不入队 ——
    /// 连按两下发送键不应该把历史刷成一片重复。
    pub fn push_history(&mut self, payload: &str) {
        if payload.is_empty() {
            return;
        }
        if self.send_history.first().is_some_and(|s| s == payload) {
            return;
        }
        // 最近的在最前面，插到头即可；超出上限时丢掉最旧的一条（在尾部）。
        self.send_history.insert(0, payload.to_string());
        if self.send_history.len() > MAX_HISTORY {
            self.send_history.pop();
        }
        // 新内容进来后，之前的浏览位置就没意义了，重新开始翻。
        self.history_idx = None;
        self.history_draft = None;
    }

    /// 往更旧的一条翻：把输入框内容换成上一条历史。
    ///
    /// 返回输入框内容是否真的变了（没变说明已经在最顶，或者内容重复）。
    /// 第一次翻会把当前输入框内容存成草稿，之后一路翻到底都能恢复。
    pub fn history_prev(&mut self) -> bool {
        if self.send_history.is_empty() {
            return false;
        }
        let idx = match self.history_idx {
            Some(i) => {
                // 下标越大越旧，到头了就停住（不能越界）。
                if i + 1 >= self.send_history.len() {
                    return false;
                }
                i + 1
            }
            None => {
                // 第一次翻：先把手上的内容存成草稿。
                self.history_draft = Some(self.input.clone());
                0
            }
        };
        self.history_idx = Some(idx);
        self.set_input_from_history(self.send_history[idx].clone())
    }

    /// 往更新的一条翻；翻过最新的一条之后恢复最初那份草稿。
    ///
    /// 返回输入框内容是否真的变了。
    pub fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_idx else { return false };
        if idx == 0 {
            // 已经是最新的一条，再往下就回到开始翻之前的样子。
            let draft = self.history_draft.take().unwrap_or_default();
            self.history_idx = None;
            return self.set_input_from_history(draft);
        }
        let next = idx - 1;
        self.history_idx = Some(next);
        self.set_input_from_history(self.send_history[next].clone())
    }

    /// 把历史内容写进输入框，并报告内容是否真的变了。
    fn set_input_from_history(&mut self, text: String) -> bool {
        if text == self.input {
            return false;
        }
        self.input = text;
        // 光标落在末尾：翻出来的命令通常要接着改，光标在末尾最顺手。
        self.cursor = self.input.chars().count();
        true
    }

    // ---------- 开关与统计 ----------

    /// 切换自动重连。
    pub fn toggle_auto_reconnect(&mut self) {
        self.auto_reconnect = !self.auto_reconnect;
        self.toast(
            if self.auto_reconnect {
                "自动重连已开启（掉线后每 2 秒重试）"
            } else {
                "自动重连已关闭"
            },
            false,
        );
        if self.auto_reconnect {
            // 立刻开始计时：让下一次 tick 就能试一次，而不是等满一个周期。
            // 计时点往前拨一个周期，`try_reconnect` 的间隔判断就会直接放行。
            self.reconnect_mark = Instant::now()
                .checked_sub(RECONNECT_INTERVAL)
                .unwrap_or_else(Instant::now);
            self.reconnect_fails = 0;
        }
    }

    /// 切换时间戳毫秒显示。
    pub fn toggle_ms(&mut self) {
        self.show_ms = !self.show_ms;
        self.toast(
            if self.show_ms {
                "时间戳显示到毫秒"
            } else {
                "时间戳只显示到秒"
            },
            false,
        );
    }

    /// 切换长行自动折行。
    ///
    /// 关着时超宽的行被截断并标 `…`（既有行为，原始数据不丢但看不到）；
    /// 开着时超宽的行按屏宽折成多行，一行数据完整显示出来。
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.toast(
            if self.wrap {
                "长行自动折行已开启"
            } else {
                "长行自动折行已关闭（超宽截断）"
            },
            false,
        );
    }

    /// 接收速率的短文本，如 "1.2 kB/s"。
    #[allow(dead_code)]
    pub fn rate_text(&self) -> String {
        Self::format_rate(self.rx_rate)
    }

    /// 发送速率的短文本，如 "12 B/s"。
    #[allow(dead_code)]
    pub fn tx_rate_text(&self) -> String {
        Self::format_rate(self.tx_rate)
    }

    /// 字节速率的显示格式：够短，能塞进状态栏。
    #[allow(dead_code)]
    fn format_rate(v: u64) -> String {
        const KB: f64 = 1024.0;
        let f = v as f64;
        if f < KB {
            format!("{v} B/s")
        } else if f < KB * KB {
            format!("{:.1} kB/s", f / KB)
        } else {
            format!("{:.1} MB/s", f / (KB * KB))
        }
    }

    /// 本次会话已经运行的时长。
    #[allow(dead_code)]
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    // ---------- 按键 ----------

    /// 焦点在终端页的发送输入框里（此时可打印字符是数据，不是命令）。
    fn in_input(&self) -> bool {
        self.focus == Focus::Input && self.tab == 0
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if let Some(m) = self.modal.take() {
            self.modal_key(m, k);
            return;
        }

        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        // 焦点切换优先于一切：即使在输入框里，Tab 也必须能用
        if k.code == KeyCode::Tab || k.code == KeyCode::BackTab {
            if ctrl {
                self.focus = Focus::Stream; // Ctrl+Tab 直接跳到数据区
            } else {
                self.cycle_focus();
            }
            return;
        }

        if ctrl {
            match k.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
                KeyCode::Char('u') | KeyCode::Char('U') => {
                    self.input.clear();
                    self.cursor = 0;
                }
                KeyCode::Char('o') | KeyCode::Char('O') => {
                    if self.connected() {
                        self.modal = Some(Modal::FilePath(String::new()));
                    } else {
                        self.toast("串口未连接，无法发送文件", true);
                    }
                }
                KeyCode::Char('h') | KeyCode::Char('H') if self.in_input() => {
                    self.toggle_hex_mode();
                }
                KeyCode::Char('l') | KeyCode::Char('L') if self.in_input() => {
                    self.repeating = !self.repeating;
                    self.toast(
                        if self.repeating {
                            format!("循环发送已开启（{} ms）", self.interval_ms)
                        } else {
                            "循环发送已关闭".to_string()
                        },
                        false,
                    );
                }
                // 自动重连 / 毫秒时间戳：只在非输入框焦点下生效，
                // 免得和输入框里的命令抢键。
                KeyCode::Char('r') | KeyCode::Char('R') if !self.in_input() => {
                    self.toggle_auto_reconnect();
                }
                KeyCode::Char('t') | KeyCode::Char('T') if !self.in_input() => self.toggle_ms(),
                // 输入框里 Ctrl+Up / Ctrl+Down 翻发送历史。
                // 必须排在下面 Ctrl+Up/Ctrl+Down 调循环间隔的分支前面。
                KeyCode::Up if self.in_input() => {
                    self.history_prev();
                }
                KeyCode::Down if self.in_input() => {
                    self.history_next();
                }
                // Ctrl+End / Ctrl+A：任何焦点下都能一键恢复自动跟随
                KeyCode::End | KeyCode::Char('a') | KeyCode::Char('A') => self.follow_tail(),
                KeyCode::Char('[') | KeyCode::Up => self.bump_interval(-50),
                KeyCode::Char(']') | KeyCode::Down => self.bump_interval(50),
                _ => {}
            }
            return;
        }

        if k.code == KeyCode::Esc {
            if self.in_input() {
                self.write_out(&[0x1B], false); // 发送 ESC 字节
            } else if !self.filter.is_empty() {
                // 过滤生效时 Esc 先取消过滤：这是撤销过滤最顺手的位置
                self.apply_filter(String::new());
            } else {
                self.focus = Focus::Params;
            }
            return;
        }

        if self.in_input() {
            self.input_key(k);
            return;
        }

        match k.code {
            // 页签切换：数字键在参数栏/数据区焦点下是空闲的，输入框里则照常打字。
            // 1/2/3/4 → 终端 / 日志 / 统计 / 帮助。
            KeyCode::Char('1') => self.goto_tab(0),
            KeyCode::Char('2') => self.goto_tab(1),
            KeyCode::Char('3') => self.goto_tab(2),
            KeyCode::Char('4') | KeyCode::F(1) | KeyCode::F(2) | KeyCode::F(3) => self.goto_tab(3),
            KeyCode::Char('q') => self.should_quit = true,
            // 清空接收区：保留发送记录与事件日志，任何焦点下都能用
            KeyCode::Char('c') | KeyCode::Char('C') => {
                let n = self.clear_rx();
                self.toast(format!("已清空接收区（{n} 条，累计统计保留）"), false);
            }
            // 暂停 / 恢复自动跟随；/ 打开过滤关键字输入框
            KeyCode::Char('p') | KeyCode::Char('P') => self.toggle_pause(),
            KeyCode::Char('/') => {
                self.focus = Focus::Params;
                self.modal = Some(Modal::Filter(self.filter.clone()));
            }
            // 弱链路终端里一键降档 / 升档刷新率
            KeyCode::Char('F') => self.cycle_fps(),
            // 长行自动折行：输入框焦点下 w 仍然是普通字符（上面已提前返回）
            KeyCode::Char('w') | KeyCode::Char('W') => self.toggle_wrap(),
            // 过滤命中导航：n = 下一个，N = 上一个。
            // 这里只登记请求，实际跳转由渲染层在绘制时消费 ——
            // 「下一条命中」要按行高算，只有渲染层知道每条记录占了多少行。
            KeyCode::Char('n') => self.request_nav(1),
            KeyCode::Char('N') => self.request_nav(-1),
            // 过滤的两个开关：I = 忽略大小写，! = 反向（排除）过滤。
            // 放在这里而不是过滤弹窗里：过滤弹窗只管关键字，
            // 开关要能随时改（正在看命中结果时想起要排除某类数据）。
            KeyCode::Char('I') => self.toggle_filter_ci(),
            KeyCode::Char('!') => self.toggle_filter_invert(),
            _ => match self.focus {
                Focus::Params => self.params_key(k),
                Focus::Input | Focus::Stream => self.body_key(k),
            },
        }
    }

    /// 登记一次「跳到下一条 / 上一条命中」的请求。
    ///
    /// 导航只在**终端页**有意义：其它页签里没有记录流，按下去什么也不会发生。
    /// 与其留一个悬着的请求（等用户切回终端页时突然跳一条命中），
    /// 不如当场说明白。
    fn request_nav(&mut self, dir: i8) {
        if self.tab == 0 {
            self.nav_dir = Some(dir);
        } else {
            self.toast("切到终端页才能跳到命中的记录", false);
        }
    }

    /// 切换页签：回到参数栏焦点，并复位该页的滚动位置。
    fn goto_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.focus = Focus::Params;
        self.reset_scroll();
        self.auto_scroll = true;
        // 导航请求只在终端页消费：换页签时把它作废，
        // 别让它在用户切回来时突然生效。
        self.nav_dir = None;
    }

    /// Tab 循环焦点：参数栏 → 输入框 → 数据区 → 参数栏。
    ///
    /// 拆出「数据区」这一档，是为了让滚动、切视图、清空、记录这些命令
    /// 在不牺牲输入框打字的前提下可用。非终端页没有输入框，
    /// 所以直接在参数栏与右侧之间循环。
    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Params if self.tab == 0 => Focus::Input,
            Focus::Params => Focus::Stream,
            Focus::Input if self.tab == 0 => Focus::Stream,
            Focus::Input | Focus::Stream => Focus::Params,
        };
    }

    fn bump_interval(&mut self, delta: i64) {
        let v = (self.interval_ms as i64 + delta).clamp(50, 60_000);
        self.interval_ms = v as u64;
    }

    /// 切换文本 / HEX 输入模式，并尽量把已有内容转换过去。
    ///
    /// 之前只翻标志位不换内容，会出现「框里还是 `AT+温度`、模式却已变成 HEX」，
    /// 一按发送就是一堆十六进制报错。现在能转就转，转不了就清空并提示。
    pub fn toggle_hex_mode(&mut self) {
        let converted = if self.hex_mode {
            // HEX → 文本：先解成字节再按 UTF-8 还原
            fmt::parse_send_hex(&self.input).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
        } else {
            // 文本 → HEX：按转义规则解析成字节再转十六进制
            fmt::parse_send_text(&self.input).ok().map(|b| fmt::hex_bytes(&b))
        };
        match converted {
            Some(s) => {
                self.cursor = s.chars().count();
                self.input = s;
                self.hex_mode = !self.hex_mode;
                self.toast(
                    format!("已切到{}模式", if self.hex_mode { "HEX" } else { "文本" }),
                    false,
                );
            }
            None => {
                self.input.clear();
                self.cursor = 0;
                self.hex_mode = !self.hex_mode;
                self.toast(
                    format!("内容无法转换，已清空并切到{}模式", if self.hex_mode { "HEX" } else { "文本" }),
                    true,
                );
            }
        }
    }

    fn params_key(&mut self, k: KeyEvent) {
        let last = ROWS.len() - 1;
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                self.scroll_top = 0;
                self.log_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(last);
                self.scroll_top = 0;
                self.log_scroll = 0;
            }
            KeyCode::Left | KeyCode::Char('h') => self.change_row(-1),
            KeyCode::Right | KeyCode::Char('l') => self.change_row(1),
            KeyCode::Enter => self.activate_row(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.refresh_ports(),
            _ => {}
        }
    }

    fn change_row(&mut self, dir: i32) {
        match ROWS[self.sel] {
            Row::Status => {}
            Row::Port => {
                if self.link.is_some() {
                    self.toast("已连接：先关闭串口再切换端口", true);
                } else {
                    self.select_port(dir as isize);
                }
            }
            Row::Baud => {
                let n = BAUDS.len() as i32;
                self.baud_idx = (((self.baud_idx as i32 + dir) % n + n) % n) as usize;
                self.note_drift();
            }
            Row::Bits => {
                for _ in 0..dir.unsigned_abs() {
                    self.bits = self.bits.next();
                }
                self.note_drift();
            }
            Row::Parity => {
                for _ in 0..dir.unsigned_abs() {
                    self.parity = self.parity.next();
                }
                self.note_drift();
            }
            Row::Stops => {
                for _ in 0..dir.unsigned_abs() {
                    self.stops = self.stops.next();
                }
                self.note_drift();
            }
            Row::Flow => {
                for _ in 0..dir.unsigned_abs() {
                    self.flow = self.flow.next();
                }
                self.note_drift();
            }
            Row::View => self.view.toggle(),
            Row::Interval => self.bump_interval(dir as i64 * 50),
            Row::Toggle | Row::Refresh => {}
        }
    }

    fn note_drift(&mut self) {
        if self.settings_drift() {
            self.toast("参数已改，关闭并重新打开串口后生效", false);
        }
    }

    fn activate_row(&mut self) {
        match ROWS[self.sel] {
            Row::Status | Row::Port => {
                if self.connected() {
                    self.toast(format!("已连接 {}", self.link.as_ref().unwrap().cfg.port), false);
                } else {
                    self.open_link();
                }
            }
            Row::Toggle => self.toggle_link(),
            Row::Refresh => self.refresh_ports(),
            _ => self.change_row(1),
        }
    }

    fn body_key(&mut self, k: KeyEvent) {
        let h = self.view_h.max(1) as i32;
        // 日志页与帮助页共用滚动处理
        if self.tab != 0 {
            match k.code {
                KeyCode::Up => self.log_scroll = self.log_scroll.saturating_sub(1),
                KeyCode::Down => self.log_scroll += 1,
                KeyCode::PageUp => self.log_scroll = self.log_scroll.saturating_sub(h as usize),
                KeyCode::PageDown => self.log_scroll += h as usize,
                KeyCode::Home => self.log_scroll = 0,
                KeyCode::Char('x') | KeyCode::Char('X') if self.tab == 1 => self.clear_records(),
                KeyCode::Char('o') | KeyCode::Char('O') => self.toggle_link(),
                _ => {}
            }
            return;
        }

        // 终端页（焦点在数据区时）：滚动与显示开关
        match k.code {
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-(self.view_h.max(1) as isize)),
            KeyCode::PageDown => self.scroll_by(self.view_h.max(1) as isize),
            KeyCode::Home => {
                self.auto_scroll = false;
                self.scroll_top = 0;
            }
            KeyCode::End => self.follow_tail(),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if self.auto_scroll {
                    self.auto_scroll = false;
                } else {
                    self.follow_tail();
                }
            }
            KeyCode::Char('v') | KeyCode::Char('V') => self.view.toggle(),
            KeyCode::Char('x') | KeyCode::Char('X') => self.clear_records(),
            KeyCode::Char('l') | KeyCode::Char('L') => self.toggle_recorder(),
            KeyCode::Char('o') | KeyCode::Char('O') => self.toggle_link(),
            // 打开当前可见记录的详情：只有数据区焦点下才有「当前可见记录」
            KeyCode::Enter => {
                self.open_detail();
            }
            _ => {}
        }
    }

    /// 手动滚动数据区。任何手动滚动都会关闭自动跟随，
    /// 按 `End` 或 `a` 才能恢复，避免新数据把用户从正在看的位置拽走。
    pub fn scroll_by(&mut self, lines: isize) {
        self.auto_scroll = false;
        if lines < 0 {
            self.scroll_top = self.scroll_top.saturating_sub(lines.unsigned_abs());
        } else {
            self.scroll_top += lines as usize;
        }
        // 向下滚到底部就自动恢复跟随，和真实终端一致：
        // 不然用户手动翻两下之后，必须 Tab 到数据区按 End 才能看回实时数据。
        // view_max_top > 0 表示"下面确实还有内容"——内容不足一屏时不该触发。
        // 暂停期间不触发：暂停就该一直暂停，不能被一次滚动悄悄解除。
        if lines > 0 && !self.paused && self.view_max_top > 0 && self.scroll_top >= self.view_max_top {
            self.follow_tail();
        }
    }

    /// 恢复自动跟随并跳到最新一行。
    pub fn follow_tail(&mut self) {
        self.paused = false;
        self.auto_scroll = true;
        self.scroll_top = usize::MAX / 2; // 由绘制时 clamp_scroll 收敛到底部
    }

    /// 绘制时调用：把滚动位置收敛到合法范围，并记录可滚上限。
    ///
    /// 收敛逻辑放在这里而不是渲染层，滚动与跟随的行为才能被单测覆盖。
    /// 暂停时即使 auto_scroll 为真也不追新 —— 暂停的意义就是画面别动。
    pub fn clamp_scroll(&mut self, max_top: usize) {
        self.view_max_top = max_top;
        self.scroll_top = self.clamped_scroll(max_top);
    }

    /// 夹紧后的滚动位置（只算不改）。
    ///
    /// 绘制时先拿它算出起点，渲染结束后再由 [`Self::clamp_scroll`] 写回：
    /// 渲染过程要借用 App 的记录，借用期间不能改 App。
    pub fn clamped_scroll(&self, max_top: usize) -> usize {
        if self.auto_scroll && !self.paused {
            max_top
        } else {
            self.scroll_top.min(max_top)
        }
    }

    fn input_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => self.send_input(),
            KeyCode::Backspace => {
                // 编辑内容即退出发送历史的浏览态
                self.leave_history_browse();
                if self.cursor > 0 {
                    let idx = byte_idx(&self.input, self.cursor - 1);
                    self.input.remove(idx);
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                self.leave_history_browse();
                if self.cursor < self.input.chars().count() {
                    let idx = byte_idx(&self.input, self.cursor);
                    self.input.remove(idx);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.input.chars().count() {
                    self.cursor += 1;
                }
            }
            // 输入框是单行的，上下键在打字时没有别的用途，直接用来滚数据区
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-(self.view_h.max(1) as isize)),
            KeyCode::PageDown => self.scroll_by(self.view_h.max(1) as isize),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) => {
                self.leave_history_browse();
                let idx = byte_idx(&self.input, self.cursor);
                self.input.insert(idx, c);
                self.cursor += 1;
            }
            _ => {}
        }
    }

    /// 退出发送历史的浏览态。
    ///
    /// 用户开始编辑内容后就不能再算作「在翻历史」：否则下一次 Ctrl+Up
    /// 会把刚敲的字整段覆盖掉，看起来就像输入丢了。
    fn leave_history_browse(&mut self) {
        self.history_idx = None;
        self.history_draft = None;
    }

    fn modal_key(&mut self, m: Modal, k: KeyEvent) {
        match m {
            Modal::FilePath(mut buf) => match k.code {
                KeyCode::Esc => self.toast("已取消", false),
                KeyCode::Enter => self.send_file(buf.trim()),
                KeyCode::Backspace => {
                    buf.pop();
                    self.modal = Some(Modal::FilePath(buf));
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.modal = Some(Modal::FilePath(buf));
                }
                _ => self.modal = Some(Modal::FilePath(buf)),
            },
            Modal::Filter(mut buf) => match k.code {
                KeyCode::Esc => self.toast("已取消过滤", false),
                KeyCode::Enter => {
                    self.apply_filter(buf.trim().to_string());
                    self.modal = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.modal = Some(Modal::Filter(buf));
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.modal = Some(Modal::Filter(buf));
                }
                _ => self.modal = Some(Modal::Filter(buf)),
            },
            // 详情弹窗是只读的：Esc 与 Enter 都是收摊，其余键在记录之间翻。
            Modal::Detail { idx, hex } => match k.code {
                KeyCode::Esc | KeyCode::Enter => self.modal = None,
                KeyCode::Up | KeyCode::Down => {
                    // 先把弹窗放回去：翻记录要从当前序号算起
                    self.modal = Some(Modal::Detail { idx, hex });
                    self.detail_step(if k.code == KeyCode::Up { -1 } else { 1 });
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    self.modal = Some(Modal::Detail { idx, hex: !hex })
                }
                _ => self.modal = Some(Modal::Detail { idx, hex }),
            },
        }
    }

    /// 应用关键字过滤并复位滚动：行数全变了，之前的位置没有意义。
    pub fn apply_filter(&mut self, keyword: String) {
        self.filter = keyword;
        self.reset_scroll();
        if self.filter.is_empty() {
            self.toast("已取消过滤", false);
        } else if self.filter_invert {
            // 排除模式：屏幕上显示的是**没命中**的那些。
            // 说「命中 N 条」而 N 指的是没命中的条数，等于告诉用户
            // 「有 N 条记录被显示出来了」—— 恰好是反的。
            let hit = self.filter_hit_count();
            let shown = self.filtered_count();
            self.toast(
                format!("过滤「{}」已排除 {hit} 条（显示 {shown} 条）", self.filter),
                false,
            );
        } else {
            let n = self.filtered_count();
            self.toast(format!("过滤「{}」命中 {n} 条", self.filter), false);
        }
    }

    // ---------- 记录详情弹窗 ----------

    /// 打开当前可见记录的详情弹窗。没有记录时返回 false。
    ///
    /// 打开的是数据区顶着的第一条（`top_record`）：那是用户正在看的那条，
    /// 弹窗里再翻也是围着它前后走。
    pub fn open_detail(&mut self) -> bool {
        let total = self.filtered_count();
        if total == 0 {
            return false;
        }
        // top_record 由渲染层写回，过滤刚变过时它可能已经越界，夹到最后一条。
        let idx = self.top_record.min(total - 1);
        self.modal = Some(Modal::Detail { idx, hex: false });
        true
    }

    /// 关闭详情弹窗。
    #[allow(dead_code)] // 界面层接上之前没人用
    pub fn close_detail(&mut self) {
        if matches!(self.modal, Some(Modal::Detail { .. })) {
            self.modal = None;
        }
    }

    /// 在详情弹窗里翻到上一条 / 下一条记录。越界就不动。
    ///
    /// 只在命中的记录之间翻：过滤生效时，被过滤掉的记录不该出现在弹窗里。
    pub fn detail_step(&mut self, dir: isize) {
        let Some(m) = &self.modal else { return };
        let (idx, hex) = match m {
            Modal::Detail { idx, hex } => (*idx, *hex),
            _ => return,
        };
        let total = self.filtered_count();
        if total == 0 {
            return;
        }
        let next = idx as isize + dir;
        if next < 0 || next >= total as isize {
            return; // 越界不动：翻到头就停住，别把序号甩出去
        }
        self.modal = Some(Modal::Detail { idx: next as usize, hex });
    }

    /// 暂停 / 恢复自动跟随。暂停只停「画面追新」，数据照收不误。
    pub fn toggle_pause(&mut self) {
        if self.paused {
            self.follow_tail();
            self.toast("已恢复自动跟随", false);
        } else {
            self.paused = true;
            self.toast("已暂停跟随（数据仍在接收，按 End 恢复）", false);
        }
    }
}

/// 第 n 个字符对应的字节下标。
fn byte_idx(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    /// 把界面上会画的所有记录（已封口 + 正在累积的当前行）取出来。
    fn snapshot(app: &App) -> Vec<&Record> {
        app.stream().collect()
    }

    /// 设备一个字符一个字符地发时，整段数据必须合并成一行，
    /// 绝不能每个字符独占一行（这是线上复现过的回归）。
    #[test]
    fn single_byte_rx_merges_into_one_line() {
        let mut app = App::new();
        app.feed_rx(b"a");
        app.feed_rx(b"b");
        app.feed_rx(b"c");

        let rows = snapshot(&app);
        assert_eq!(rows.len(), 1, "逐字符合并后应只有 1 行，实际 {} 行", rows.len());
        assert_eq!(rows[0].data, b"abc", "合并后的内容不对");
        assert_eq!(rows[0].dir, Dir::Rx);
        // 还没遇到换行，也没到空闲时间：数据仍在「当前行」里，实时显示
        assert!(app.records.is_empty(), "未换行不应提前封口，records 有 {} 条", app.records.len());
        assert!(app.pending_rx.is_some(), "当前行不应为空");
    }

    /// 只有数据里真的出现 `\n` 才另起一行；换行之后的尾巴留在当前行。
    #[test]
    fn newline_starts_new_line() {
        let mut app = App::new();
        app.feed_rx(b"line1\r\nline2");

        assert_eq!(app.records.len(), 1, "换行前的部分应封口成 1 条，实际 {}", app.records.len());
        assert_eq!(app.records[0].data, b"line1\r\n", "封口内容应含换行符本身");
        let tail = app.pending_rx.as_ref().expect("换行后的尾巴应留在当前行");
        assert_eq!(tail.data, b"line2", "尾巴内容不对");

        // 界面上是两行：一行已封口 + 一行当前行
        assert_eq!(app.stream().count(), 2, "stream 应有 2 条");
    }

    /// 换行符被拆在两次读取之间也要正确封口，不产生半截记录。
    #[test]
    fn newline_split_across_reads() {
        let mut app = App::new();
        app.feed_rx(b"he");
        app.feed_rx(b"llo\r\n");

        assert_eq!(app.records.len(), 1, "应合并成 1 条已封口记录，实际 {}", app.records.len());
        assert_eq!(app.records[0].data, b"hello\r\n", "跨读取合并的内容不对");
        assert!(app.pending_rx.is_none(), "以换行结尾后不应残留当前行，实际 {:?}", app.pending_rx.as_ref().map(|r| r.data.clone()));
    }

    /// 末尾没有换行的余量必须留在「当前行」，不能被提前封口；
    /// 换行之前的内容整体封口（一条记录里按真实换行分成多行显示）。
    #[test]
    fn trailing_bytes_stay_pending() {
        let mut app = App::new();
        app.feed_rx(b"a\r\nb\r\nc");

        // 前两个换行之前的内容已封口
        assert_eq!(app.records.len(), 1, "换行前的部分应封口成 1 条记录，实际 {} 条", app.records.len());
        assert_eq!(app.records[0].data, b"a\r\nb\r\n", "封口内容不对");
        // 界面上仍是两行真实内容
        let shown: Vec<&[u8]> = fmt::split_lines(&app.records[0].data)
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(shown, vec![b"a".as_slice(), b"b".as_slice()], "封口记录应显示为 a、b 两行");

        // 末尾余量还在当前行
        let tail = app.pending_rx.as_ref().expect("末尾余量应留在当前行");
        assert_eq!(tail.data, b"c", "末尾余量内容不对");
        assert_eq!(app.stream().count(), 2, "stream 应有 2 条");
    }

    /// 空闲超过 RX_IDLE_SEAL 后把当前行收尾，下次数据另起一行。
    #[test]
    fn idle_seal_commits_pending_line() {
        let mut app = App::new();
        app.feed_rx(b"ping");
        assert!(app.pending_rx.is_some(), "刚到的数据应还在当前行");

        // 把最近到达时刻往前拨，模拟已经空闲超过阈值
        let r = app.pending_rx.as_mut().unwrap();
        r.last_rx = Instant::now() - Duration::from_secs(2);
        app.idle_seal();

        assert!(app.pending_rx.is_none(), "空闲到期后当前行应被封口");
        assert_eq!(app.records.len(), 1, "应封口成 1 条记录，实际 {}", app.records.len());
        assert_eq!(app.records[0].data, b"ping", "封口内容不对");
        assert_eq!(app.records[0].dir, Dir::Rx);
        assert_eq!(app.buffered, 4, "封口后缓冲字节数应为 4");
        assert_eq!(app.stream().count(), 1, "封口后 stream 仍只有 1 行");
    }

    /// 空闲封行不能提前收尾：刚到的数据还不到 RX_IDLE_SEAL。
    #[test]
    fn idle_seal_keeps_fresh_line() {
        let mut app = App::new();
        app.feed_rx(b"x");
        app.idle_seal();

        let pending = app.pending_rx.as_ref().expect("未到空闲时间，当前行应保留");
        assert_eq!(pending.data, b"x", "当前行内容不该被改动");
        assert!(app.records.is_empty(), "未到空闲时间不应封口，records 有 {} 条", app.records.len());
    }

    /// 合并后的时间戳取首字节的时刻，后到的字节不刷新它。
    #[test]
    fn merged_line_keeps_first_byte_timestamp() {
        let mut app = App::new();
        let before = SystemTime::now();
        app.feed_rx(b"a");
        sleep(Duration::from_millis(5));
        app.feed_rx(b"b");
        let after = SystemTime::now();

        let rec = app.pending_rx.as_ref().expect("两字节应合并进当前行");
        assert_eq!(rec.data, b"ab", "两字节应合并成一行");
        assert_eq!(app.stream().count(), 1, "合并后只有 1 行");
        // 时间戳落在第一次 feed 前后夹出的区间内，即没有被第二次刷新
        assert!(
            rec.t >= before && rec.t <= after,
            "时间戳应取首字节的时刻，实际 {:?} 不在 [{:?}, {:?}] 内",
            rec.t,
            before,
            after
        );
    }

    /// 发送是一次完整写入，立即封口，不参与接收端的行合并。
    #[test]
    fn tx_is_sealed_immediately() {
        let mut app = App::new();
        app.commit_tx(b"AT\r\n");

        assert!(app.pending_rx.is_none(), "TX 不应占用接收端的当前行");
        assert_eq!(app.stream().count(), 1, "stream 应只有 1 条");
        let rec = &app.records[0];
        assert_eq!(rec.dir, Dir::Tx, "方向应为 TX");
        assert_eq!(rec.data, b"AT\r\n", "TX 内容不应被截断或合并");
    }

    /// 系统事件方向是 SYS，文本原样保留。
    #[test]
    fn sys_record_has_sys_dir() {
        let mut app = App::new();
        app.push_sys("hello");

        assert!(app.pending_rx.is_none(), "SYS 不应占用接收端的当前行");
        assert_eq!(app.records.len(), 1, "应产生 1 条记录，实际 {}", app.records.len());
        let rec = &app.records[0];
        assert_eq!(rec.dir, Dir::Sys, "方向应为 SYS");
        assert_eq!(rec.data, b"hello", "SYS 内容不对");
    }

    /// 缓冲区上限：记录数封顶，buffered 始终等于现存记录的数据长度之和。
    #[test]
    fn buffer_limit_trims_old_records() {
        let mut app = App::new();
        for i in 0..30_000 {
            app.feed_rx(format!("L{i}\r\n").as_bytes());
        }

        assert!(
            app.records.len() <= MAX_RECORDS,
            "记录数应不超过上限 {MAX_RECORDS}，实际 {}",
            app.records.len()
        );
        assert!(app.pending_rx.is_none(), "每条都以换行结尾，不应残留当前行");
        // 丢弃最旧记录时 buffered 要同步扣减，保持自洽
        let sum: u64 = app.records.iter().map(|r| r.data.len() as u64).sum();
        assert_eq!(app.buffered, sum, "buffered({}) 应等于现存记录数据长度之和({})", app.buffered, sum);
        // 留下的是最新的记录
        assert_eq!(app.records.back().map(|r| r.data.clone()), Some(b"L29999\r\n".to_vec()));
    }

    // ---------- 焦点与按键分发（真实用户踩过的坑） ----------

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Tab 必须在输入框里也生效：曾经被 input_key 吞掉，导致跳不回参数栏。
    #[test]
    fn tab_cycles_focus_through_all_three_stops() {
        let mut app = App::new();
        assert_eq!(app.focus, Focus::Params);

        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input, "第一次 Tab 应进入输入框");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Stream, "第二次 Tab 应到数据区");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Params, "第三次 Tab 回到参数栏");

        // 在输入框里打字不会影响焦点
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input, "ab");
        // 关键回归：在输入框里按 Tab 必须能跳出去
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Stream, "输入框内 Tab 必须生效");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Params, "必须能从输入框一路回到参数栏");
    }

    /// 非终端页没有「输入框」这一档，Tab 在参数栏与右侧之间来回。
    #[test]
    fn tab_skips_input_on_non_terminal_tabs() {
        let mut app = App::new();
        app.tab = 1;
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Stream, "日志页应直接跳到右侧");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Params);
    }

    /// 页签切换：1/2/3/4 依次是终端、日志、统计、帮助。
    #[test]
    fn number_keys_switch_tabs() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, 1);
        assert_eq!(app.focus, Focus::Params);
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.tab, 2);
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.tab, 0);
        // 第 4 个页签是帮助页
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab, 3);
    }

    /// 输入框里打数字是内容，不是切页签。
    #[test]
    fn digits_type_in_input_focus() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Tab)); // 输入框
        for c in ["1", "2", "3"] {
            app.on_key(key(KeyCode::Char(c.chars().next().unwrap())));
        }
        assert_eq!(app.tab, 0, "输入框里数字不应切页签");
        assert_eq!(app.input, "123");
    }

    /// 输入框是单行的，方向键必须能滚动数据区。
    #[test]
    fn arrow_keys_scroll_while_typing() {
        let mut app = App::new();
        for i in 0..30 {
            app.feed_rx(format!("line{i}\r\n").as_bytes());
        }
        app.view_h = 10;
        app.on_key(key(KeyCode::Tab)); // 输入框
        assert_eq!(app.focus, Focus::Input);

        app.on_key(key(KeyCode::Up));
        assert_eq!(app.scroll_top, 0, "已经在顶部，继续上移不应越界");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.scroll_top, 1);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.scroll_top, 2);
        assert!(!app.auto_scroll, "手动滚动应关闭自动跟随");

        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll_top, 12, "翻页应按可视高度 {} 行", app.view_h);

        // 打字仍然正常，两件事互不干扰
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "x");
        assert_eq!(app.scroll_top, 12, "打字不应影响滚动位置");
    }

    /// 数据区档位下 v/x/l 等命令键生效，不会被当成输入内容。
    #[test]
    fn stream_focus_accepts_command_keys() {
        let mut app = App::new();
        app.feed_rx(b"data\r\n");
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab)); // 数据区
        assert_eq!(app.focus, Focus::Stream);

        app.on_key(key(KeyCode::Char('v')));
        assert!(matches!(app.view, View::Hex), "数据区里 v 应切换视图，实际 {:?}", app.view.label());
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.records.is_empty(), "数据区里 x 应清空缓冲");
    }

    /// Ctrl+Tab 直接跳到数据区。
    #[test]
    fn ctrl_tab_jumps_to_stream() {
        let mut app = App::new();
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Stream);
    }

    /// 文本 / HEX 切换时应把已有内容一起转换，避免"框里是文本、模式却是 HEX"。
    #[test]
    fn hex_mode_toggle_converts_existing_content() {
        let mut app = App::new();
        app.input = "AT+温度\r\n".into();
        app.cursor = app.input.chars().count();
        app.toggle_hex_mode();
        assert!(app.hex_mode);
        assert!(
            app.input.bytes().all(|b| b.is_ascii_hexdigit() || b == b' '),
            "转成 HEX 后应只剩十六进制数字与空格，实际 {:?}",
            app.input
        );
        assert!(app.input.starts_with("41 54 2B E6 B8 A9 E5 BA A6"), "实际 {}", app.input);

        // 再切回文本应还原
        app.toggle_hex_mode();
        assert!(!app.hex_mode);
        assert_eq!(app.input, "AT+温度\r\n");
    }

    /// 切不过去时清空并提示，绝不留下会误发的脏内容。
    #[test]
    fn hex_mode_toggle_clears_unconvertible_content() {
        let mut app = App::new();
        app.hex_mode = true;
        app.input = "4G".into(); // G 不是十六进制数字
        app.cursor = 2;
        app.toggle_hex_mode();
        assert!(!app.hex_mode, "已切回文本模式");
        assert_eq!(app.input, "", "无法转换时必须清空，否则一发送就是十六进制报错");
        assert!(app.toast.as_ref().is_some_and(|(_, err, _)| *err), "应给出错误提示");
    }

    /// 状态区显示的字节数必须与真正发送的一致。
    #[test]
    fn parsed_len_matches_what_will_be_sent() {
        let mut app = App::new();
        for input in ["AT\r\n", "温度", "\\x41\\x42", ""] {
            app.input = input.into();
            let n = app.parsed_len().expect("应能解析");
            let expect = fmt::parse_send_text(input).unwrap().len();
            assert_eq!(n, expect, "输入 {input:?} 的字节数不一致");
        }
        app.hex_mode = true;
        app.input = "41 42 43".into();
        assert_eq!(app.parsed_len().unwrap(), 3, "HEX 模式应解析出 3 字节");
    }

    // ---------- 审查发现的缺陷回归 ----------

    /// 接收字节数只能累加一次。之前 feed_rx 与 drain_serial 各加了一遍，
    /// 状态栏显示的接收量是真实值的 2 倍，而单测直接调 feed_rx 抓不到。
    #[test]
    fn rx_counters_are_not_double_counted() {
        let mut app = App::new();
        for chunk in [&b"hello"[..], &b"world"[..], &b"!"[..]] {
            app.feed_rx(chunk);
        }
        assert_eq!(app.rx_bytes, 11, "11 字节应收 11，不应重复计数");
        assert_eq!(app.rx_packets, 3, "3 次 feed 应算 3 包");
    }

    /// 单条数据超过缓冲上限时，不能把整个缓冲区（含自身）一起抹掉。
    ///
    /// 注意语义：一条 4 MiB 的巨型记录本身就会占满整个预算，
    /// 此时淘汰历史记录是合理的；真正要保证的是"界面上永远有东西可看"。
    #[test]
    fn oversized_record_does_not_wipe_everything() {
        let mut app = App::new();
        app.feed_rx(b"first-line\r\n");
        app.feed_rx(b"second-line\r\n");

        // 5 MiB 无换行数据，单条就超过 4 MiB 上限
        app.feed_rx(&vec![b'A'; 5 * 1024 * 1024]);
        assert!(!app.records.is_empty(), "缓冲不能被一条巨型数据清空");
        assert!(app.stream().count() > 0, "界面上至少要有内容可显示");
        let sum: u64 = app.records.iter().map(|r| r.data.len() as u64).sum();
        assert_eq!(app.buffered, sum, "buffered 要与现存记录自洽");

        // 巨型记录之后的新数据必须可见（最新数据永远能看到）
        app.feed_rx(b"after\r\n");
        assert!(
            app.stream().any(|r| r.data == b"after\r\n"),
            "巨型记录不应把后续数据也挤掉"
        );
    }

    /// 只用 \r 刷新的设备（进度条）不能把当前行撑到无限大，
    /// 也不能因为每次收数都全量扫描而退化成二次复杂度。
    #[test]
    fn pending_line_is_capped_for_cr_only_devices() {
        let mut app = App::new();
        let start = Instant::now();
        // 模拟持续刷新：每次只发一个 \r，永远等不到换行与空闲阈值。
        // 12 字节 × 120_000 ≈ 1.4 MB，应触发多次封口。
        for _ in 0..120_000 {
            app.feed_rx(b"progress 50%\r");
        }
        let elapsed = start.elapsed();
        assert!(
            app.pending_rx.as_ref().map(|r| r.data.len()).unwrap_or(0) <= MAX_PENDING,
            "当前行不应超过上限"
        );
        assert!(app.records.len() >= 2, "超出上限的刷新应被切成多条记录");
        assert!(
            elapsed < Duration::from_secs(5),
            "12 万次刷新耗时 {elapsed:?}，收数路径疑似退化"
        );
    }

    /// 断开连接必须停掉循环发送，避免重连到别的设备时静默发出旧命令。
    #[test]
    fn disconnect_stops_repeating_send() {
        let mut app = App::new();
        app.repeating = true;
        app.last_sent = b"PING\r\n".to_vec();
        app.close_link("测试");
        assert!(!app.repeating, "断开后应自动关闭循环发送");
    }

    /// 向下滚到底部应自动恢复跟随——真实终端就是这个行为，
    /// 否则在输入框翻两下之后必须切焦点才能看回实时数据。
    #[test]
    fn scrolling_to_bottom_resumes_following() {
        let mut app = App::new();
        for i in 0..100 {
            app.feed_rx(format!("line-{i}\r\n").as_bytes());
        }
        app.view_h = 10;
        app.view_max_top = 50; // 由绘制时写入，这里模拟
        app.follow_tail();
        app.clamp_scroll(50); // 模拟绘制收敛
        assert!(app.auto_scroll);
        assert_eq!(app.scroll_top, 50, "跟随时应停在最底部");

        // 向上翻，手动浏览
        app.scroll_by(-30);
        assert!(!app.auto_scroll, "向上翻应脱离跟随");
        assert_eq!(app.scroll_top, 50 - 30);

        // 向下滚过头 → 到底 → 自动恢复跟随
        app.scroll_by(10);
        assert_eq!(app.scroll_top, 30);
        assert!(!app.auto_scroll, "还没到底，不应恢复");

        app.scroll_by(100);
        assert!(app.auto_scroll, "滚到底应自动恢复跟随");
    }

    /// Ctrl+End 在任何焦点下都能一键恢复跟随。
    #[test]
    fn ctrl_end_resumes_following_from_any_focus() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Tab)); // 输入框
        app.clamp_scroll(5);
        app.scroll_by(-5);
        assert!(!app.auto_scroll);
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert!(app.auto_scroll, "Ctrl+End 应恢复跟随");
        assert_eq!(app.focus, Focus::Input, "不应改变焦点");
    }

    /// 缓存的换行计数必须和实际切分行数一致，并且随追加 / 封行同步。
    /// 它错一行，显示高度就会算错（滚到底多出空行或数据被截掉）。
    #[test]
    fn cached_line_count_stays_correct() {
        let mut app = App::new();
        let count = |a: &App| a.stream().map(|r| r.text_lines()).sum::<usize>();

        // 逐字符合并：整段一行
        for c in [b"a", b"b", b"c"] {
            app.feed_rx(c);
        }
        assert_eq!(count(&app), 1);

        // 带换行：已封口一条 + 当前行
        app.feed_rx(b"\r\nsecond line");
        assert_eq!(count(&app), 2);
        app.feed_rx(b"\r\nthird");
        assert_eq!(count(&app), 3);

        // 末尾换行不产生空行
        app.feed_rx(b"x\r\n");
        assert_eq!(count(&app), 3, "行尾换行不该多出一行");

        // 中途的空行要计入：\r\n\r\n 是两个空行（末尾换行那个不算）
        app.feed_rx(b"\r\n\r\n");
        assert_eq!(count(&app), 5, "真正的空行要占行，末尾换行不占");
        assert_eq!(app.records.back().unwrap().text_lines(), 2);
    }

    /// 超过单条上限后按尾部重算，换行计数不能对不上（否则显示高度是错的）。
    #[test]
    fn truncated_record_recomputes_line_count() {
        let mut app = App::new();
        // 头部塞满换行，尾部一行到底：截尾后行数必须等于真实行数
        let mut data = vec![b'\n'; 64];
        data.extend_from_slice(b"tail");
        data.extend_from_slice(&vec![b'\n'; MAX_BYTES as usize]);
        data.extend_from_slice(b"last\r\n");
        app.feed_rx(&data);
        assert!(!app.records.is_empty());
        for rec in &app.records {
            assert_eq!(
                rec.text_lines(),
                fmt::display_lines(&rec.data).len(),
                "截尾后行数与真实切分不一致"
            );
        }
    }

    /// 清空接收区：只删 RX，发送记录与事件日志留着，累计统计不动。
    #[test]
    fn clear_rx_keeps_tx_and_events() {
        let mut app = App::new();
        app.feed_rx(b"first\r\n");
        app.feed_rx(b"second\r\n");
        app.commit_tx(b"PING\r\n");
        app.push_sys("提示：已打开串口");
        app.feed_rx("未换行的当前行".as_bytes());

        let removed = app.clear_rx();
        assert_eq!(removed, 2, "应删掉两条已封口的接收记录");
        assert!(app.pending_rx.is_none(), "当前行也应被清掉");
        assert_eq!(app.records.len(), 2, "发送记录 + 事件日志应保留");
        assert_eq!(app.records[0].dir, Dir::Tx);
        assert_eq!(app.records[1].dir, Dir::Sys);

        // 缓冲量按剩下的记录重算，不留悬空的旧值
        let sum: u64 = app.records.iter().map(|r| r.data.len() as u64).sum();
        assert_eq!(app.buffered, sum);
        // 累计统计是「本次连接总量」，清屏不清零
        assert!(app.rx_bytes > 0, "清屏不清零接收总量");
    }

    /// 按 c 键就能清接收区，且在参数栏焦点下也生效。
    #[test]
    fn c_key_clears_rx_from_any_focus() {
        let mut app = App::new();
        app.feed_rx(b"data\r\n");
        app.commit_tx(b"Q\r\n");
        assert_eq!(app.focus, Focus::Params);

        app.on_key(key(KeyCode::Char('c')));
        assert!(app.records.iter().all(|r| r.dir != Dir::Rx), "接收记录应被清掉");
        assert!(app.records.iter().any(|r| r.dir == Dir::Tx), "发送记录应保留");
        assert!(app.toast.is_some(), "应给出结果提示");
    }

    /// 脏标记：界面没变化时指纹必须稳定，变化时必须变。
    /// 指纹不稳就等于白优化，稳定性丢了会退化成每帧都重绘。
    #[test]
    fn ui_signature_is_stable_until_something_visible_changes() {
        let mut app = App::new();
        let base = app.ui_signature();
        assert_eq!(base, app.ui_signature(), "什么都不做时指纹必须不变");

        // 收数据 → 必须变（否则新数据不会显示出来）
        app.feed_rx(b"hello\r\n");
        let after_rx = app.ui_signature();
        assert_ne!(after_rx, base, "收到数据后必须重绘");

        // 继续往当前行追加字节 → 也必须变（当前行是实时显示的）
        app.feed_rx(b"more");
        let after_pending = app.ui_signature();
        assert_ne!(after_pending, after_rx, "当前行增长必须重绘");

        // 只切焦点 → 必须变
        let a = app.ui_signature();
        app.on_key(key(KeyCode::Tab));
        assert_ne!(app.ui_signature(), a, "切焦点必须重绘");

        // 切帧率 → 必须变（状态栏会显示它）
        let b = app.ui_signature();
        app.cycle_fps();
        assert_ne!(app.ui_signature(), b, "改帧率必须重绘");
        assert!(FPS_STEPS.contains(&app.fps), "帧率应落在预设档位里");
    }

    /// 过滤：只保留命中关键字的记录，且按记录级过滤（不打乱行结构）。
    #[test]
    fn filter_keeps_only_matching_records() {
        let mut app = App::new();
        app.feed_rx(b"INFO boot ok\r\n");
        app.feed_rx("ERROR 温度过高\r\n".as_bytes());
        app.feed_rx("INFO 继续运行\r\n".as_bytes());
        assert_eq!(app.stream_filtered().count(), 3, "不过滤时是全部记录");

        app.apply_filter("ERROR".into());
        let hit: Vec<_> = app.stream_filtered().collect();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].data, "ERROR 温度过高\r\n".as_bytes());

        // 中文关键字也能命中（按原始字节搜，合法 UTF-8 必然落在合法片段里）
        app.apply_filter("温度".into());
        assert_eq!(app.stream_filtered().count(), 1);

        // 取消过滤后恢复全量
        app.apply_filter(String::new());
        assert_eq!(app.stream_filtered().count(), 3);
        assert!(app.filter.is_empty());
    }

    /// 过滤判据必须与显示文本一致：显示成 `^X` / `·` / `^[m` 的内容
    /// 能被搜到，被折叠掉的转义序列（`[31m`）不该再算命中。
    #[test]
    fn filter_matches_the_displayed_text() {
        let mut app = App::new();
        app.feed_rx(b"plain\r\n");
        app.feed_rx("\x1b[31mERROR\x1b[0m\r\n".as_bytes());
        app.feed_rx(b"50%\r75%\n"); // 单独的 \r 显示成 ^M
        app.feed_rx(&[0xFF, b'x', b'\n']); // 非法字节显示成 ·

        // 转义序列已被折叠成 `^[m`：原始字节里的「[31m」不该再命中
        app.apply_filter("[31m".into());
        assert_eq!(app.stream_filtered().count(), 0, "折叠掉的转义不该算命中");

        // 控制符显示成 `^M`：按显示文本能搜到
        app.apply_filter("^M".into());
        assert_eq!(app.stream_filtered().count(), 1, "`^M` 在画面上是看得见的");

        // 非法字节显示成 `·`：按显示文本能搜到
        app.apply_filter("·".into());
        assert_eq!(app.stream_filtered().count(), 1, "`·` 在画面上是看得见的");

        // 普通文本照旧
        app.apply_filter("ERROR".into());
        assert_eq!(app.stream_filtered().count(), 1);
    }

    /// 过滤必须足够快：满缓冲下一帧要过几千条记录。
    #[test]
    fn filtering_a_full_buffer_is_fast() {
        let mut app = App::new();
        for i in 0..8_000 {
            app.feed_rx(format!("row-{i} payload 温度传感器\r\n").as_bytes());        }
        app.apply_filter("温度".into());
        let start = Instant::now();
        let mut n = 0;
        for _ in 0..50 {
            n = app.stream_filtered().count();
        }
        let elapsed = start.elapsed();
        assert!(n > 0);
        assert!(
            elapsed < Duration::from_millis(500),
            "50 次全量过滤耗时 {elapsed:?}，每帧 {} ms",
            elapsed.as_millis() / 50
        );
    }

    /// 忽略大小写的过滤同样要进一帧预算：它比精确匹配慢（要多比一次大小写），
    /// 但不能慢到「打开这个开关界面就卡住」。
    ///
    /// 阈值从 500 毫秒收到 250 毫秒：早先那版按字符扫的实现要 430~576 毫秒，
    /// 阈值压在实测值上，一有后台负载就变红 ——  flaky 的性能测试等于没有测试。
    /// 改成按字节扫之后实测约 107 毫秒，留两倍多的余量，
    /// 而回到旧实现（430 毫秒以上）一定会被这条断言抓住。
    #[test]
    fn case_insensitive_filtering_stays_fast() {
        let mut app = App::new();
        for i in 0..8_000 {
            app.feed_rx(format!("row-{i} payload 温度传感器\r\n").as_bytes());
        }
        app.apply_filter("温度".into());
        app.toggle_filter_ci();
        let start = Instant::now();
        let mut n = 0;
        for _ in 0..50 {
            n = app.stream_filtered().count();
        }
        let elapsed = start.elapsed();
        assert!(n > 0);
        assert!(
            elapsed < Duration::from_millis(250),
            "50 次忽略大小写的全量过滤耗时 {elapsed:?}，每帧 {} ms",
            elapsed.as_millis() / 50
        );
    }

    /// 暂停：数据照收，但画面不再自动追新；滚动也不能把暂停悄悄解除。
    #[test]
    fn pause_freezes_view_but_keeps_receiving() {
        let mut app = App::new();
        for i in 0..100 {
            app.feed_rx(format!("line-{i}\r\n").as_bytes());
        }
        app.clamp_scroll(50);
        app.follow_tail();
        app.clamp_scroll(50); // 模拟一次绘制收敛，跟随位置才是真实行号
        assert!(app.auto_scroll && !app.paused);
        assert_eq!(app.scroll_top, 50);

        app.toggle_pause();
        assert!(app.paused);
        let frozen = app.scroll_top;

        // 暂停期间新数据照收，但滚动位置不动
        for i in 100..200 {
            app.feed_rx(format!("line-{i}\r\n").as_bytes());
        }
        app.clamp_scroll(150);
        assert_eq!(app.scroll_top, frozen, "暂停时画面不能被新数据拽走");
        assert!(app.records.len() > 100, "数据一条都不能少收");

        // 滚到底也不能自动恢复跟随 —— 暂停就该一直暂停
        app.scroll_by(500);
        assert!(app.paused, "滚动不应解除暂停");

        // End / a 才恢复
        app.follow_tail();
        assert!(!app.paused && app.auto_scroll);
        app.clamp_scroll(150);
        assert_eq!(app.scroll_top, 150, "恢复后应跟到最新");
    }

    /// 等长的不同关键字 / 不同提示必须触发重绘，否则界面会停在旧内容上。
    #[test]
    fn equal_length_strings_still_trigger_redraw() {
        let mut app = App::new();
        let a = app.ui_signature();
        app.apply_filter("abc".into());
        let b = app.ui_signature();
        app.apply_filter("xyz".into());
        assert_ne!(a, b);
        assert_ne!(b, app.ui_signature(), "等长不同关键字也必须重绘");

        let mut app2 = App::new();
        app2.toast("串口已打开", false);
        let c = app2.ui_signature();
        app2.toast("串口已关闭", false);
        assert_ne!(c, app2.ui_signature(), "等长不同提示也必须重绘");
    }

    /// 帧数统计（界面每帧给全部记录数行数）必须足够快：
    /// 缓冲塞满 4 MiB / 2 万条时，一次统计要能进一帧预算内。
    #[test]
    fn counting_lines_over_full_buffer_is_fast() {
        let mut app = App::new();
        // 塞到接近上限：2 万条记录
        for i in 0..20_000 {
            app.feed_rx(format!("row-{i}\r\n").as_bytes());
        }
        let start = Instant::now();
        let mut total = 0usize;
        for _ in 0..50 {
            total = app.stream().map(|r| r.text_lines()).sum();
        }
        let elapsed = start.elapsed();
        assert!(total > 0);
        assert!(
            elapsed < Duration::from_millis(500),
            "50 次全量行数统计耗时 {elapsed:?}，每帧会退化成 {} ms",
            elapsed.as_millis() / 50
        );
    }

    // ---------- 发送历史 ----------

    /// 上翻 / 下翻 / 翻到底恢复草稿。
    #[test]
    fn send_history_navigates_up_and_down() {
        let mut app = App::new();
        app.push_history("AT");
        app.push_history("AT+VERSION?");
        app.push_history("PING");
        // 最近的一条在最前面
        assert_eq!(app.send_history, vec!["PING", "AT+VERSION?", "AT"]);

        // 手上还有没发出去的内容：翻历史时先把它存成草稿
        app.input = "草稿".into();
        assert!(app.history_prev(), "第一次上翻应替换输入框内容");
        assert_eq!(app.input, "PING");
        assert_eq!(app.history_idx, Some(0));

        assert!(app.history_prev());
        assert_eq!(app.input, "AT+VERSION?");
        assert!(app.history_prev());
        assert_eq!(app.input, "AT");
        assert!(!app.history_prev(), "已经到最顶了");
        assert_eq!(app.input, "AT", "到顶时不能改输入框");

        // 往回走
        assert!(app.history_next());
        assert_eq!(app.input, "AT+VERSION?");
        assert!(app.history_next());
        assert_eq!(app.input, "PING");
        assert!(app.history_next(), "翻过最新的一条应恢复草稿");
        assert_eq!(app.input, "草稿");
        assert!(app.history_idx.is_none(), "恢复草稿后不再处于浏览状态");
        assert!(!app.history_next(), "不在浏览历史里时不应有动作");
    }

    /// 相邻去重 + 上限 100 条 + 空内容不入队。
    #[test]
    fn send_history_dedups_neighbours_and_caps_at_100() {
        let mut app = App::new();
        app.push_history("A");
        app.push_history("A");
        assert_eq!(app.send_history.len(), 1, "与上一条相同就不该重复入队");

        app.push_history("");
        assert_eq!(app.send_history.len(), 1, "空内容不入队");

        app.push_history("B");
        assert_eq!(app.send_history, vec!["B", "A"], "最近的排在最前面");

        for i in 0..200 {
            app.push_history(&format!("cmd{i}"));
        }
        assert_eq!(app.send_history.len(), 100, "历史最多保留 100 条");
        assert_eq!(app.send_history[0], "cmd199", "最新的一条在最前面");
        assert_eq!(app.send_history[99], "cmd100", "最旧的应该被丢掉");
    }

    /// 没真正写出去的内容不进历史（没连上、写失败都不算）。
    #[test]
    fn send_without_connection_does_not_enter_history() {
        let mut app = App::new();
        app.input = "AT\r\n".into();
        app.send_input();
        assert!(
            app.send_history.is_empty(),
            "字节没写出去就不该进历史，实际 {:?}",
            app.send_history
        );
    }

    /// 输入框里 Ctrl+Up / Ctrl+Down 翻历史；其它焦点下不动输入框。
    #[test]
    fn ctrl_arrows_recall_history_only_in_input_focus() {        let mut app = App::new();
        app.push_history("PING");

        // 参数栏焦点：Ctrl+Up 仍然是调循环间隔（既有行为）
        let interval = app.interval_ms;
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.interval_ms, interval - 50, "非输入框下 Ctrl+Up 仍调间隔");
        assert!(app.input.is_empty(), "非输入框下不该动输入框");
        assert!(app.history_idx.is_none());

        // 输入框焦点：Ctrl+Up / Ctrl+Down 翻历史
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input);
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.input, "PING", "Ctrl+Up 应翻出上一条历史");
        assert_eq!(app.interval_ms, interval - 50, "翻历史时不该再调间隔");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.input, "", "Ctrl+Down 应恢复原来的输入内容");
    }

    /// 浏览发送历史时打字要结束浏览态。
    ///
    /// 否则下一次 Ctrl+Up 会把用户刚敲的字整段覆盖掉（看起来像输入丢了）。
    #[test]
    fn typing_while_browsing_history_ends_the_browse() {
        let mut app = App::new();
        app.push_history("OLD1");
        app.push_history("OLD2");
        app.on_key(key(KeyCode::Tab)); // 输入框
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.input, "OLD2", "Ctrl+Up 应翻出最近的一条");
        assert_eq!(app.history_idx, Some(0));

        app.on_key(key(KeyCode::Char('X')));
        assert_eq!(app.input, "OLD2X");
        assert_eq!(app.history_idx, None, "打字后应当退出发送历史的浏览态");

        // 再翻历史不会把刚敲的 X 悄悄吞掉：草稿里还留着它
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.input, "OLD2");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.input, "OLD2X", "刚敲的字不该被翻历史吞掉");
    }

    // ---------- 自动重连 ----------

    /// 自动重连默认关闭，开关要有提示。
    #[test]
    fn auto_reconnect_toggles_with_toast() {
        let mut app = App::new();
        assert!(!app.auto_reconnect, "默认关闭");
        app.toggle_auto_reconnect();
        assert!(app.auto_reconnect);
        assert!(app.toast.is_some(), "开关应该有提示");
        app.toggle_auto_reconnect();
        assert!(!app.auto_reconnect);
    }

    /// 开启自动重连后，下一次 tick 就该试一次（不用等满 2 秒）。
    #[test]
    fn enabling_auto_reconnect_tries_on_the_next_tick() {
        let mut app = App::new();
        app.ports = vec![PortEntry {
            port: "stui-nonexistent-port".into(),
            desc: "测试用".into(),
        }];
        app.toggle_auto_reconnect();
        // 计时点被往前拨了一个周期，重连的间隔判断会直接放行。
        assert!(
            app.reconnect_mark.elapsed() >= RECONNECT_INTERVAL,
            "开启后应当立刻能试一次，而不是等满 2 秒"
        );
        app.tick(); // 端口打不开，失败一次（不到 5 次不记日志）
        assert_eq!(app.reconnect_fails, 1, "应当已经尝试过一次重连");
    }

    /// 手动断开不该关掉自动重连 —— 这正是这个开关的意义，
    /// 而且日志里必须说清楚「它还开着」。
    #[test]
    fn manual_disconnect_keeps_auto_reconnect_on() {
        let mut app = App::new();
        app.auto_reconnect = true;
        app.close_link("手动断开");
        assert!(app.auto_reconnect, "手动断开不应关掉自动重连");

        app.note_auto_reconnect();
        let texts: Vec<String> = app
            .records
            .iter()
            .map(|r| String::from_utf8_lossy(&r.data).into_owned())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("自动重连仍开启")),
            "日志里要说明自动重连仍开启，实际 {texts:?}"
        );
    }

    /// 重连失败不能刷屏：连着失败 5 次才记一条日志。
    #[test]
    fn auto_reconnect_failure_is_logged_only_every_fifth_time() {
        let mut app = App::new();
        // 一个肯定打不开的端口名：让每一次尝试都快速失败。
        app.ports = vec![PortEntry {
            port: "stui-nonexistent-port".into(),
            desc: "测试用".into(),
        }];
        app.auto_reconnect = true;

        for i in 1..=4 {
            app.reconnect_mark = Instant::now() - RECONNECT_INTERVAL - Duration::from_millis(1);
            app.tick();
            assert!(
                app.records.is_empty(),
                "第 {i} 次失败不该记日志，实际 {} 条",
                app.records.len()
            );
        }

        // 第 5 次才记一条
        app.reconnect_mark = Instant::now() - RECONNECT_INTERVAL - Duration::from_millis(1);
        app.tick();
        assert_eq!(app.records.len(), 1, "第 5 次失败应记一条日志");
        let text = String::from_utf8_lossy(&app.records[0].data);
        assert!(text.contains("自动重连失败"), "日志内容不对：{text}");

        // 再失败 4 次不记，第 10 次才再记一条
        for _ in 0..4 {
            app.reconnect_mark = Instant::now() - RECONNECT_INTERVAL - Duration::from_millis(1);
            app.tick();
        }
        assert_eq!(app.records.len(), 1, "还没到下一个 5 次，不该再记");
        app.reconnect_mark = Instant::now() - RECONNECT_INTERVAL - Duration::from_millis(1);
        app.tick();
        assert_eq!(app.records.len(), 2, "第 10 次失败应再记一条");
    }

    /// 没开自动重连时，绝不主动去开端口。
    #[test]
    fn reconnect_is_off_unless_enabled() {
        let mut app = App::new();
        app.ports = vec![PortEntry {
            port: "stui-nonexistent-port".into(),
            desc: "测试用".into(),
        }];
        app.reconnect_mark = Instant::now() - RECONNECT_INTERVAL - Duration::from_millis(1);
        app.tick();
        assert!(app.link.is_none(), "没开自动重连就不该去连");
        assert!(app.records.is_empty(), "不该产生任何日志");
    }

    // ---------- 毫秒开关 / 速率 / 时长 ----------

    /// 毫秒开关默认开，切换要有提示。
    #[test]
    fn millisecond_toggle_defaults_to_on() {
        let mut app = App::new();
        assert!(app.show_ms, "默认显示毫秒");
        app.toggle_ms();
        assert!(!app.show_ms);
        assert!(app.toast.is_some(), "切换应该有提示");
        app.toggle_ms();
        assert!(app.show_ms);
    }

    /// 速率：窗口没满不出数，满 1 秒按这一秒的字节数结算。
    #[test]
    fn rate_stat_updates_after_a_full_window() {
        let mut app = App::new();
        app.feed_rx(b"0123456789"); // 10 字节
        assert_eq!(app.rate_rx_acc, 10, "收到的字节要累进统计");

        app.tick();
        assert_eq!(app.rx_rate, 0, "窗口没满不该出速率");

        // 把窗口起点往前拨 2 秒，模拟已经过去两个统计周期
        app.rate_mark = Instant::now() - Duration::from_secs(2);
        app.tick();
        assert_eq!(app.rx_rate, 5, "10 字节 / 2 秒 = 5 B/s");
        assert_eq!(app.rate_rx_acc, 0, "结算后累计要清零");
    }

    /// 速率文本要短、要能塞进状态栏。
    #[test]
    fn rate_text_is_short_and_readable() {
        let mut app = App::new();
        assert_eq!(app.rate_text(), "0 B/s", "没有数据时就是 0 B/s");
        app.rx_rate = 512;
        assert_eq!(app.rate_text(), "512 B/s");
        app.rx_rate = 1536;
        assert_eq!(app.rate_text(), "1.5 kB/s");
        app.rx_rate = 2 * 1024 * 1024;
        assert_eq!(app.rate_text(), "2.0 MB/s");
        app.tx_rate = 1024;
        assert_eq!(app.tx_rate_text(), "1.0 kB/s");
    }

    /// 会话时长：从 App::new 起算，只增不减。
    #[test]
    fn uptime_grows_from_start() {
        let app = App::new();
        assert!(app.uptime() < Duration::from_secs(5), "刚建好不该有很长时长");
        std::thread::sleep(Duration::from_millis(20));
        assert!(app.uptime() >= Duration::from_millis(10), "时长要随时间增长");
    }

    // ---------- 脏标记与新状态 ----------

    /// 新增的可见状态必须进指纹，否则界面会停在旧的样子。
    ///
    /// 这里直接改字段（不走 toggle），避免 toast 的变化把断言「顶掉」。
    #[test]
    fn ui_signature_notices_new_visible_state() {
        let mut app = App::new();

        let a = app.ui_signature();
        app.auto_reconnect = true;
        assert_ne!(app.ui_signature(), a, "自动重连开关必须触发重绘");
        app.auto_reconnect = false;

        let b = app.ui_signature();
        app.show_ms = false;
        assert_ne!(app.ui_signature(), b, "毫秒开关必须触发重绘");
        app.show_ms = true;

        let c = app.ui_signature();
        app.push_history("AT");
        assert_ne!(app.ui_signature(), c, "发送历史变化必须触发重绘");

        let d = app.ui_signature();
        app.history_idx = Some(0);
        assert_ne!(app.ui_signature(), d, "历史游标变化必须触发重绘");

        let e = app.ui_signature();
        app.rx_rate = 1234;
        assert_ne!(app.ui_signature(), e, "接收速率变化必须触发重绘");

        let f = app.ui_signature();
        app.tx_rate = 4321;
        assert_ne!(app.ui_signature(), f, "发送速率变化必须触发重绘");
    }

    // ---------- 显示缓存的失效 ----------

    /// 回归：正在累积的那一行，追加字节后必须能按**新内容**被过滤命中。
    ///
    /// 缓存一旦填上就改不了的话，过滤会一直按旧内容判命中 ——
    /// 用户看到「命中 N 条」却漏掉了刚到的数据。
    #[test]
    fn pending_display_cache_is_invalidated_on_append() {
        let mut app = App::new();
        app.feed_rx(b"abc");
        app.apply_filter("xyz".into());
        // 这一轮过滤扫描会碰过「当前行」，把它的显示缓存填上
        assert_eq!(app.stream_filtered().count(), 0, "此时不该命中");

        app.feed_rx(b"xyz");
        assert_eq!(
            app.stream_filtered().count(),
            1,
            "追加后必须能按新到的内容命中：显示缓存没有作废"
        );
    }

    /// 追加数据后，列宽缓存与显示文本缓存都要重算（两者都是「数据变了就作废」）。
    #[test]
    fn appending_data_invalidates_record_caches() {
        let mut app = App::new();
        app.feed_rx(b"ab");
        {
            let rec = app.pending_rx.as_ref().expect("当前行应在");
            assert_eq!(rec.line_cols(), &[2], "两个字符占 2 列");
            assert_eq!(rec.display_text(), "ab");
        }

        app.feed_rx(b"cde");
        let rec = app.pending_rx.as_ref().expect("当前行应在");
        assert_eq!(rec.line_cols(), &[5], "追加后列宽缓存要重算");
        assert_eq!(rec.display_text(), "abcde", "追加后显示文本要重算");
    }

    /// 时间戳显示串是「HH:MM:SS.mmm」，且永久缓存（时间戳永不改变）。
    #[test]
    fn stamp_text_is_hms_with_millis_and_permanently_cached() {
        let rec = Record::new(Dir::Rx, b"x");
        let s = rec.stamp_text();
        assert_eq!(s.len(), 12, "HH:MM:SS.mmm 是 12 个字符，实际 {s}");
        assert_eq!(s, rec.stamp().hms_ms(), "内容要与 stamp 一致");
        // 同一个缓存：再取一次还是同一块内存（不会被作废重算）
        assert!(
            std::ptr::eq(rec.stamp_text().as_ptr(), s.as_ptr()),
            "时间戳缓存不该被重算"
        );
    }

    /// 显示形态（各行文本 / 各行列宽）与过滤判据必须是同一份。
    #[test]
    fn text_view_matches_display_text() {
        let rec = Record::new(Dir::Rx, b"hello\r\nworld");
        let view = rec.text_view();
        assert_eq!(view.lines, vec!["hello".to_string(), "world".to_string()]);
        assert_eq!(view.cols, vec![5, 5]);
        assert_eq!(view.text, "hello\nworld");
        assert_eq!(rec.display_text(), view.text, "过滤判据就是整段显示文本");
    }

    // ---------- 折行 ----------

    /// 折行开关：字段翻转、提示条出现、脏标记跟着变。
    #[test]
    fn wrap_toggle_flips_flag_and_triggers_redraw() {
        let mut app = App::new();
        assert!(!app.wrap, "默认不折行（保持「超宽截断」的行为）");
        let a = app.ui_signature();
        app.toggle_wrap();
        assert!(app.wrap, "开关要生效");
        assert_ne!(app.ui_signature(), a, "折行开关必须触发重绘");
        assert!(app.toast.is_some(), "开关要有提示");
        app.toggle_wrap();
        assert!(!app.wrap);
    }

    /// w 键切折行；输入框里 w 仍然是普通字符。
    #[test]
    fn w_key_toggles_wrap_outside_the_input_box() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('w')));
        assert!(app.wrap, "非输入框焦点下 w 应切折行");

        app.focus = Focus::Input;
        app.on_key(key(KeyCode::Char('w')));
        assert_eq!(app.input, "w", "输入框里 w 是普通字符");
        assert!(app.wrap, "输入框里不该切折行");
    }

    // ---------- 记录详情弹窗 ----------

    /// 没有记录时打不开详情；有记录时打开的是数据区顶着的那一条。
    #[test]
    fn detail_modal_opens_at_the_top_visible_record() {
        let mut app = App::new();
        assert!(!app.open_detail(), "没有记录时不能打开详情");

        app.feed_rx(b"one\r\n");
        app.feed_rx(b"two\r\n");
        app.top_record = 1;
        assert!(app.open_detail(), "有记录就该能打开详情");
        match app.modal {
            Some(Modal::Detail { idx, hex }) => {
                assert_eq!(idx, 1, "打开的是数据区顶着的第一条");
                assert!(!hex, "默认按文本显示");
            }
            _ => panic!("应当打开详情弹窗"),
        }
    }

    /// 弹窗里翻记录：到头就停住，不越界。
    #[test]
    fn detail_step_stops_at_the_boundary() {
        let mut app = App::new();
        app.feed_rx(b"one\r\n");
        app.feed_rx(b"two\r\n");
        app.open_detail();

        app.detail_step(-1); // 已经在第一条，不能再往前
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 0),
            _ => panic!("详情弹窗不该被关掉"),
        }

        app.detail_step(1);
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 1),
            _ => panic!("详情弹窗不该被关掉"),
        }

        app.detail_step(1); // 到最后一条，不能再往后
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 1),
            _ => panic!("详情弹窗不该被关掉"),
        }
    }

    /// 过滤生效时，弹窗只在命中的记录之间翻。
    #[test]
    fn detail_navigation_stays_inside_the_filtered_records() {
        let mut app = App::new();
        app.feed_rx(b"INFO one\r\n");
        app.feed_rx(b"ERROR two\r\n");
        app.feed_rx(b"INFO three\r\n");
        app.apply_filter("INFO".into());
        assert_eq!(app.filtered_count(), 2, "只有两条命中");

        app.open_detail();
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 0),
            _ => panic!("详情弹窗不该被关掉"),
        }
        app.detail_step(1);
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 1),
            _ => panic!("详情弹窗不该被关掉"),
        }
        app.detail_step(1); // 命中的只有两条，不能翻到第三条
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 1),
            _ => panic!("详情弹窗不该被关掉"),
        }
    }

    /// 弹窗里的 Esc 是关弹窗，不能顺手把过滤也取消了。
    #[test]
    fn esc_in_detail_modal_closes_it_instead_of_cancelling_the_filter() {
        let mut app = App::new();
        app.feed_rx(b"data\r\n");
        app.apply_filter("data".into());
        app.focus = Focus::Stream;

        app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(app.modal, Some(Modal::Detail { .. })),
            "数据区焦点下 Enter 应打开详情"
        );

        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none(), "Esc 应关闭详情弹窗");
        assert_eq!(app.filter, "data", "Esc 不该顺手把过滤也取消了");
    }

    /// 弹窗里 Enter 也关弹窗；v 切 HEX；上下键翻记录。
    #[test]
    fn detail_modal_keys_enter_v_and_arrows() {
        let mut app = App::new();
        app.feed_rx(b"one\r\n");
        app.feed_rx(b"two\r\n");
        app.open_detail();

        app.on_key(key(KeyCode::Char('v')));
        match app.modal {
            Some(Modal::Detail { hex, .. }) => assert!(hex, "v 应切到 HEX"),
            _ => panic!("详情弹窗不该被关掉"),
        }

        app.on_key(key(KeyCode::Down));
        match app.modal {
            Some(Modal::Detail { idx, hex }) => {
                assert_eq!(idx, 1, "↓ 应翻到下一条");
                assert!(hex, "翻记录不该把 HEX 模式改回去");
            }
            _ => panic!("详情弹窗不该被关掉"),
        }

        app.on_key(key(KeyCode::Up));
        match app.modal {
            Some(Modal::Detail { idx, .. }) => assert_eq!(idx, 0),
            _ => panic!("详情弹窗不该被关掉"),
        }

        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_none(), "Enter 也应关闭弹窗");
    }

    /// 弹窗里翻记录、切 HEX 都要触发重绘：
    /// 弹窗一直开着，`modal.is_some()` 不会变，光看它界面就停在旧的那一条。
    #[test]
    fn detail_navigation_triggers_a_redraw() {
        let mut app = App::new();
        app.feed_rx(b"one\r\n");
        app.feed_rx(b"two\r\n");
        app.open_detail();

        let a = app.ui_signature();
        app.detail_step(1);
        assert_ne!(app.ui_signature(), a, "弹窗里翻记录必须触发重绘");

        let b = app.ui_signature();
        app.on_key(key(KeyCode::Char('v')));
        assert_ne!(app.ui_signature(), b, "切 HEX 也必须触发重绘");
    }

    // ---------- 过滤命中导航 ----------

    /// n / N 只登记导航请求，实际跳转由渲染层消费。
    #[test]
    fn nav_keys_request_the_next_or_previous_match() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.nav_dir, Some(1), "n = 下一个");
        app.on_key(key(KeyCode::Char('N')));
        assert_eq!(app.nav_dir, Some(-1), "N = 上一个");

        // 输入框里 n 仍然是普通字符
        app.nav_dir = None;
        app.focus = Focus::Input;
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.input, "n", "输入框里 n 是内容");
        assert_eq!(app.nav_dir, None, "输入框里不该登记导航请求");
    }

    // ---------- 自动刷新端口列表 ----------

    /// 自动刷新开关必须进指纹，否则界面上的状态显示会停在旧的样子。
    #[test]
    fn auto_ports_toggle_is_visible_in_the_signature() {
        let mut app = App::new();
        assert!(app.auto_ports, "默认开启自动刷新");
        let a = app.ui_signature();
        app.auto_ports = false;
        assert_ne!(app.ui_signature(), a, "自动刷新开关必须触发重绘");
    }

    /// 自动刷新不能刷日志：列表没变化时一条都不能记。
    ///
    /// 每 5 秒记一条「枚举到 N 个串口」，跑上几小时日志里就只剩它了。
    #[test]
    fn auto_port_refresh_does_not_spam_the_log() {
        let mut app = App::new();
        // 让自动刷新看到的列表与当前一致：结果没变就不该记日志
        app.ports = port::list_ports().unwrap_or_default();
        app.ports_mark = Instant::now() - AUTO_PORTS_INTERVAL - Duration::from_millis(1);
        let before = app.records.len();
        app.tick();
        assert_eq!(app.records.len(), before, "端口列表没变化时不该记日志");
    }

    /// 列表真的变了要记一条：用户得知道设备插上了还是拔掉了。
    #[test]
    fn auto_port_refresh_logs_when_the_list_changes() {
        let Ok(real) = port::list_ports() else { return }; // 环境枚举不了就跳过
        let mut app = App::new();
        // 假装当前列表是错的：真实列表与它不同 → 应该记一条
        app.ports = vec![PortEntry { port: "stui-fake-port".into(), desc: "假的".into() }];
        app.ports_mark = Instant::now() - AUTO_PORTS_INTERVAL - Duration::from_millis(1);
        app.tick();

        let texts: Vec<String> = app
            .records
            .iter()
            .map(|r| String::from_utf8_lossy(&r.data).into_owned())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("串口列表有变化")),
            "列表变了要记一条日志，实际 {texts:?}"
        );
    }

    /// 自动刷新关掉之后就不再枚举（也不会再动端口列表）。
    #[test]
    fn auto_ports_off_stops_refreshing() {
        let mut app = App::new();
        app.auto_ports = false;
        app.ports_mark = Instant::now() - AUTO_PORTS_INTERVAL - Duration::from_millis(1);
        let before = app.ports.len();
        app.tick();
        assert_eq!(app.ports.len(), before, "关掉自动刷新就不该再枚举");
    }

    // ---------- 新状态进指纹 ----------

    /// 新增的可见状态（折行 / 自动刷新 / 导航请求 / 顶行记录）都要进指纹。
    #[test]
    fn ui_signature_notices_the_new_visible_state() {
        let mut app = App::new();

        let a = app.ui_signature();
        app.wrap = true;
        assert_ne!(app.ui_signature(), a, "折行开关必须触发重绘");
        app.wrap = false;

        let b = app.ui_signature();
        app.auto_ports = false;
        assert_ne!(app.ui_signature(), b, "自动刷新开关必须触发重绘");
        app.auto_ports = true;

        let c = app.ui_signature();
        app.nav_dir = Some(1);
        assert_ne!(app.ui_signature(), c, "导航请求必须触发重绘");
        app.nav_dir = Some(-1);
        assert_ne!(app.ui_signature(), c, "上一个与下一个是不同的请求");

        let d = app.ui_signature();
        app.top_record = 3;
        assert_ne!(app.ui_signature(), d, "顶行记录变化必须触发重绘");
    }

    // ---------- 折行行数的缓存 ----------

    /// 折行行数：宽度不变时结果稳定，宽度变了跟着变，追加数据后作废重算。
    ///
    /// 缓存错了的后果很具体：行数算少了滚不到底，算多了底部多出空行。
    #[test]
    fn visual_rows_is_cached_and_follows_width_and_data() {
        let mut app = App::new();
        app.feed_rx(b"aaaa"); // 4 列
        {
            let rec = app.pending_rx.as_ref().expect("当前行应在");
            assert_eq!(rec.visual_rows(10), 1, "4 列在宽度 10 下放得下");
            assert_eq!(rec.visual_rows(10), 1, "同一宽度重复调用结果一致");
            assert_eq!(rec.visual_rows(2), 2, "宽度变了结果跟着变");
        }

        // 追加数据 → 缓存作废，行数变多
        app.feed_rx(b"bb"); // 6 列
        let rec = app.pending_rx.as_ref().expect("当前行应在");
        assert_eq!(rec.visual_rows(2), 3, "追加后行数应增加（缓存已作废）");
        assert_eq!(rec.visual_rows(10), 1, "追加后宽度 10 下仍是一行");
    }

    /// 折行行数必须与「按原始字节数行数」的结果一致。
    ///
    /// 界面走显示文本这条路径，日志与统计走原始字节那条：
    /// 两条路径给出的行数必须一样，否则同一段数据在两处高度不同。
    #[test]
    fn visual_rows_matches_the_byte_level_row_count() {
        let mut app = App::new();
        app.feed_rx("第一行温度传感器数据\r\n第二行".as_bytes());
        app.commit_tx(b"AT\r\n");
        let mut checked = 0;
        for rec in app.stream() {
            for width in [1usize, 2, 3, 5, 8, 40] {
                assert_eq!(
                    rec.visual_rows(width),
                    fmt::wrap_row_count(&rec.data, width),
                    "行数不一致：{:?} @ {width}",
                    String::from_utf8_lossy(&rec.data)
                );
            }
            checked += 1;
        }
        assert!(checked >= 2, "应当至少检查到已封口记录与当前行");
    }

    /// 折行行数要按逻辑行求和：一条记录里有两条逻辑行，就至少占两行。
    #[test]
    fn visual_rows_sums_up_the_logical_lines() {
        let rec = Record::new(Dir::Rx, b"hello\r\nworld");
        assert_eq!(rec.visual_rows(40), 2, "两条逻辑行占两行");
        assert_eq!(rec.visual_rows(3), 4, "每条 5 列的逻辑行在宽度 3 下占 2 行");
    }

    /// 开着折行时，界面每帧都要给每条记录算行高。
    ///
    /// 不缓存的话，每帧的开销随缓冲里的**内容**线性增长：
    /// 缓冲越大越慢，于是开了折行反而更卡。
    /// 缓存之后每条记录只剩一次查表，开销与内容多少无关。
    #[test]
    fn counting_visual_rows_over_a_full_buffer_stays_fast() {
        let mut app = App::new();
        for i in 0..20_000 {
            app.feed_rx(format!("row-{i} payload 温度传感器数据\r\n").as_bytes());
        }
        // 第一遍：算一遍（填缓存）
        let start = Instant::now();
        let mut total = 0usize;
        for rec in app.stream() {
            total += rec.visual_rows(40);
        }
        let first = start.elapsed();
        assert!(total > 0);

        // 第二遍：宽度不变，全部命中缓存 —— 这才是每帧的真实开销
        let start = Instant::now();
        let mut cached_total = 0usize;
        for _ in 0..50 {
            for rec in app.stream() {
                cached_total += rec.visual_rows(40);
            }
        }
        let cached = start.elapsed();
        assert!(cached_total > 0);
        assert!(
            cached < Duration::from_millis(500),
            "50 次全量折行行数统计耗时 {cached:?}，每帧 {} ms",
            cached.as_millis() / 50
        );
        // 缓存命中后不该比第一次还慢（第一次还要建显示缓存）
        assert!(
            cached <= first * 50,
            "缓存命中后耗时 {cached:?} 明显高于首次计算 {first:?}，缓存可能没生效"
        );
    }

    // ---------- 过滤的大小写与反向选项 ----------

    /// 忽略大小写：过滤要能命中大小写不同的同一条记录。
    #[test]
    fn filter_can_ignore_ascii_case() {
        let mut app = App::new();
        app.feed_rx(b"ERROR boot\r\n");
        app.feed_rx(b"error again\r\n");
        app.feed_rx(b"INFO ok\r\n");
        assert_eq!(app.stream_filtered().count(), 3, "不过滤时是全部记录");

        app.apply_filter("error".into());
        assert_eq!(app.stream_filtered().count(), 1, "区分大小写时只命中一条");

        app.toggle_filter_ci();
        assert!(app.filter_ci, "开关要生效");
        assert_eq!(app.stream_filtered().count(), 2, "忽略大小写后两条都命中");

        app.toggle_filter_ci();
        assert_eq!(app.stream_filtered().count(), 1, "关掉后恢复区分大小写");
    }

    /// 中文关键字不受大小写开关影响（中文没有大小写）。
    #[test]
    fn filter_ci_does_not_affect_chinese_keywords() {
        let mut app = App::new();
        app.feed_rx("温度过高\r\n".as_bytes());
        app.feed_rx("INFO 正常\r\n".as_bytes());
        app.apply_filter("温度".into());
        app.toggle_filter_ci();
        assert_eq!(app.stream_filtered().count(), 1, "中文关键字照旧命中");
    }

    /// 反向过滤：命中的变成不命中的。
    #[test]
    fn inverted_filter_excludes_the_matches() {
        let mut app = App::new();
        app.feed_rx(b"ERROR one\r\n");
        app.feed_rx(b"INFO two\r\n");
        app.apply_filter("ERROR".into());
        assert_eq!(app.stream_filtered().count(), 1, "只有 ERROR 那条命中");

        app.toggle_filter_invert();
        assert!(app.filter_invert, "开关要生效");
        let hit: Vec<_> = app.stream_filtered().collect();
        assert_eq!(hit.len(), 1, "反向过滤后只剩原来没命中的那条");
        assert_eq!(hit[0].data, b"INFO two\r\n");

        app.toggle_filter_invert();
        assert_eq!(app.stream_filtered().count(), 1, "关掉后恢复原样");
    }

    /// 过滤为空时两个开关都不改变行为：还是不过滤。
    #[test]
    fn empty_filter_ignores_the_options() {
        let mut app = App::new();
        app.feed_rx(b"one\r\n");
        app.feed_rx(b"two\r\n");
        app.toggle_filter_ci();
        app.toggle_filter_invert();
        app.apply_filter(String::new());
        assert_eq!(app.stream_filtered().count(), 2, "过滤为空就是全显示");
    }

    /// 过滤开关的显示标记：界面的过滤标题要靠它拼出「i」「!」。
    #[test]
    fn filter_flags_show_the_active_options() {
        let mut app = App::new();
        assert_eq!(app.filter_flags(), "", "默认没有任何过滤开关");
        app.toggle_filter_ci();
        assert_eq!(app.filter_flags(), "i");
        app.toggle_filter_invert();
        assert_eq!(app.filter_flags(), "i!");
        app.toggle_filter_ci();
        assert_eq!(app.filter_flags(), "!");
    }

    /// 排除模式的提示条不能说「命中 N 条」—— 那个 N 是**没命中**的条数。
    ///
    /// 断言意图：排除模式下屏幕上显示的是没命中的记录，
    /// 提示条却说「命中 N 条」，用户按这个 N 去数屏幕上的记录只会数错。
    #[test]
    fn invert_filter_toast_counts_hits_and_shown_separately() {
        let mut app = App::new();
        app.feed_rx(b"ERROR one\r\n");
        app.feed_rx(b"ERROR two\r\n");
        app.feed_rx(b"INFO three\r\n");
        app.apply_filter("ERROR".into());
        let msg = app.toast.as_ref().map(|(m, _, _)| m.clone()).unwrap_or_default();
        assert!(msg.contains("命中 2 条"), "正向模式应说命中 2 条：{msg}");

        app.toggle_filter_invert();
        // 排除模式下再应用一次过滤：提示条这时说的是排除模式的话。
        app.apply_filter("ERROR".into());
        let msg = app.toast.as_ref().map(|(m, _, _)| m.clone()).unwrap_or_default();
        assert!(
            msg.contains("已排除 2 条") && msg.contains("显示 1 条"),
            "排除模式应分别说清排除了几条、显示了几条：{msg}"
        );
        assert!(
            !msg.contains("命中 1 条"),
            "排除模式不该把没命中的条数说成命中：{msg}"
        );
    }

    /// 过滤结果是缓存的：数据变了必须重算，不能拿旧值。
    ///
    /// 断言意图：缓存一错就是「过滤了却看不到新数据」或「明明该排除却还在显示」。
    #[test]
    fn filter_cache_follows_data_and_option_changes() {
        let mut app = App::new();
        app.feed_rx(b"ERROR one\r\n");
        app.apply_filter("ERROR".into());
        assert_eq!(app.filtered_count(), 1, "此时命中一条");
        assert_eq!(app.filter_hit_count(), 1, "命中数也是一条");

        // 新来一条命中的：缓存必须跟着更新。
        app.feed_rx(b"ERROR two\r\n");
        assert_eq!(app.filtered_count(), 2, "新数据要立刻算进过滤结果");
        assert_eq!(app.filter_hit_count(), 2, "命中数也要更新");

        // 切到排除模式：显示的是没命中的那些。
        app.toggle_filter_invert();
        assert_eq!(app.filtered_count(), 0, "两条都命中，排除后没有可显示的");
        assert_eq!(app.filter_hit_count(), 2, "命中数仍是两条");

        // 再来一条没命中的：显示数要跟着变。
        app.feed_rx(b"INFO three\r\n");
        assert_eq!(app.filtered_count(), 1, "没命中的那条要显示出来");
        assert_eq!(app.filter_hit_count(), 2, "命中数不受影响");

        // 换关键字：命中集合整个换掉。
        // 排除模式下「命中 INFO」的那条被排除，显示的是两条 ERROR。
        app.apply_filter("INFO".into());
        assert_eq!(app.filtered_count(), 2, "排除模式下显示的是两条 ERROR");
        assert_eq!(app.filter_hit_count(), 1, "命中的是 INFO 那条");

        // 清掉数据：命中数与显示数都要归零。
        app.clear_records();
        assert_eq!(app.filtered_count(), 0, "清空后没有记录");
        assert_eq!(app.filter_hit_count(), 0, "清空后没有命中");
    }

    /// 忽略大小写的高亮区间必须落在字符下标上（界面按它画高亮）。
    #[test]
    fn highlight_ranges_for_ci_filter_are_char_indices() {
        let rec = Record::new(Dir::Rx, "温ERROR error".as_bytes());
        let line = &rec.text_view().lines[0];
        assert_eq!(
            fmt::match_ranges_ignore_ascii_case(line, "error"),
            vec![(1, 6), (7, 12)]
        );
        // 大小写敏感时只有小写那段命中
        assert_eq!(fmt::match_ranges(line, "error"), vec![(7, 12)]);
    }

    /// 两个过滤开关都要进指纹：漏掉任何一个，界面会停在旧的过滤结果上。
    #[test]
    fn ui_signature_notices_the_filter_options() {
        let mut app = App::new();

        let a = app.ui_signature();
        app.filter_ci = true;
        assert_ne!(app.ui_signature(), a, "忽略大小写开关必须触发重绘");
        app.filter_ci = false;

        let b = app.ui_signature();
        app.filter_invert = true;
        assert_ne!(app.ui_signature(), b, "反向过滤开关必须触发重绘");
        app.filter_invert = false;
    }

    /// I 与 ! 两个键切换过滤选项；输入框里它们只是普通字符。
    #[test]
    fn filter_option_keys_work_outside_the_input_box() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('I')));
        assert!(app.filter_ci, "I 应切换忽略大小写");
        app.on_key(key(KeyCode::Char('!')));
        assert!(app.filter_invert, "! 应切换反向过滤");

        app.focus = Focus::Input;
        app.on_key(key(KeyCode::Char('I')));
        assert_eq!(app.input, "I", "输入框里 I 是内容");
        assert!(app.filter_ci, "输入框里不该切开关");
        app.on_key(key(KeyCode::Char('!')));
        assert_eq!(app.input, "I!", "输入框里 ! 是内容");
        assert!(app.filter_invert, "输入框里不该切开关");
    }
}
