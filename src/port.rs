//! 串口抽象层：参数枚举、端口枚举、连接管理与读线程。

use std::io::{self, ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serialport::{
    DataBits, FlowControl as SfFlow, Parity as SfParity, SerialPortType, StopBits as SfStops,
};

/// serialport 4.x 中 `SerialPort` 是 trait，实际句柄是这个装箱类型。
type Port = Box<dyn serialport::SerialPort>;
/// 接收端用 `Arc<Mutex<..>>` 包装：`std::sync::mpsc::Receiver` 不可克隆。
pub type Rx = Arc<Mutex<Receiver<RxMsg>>>;

pub const BAUDS: [u32; 8] = [
    9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600,
];

/// 接收区视图。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Text,
    Hex,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Text => "文本",
            View::Hex => "HEX",
        }
    }
    pub fn toggle(&mut self) {
        *self = match self {
            View::Text => View::Hex,
            View::Hex => View::Text,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bits {
    Five,
    Six,
    Seven,
    Eight,
}

impl Bits {
    pub fn label(self) -> &'static str {
        match self {
            Bits::Five => "5",
            Bits::Six => "6",
            Bits::Seven => "7",
            Bits::Eight => "8",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Bits::Five => Bits::Six,
            Bits::Six => Bits::Seven,
            Bits::Seven => Bits::Eight,
            Bits::Eight => Bits::Five,
        }
    }
    fn to_sf(self) -> DataBits {
        match self {
            Bits::Five => DataBits::Five,
            Bits::Six => DataBits::Six,
            Bits::Seven => DataBits::Seven,
            Bits::Eight => DataBits::Eight,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parity {
    None,
    Odd,
    Even,
}

impl Parity {
    pub fn label(self) -> &'static str {
        match self {
            Parity::None => "无",
            Parity::Odd => "奇",
            Parity::Even => "偶",
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Parity::None => "N",
            Parity::Odd => "O",
            Parity::Even => "E",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Parity::None => Parity::Odd,
            Parity::Odd => Parity::Even,
            Parity::Even => Parity::None,
        }
    }
    fn to_sf(self) -> SfParity {
        match self {
            Parity::None => SfParity::None,
            Parity::Odd => SfParity::Odd,
            Parity::Even => SfParity::Even,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stops {
    One,
    Two,
}

impl Stops {
    pub fn label(self) -> &'static str {
        match self {
            Stops::One => "1",
            Stops::Two => "2",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Stops::One => Stops::Two,
            Stops::Two => Stops::One,
        }
    }
    fn to_sf(self) -> SfStops {
        match self {
            Stops::One => SfStops::One,
            Stops::Two => SfStops::Two,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    None,
    Hardware,
    Software,
}

impl Flow {
    pub fn label(self) -> &'static str {
        match self {
            Flow::None => "无",
            Flow::Hardware => "硬件",
            Flow::Software => "软件",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Flow::None => Flow::Hardware,
            Flow::Hardware => Flow::Software,
            Flow::Software => Flow::None,
        }
    }
    fn to_sf(self) -> SfFlow {
        match self {
            Flow::None => SfFlow::None,
            Flow::Hardware => SfFlow::Hardware,
            Flow::Software => SfFlow::Software,
        }
    }
}

// ------------------------------------------------------------------
// 枚举的稳定字符串编码
//
// 这些编码是配置文件与命令行参数共用的“协议”：一端写进去的值，
// 另一端必须能原样读回来，所以编解码必须严格互逆。
// 取值刻意与 cli.rs 的 --parity/--stops/--flow 写法保持一致，
// 这样同一份字符串既能填进配置文件，也能直接传给命令行。
// ------------------------------------------------------------------

/// 流控的编码：none / hardware / software。
#[allow(dead_code)]
pub fn flow_code(flow: Flow) -> &'static str {
    match flow {
        Flow::None => "none",
        Flow::Hardware => "hardware",
        Flow::Software => "software",
    }
}

/// 解析流控编码；非法值返回 None（调用方决定是回退默认值还是报错）。
#[allow(dead_code)]
pub fn flow_from_code(s: &str) -> Option<Flow> {
    match s.to_ascii_lowercase().as_str() {
        "none" => Some(Flow::None),
        "hardware" => Some(Flow::Hardware),
        "software" => Some(Flow::Software),
        _ => None,
    }
}

/// 校验位的编码：none / odd / even。
#[allow(dead_code)]
pub fn parity_code(parity: Parity) -> &'static str {
    match parity {
        Parity::None => "none",
        Parity::Odd => "odd",
        Parity::Even => "even",
    }
}

/// 解析校验位编码。
#[allow(dead_code)]
pub fn parity_from_code(s: &str) -> Option<Parity> {
    match s.to_ascii_lowercase().as_str() {
        "none" => Some(Parity::None),
        "odd" => Some(Parity::Odd),
        "even" => Some(Parity::Even),
        _ => None,
    }
}

/// 数据位的编码："5".."8"。
#[allow(dead_code)]
pub fn bits_code(bits: Bits) -> &'static str {
    match bits {
        Bits::Five => "5",
        Bits::Six => "6",
        Bits::Seven => "7",
        Bits::Eight => "8",
    }
}

/// 解析数据位编码。
#[allow(dead_code)]
pub fn bits_from_code(s: &str) -> Option<Bits> {
    match s {
        "5" => Some(Bits::Five),
        "6" => Some(Bits::Six),
        "7" => Some(Bits::Seven),
        "8" => Some(Bits::Eight),
        _ => None,
    }
}

/// 停止位的编码："1" / "2"。
#[allow(dead_code)]
pub fn stops_code(stops: Stops) -> &'static str {
    match stops {
        Stops::One => "1",
        Stops::Two => "2",
    }
}

/// 解析停止位编码。
#[allow(dead_code)]
pub fn stops_from_code(s: &str) -> Option<Stops> {
    match s {
        "1" => Some(Stops::One),
        "2" => Some(Stops::Two),
        _ => None,
    }
}

/// 视图的编码：text / hex。
#[allow(dead_code)]
pub fn view_code(view: View) -> &'static str {
    match view {
        View::Text => "text",
        View::Hex => "hex",
    }
}

/// 解析视图编码。
#[allow(dead_code)]
pub fn view_from_code(s: &str) -> Option<View> {
    match s.to_ascii_lowercase().as_str() {
        "text" => Some(View::Text),
        "hex" => Some(View::Hex),
        _ => None,
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PortCfg {
    pub port: String,
    pub baud: u32,
    pub bits: Bits,
    pub parity: Parity,
    pub stops: Stops,
    pub flow: Flow,
}

impl PortCfg {
    /// 紧凑记法，如 `115200 8N1`。
    pub fn summary(&self) -> String {
        format!("{} {}{}{}", self.baud, self.bits.label(), self.parity.code(), self.stops.label())
    }
    pub fn flow_label(&self) -> &'static str {
        self.flow.label()
    }
}

#[derive(Clone, Debug)]
pub struct PortEntry {
    pub port: String,
    pub desc: String,
}

/// 带 USB 厂商 / 产品信息的端口描述。
///
/// `PortEntry` 只有一行描述文本，界面上看不出设备型号；
/// 需要按厂商或产品过滤时用这个结构。
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct PortDetail {
    pub port: String,
    /// 与 `PortEntry::desc` 相同：产品名在前、厂商名在后，中间空格。
    pub desc: String,
    /// USB 厂商名；非 USB 端口或系统未提供时为空串。
    pub manufacturer: String,
    /// USB 产品名；非 USB 端口或系统未提供时为空串。
    pub product: String,
    /// USB 厂商 ID（VID）。
    ///
    /// 非 USB 端口（蓝牙 / PCI / 未知）拿不到，填 `None`；
    /// USB 端口但系统没报出厂商号（值为 0）时同样填 `None`。
    pub vid: Option<u16>,
    /// USB 产品 ID（PID）。
    ///
    /// 规则与 `vid` 相同：拿不到就是 `None`，不要用 0 冒充一个真实编号。
    pub pid: Option<u16>,
}

/// 枚举串口并带上厂商 / 产品信息。
///
/// 与 `list_ports` 走同一套数据源，区别只是多带了两个字段；
/// 排序规则同样让 `COM10` 排在 `COM9` 之后。
pub fn list_ports_detailed() -> io::Result<Vec<PortDetail>> {
    let ports = serialport::available_ports()?;
    let mut out: Vec<PortDetail> = ports
        .into_iter()
        .map(|p| {
            let (desc, manufacturer, product, vid, pid) = match &p.port_type {
                SerialPortType::UsbPort(u) => {
                    // 系统有时给出空串占位，统一当成“没有信息”处理；
                    // 拼接规则与旧版 list_ports 完全一致：产品名在前、厂商名在后。
                    let product = u
                        .product
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default();
                    let manufacturer = u
                        .manufacturer
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_default();
                    let desc = match (product.is_empty(), manufacturer.is_empty()) {
                        (true, true) => String::new(),
                        (true, false) => manufacturer.clone(),
                        (false, true) => product.clone(),
                        (false, false) => format!("{product} {manufacturer}"),
                    };
                    // serialport 用 0 表示“系统没报出这个编号”，
                    // 0x0000 不是真实存在的 VID/PID，所以翻译成 None，
                    // 让调用方能区分“没拿到”和“拿到了 0”。
                    let vid = (u.vid != 0).then_some(u.vid);
                    let pid = (u.pid != 0).then_some(u.pid);
                    (desc, manufacturer, product, vid, pid)
                }
                SerialPortType::BluetoothPort => {
                    ("蓝牙串口".to_string(), String::new(), String::new(), None, None)
                }
                SerialPortType::PciPort => {
                    ("PCI 串口".to_string(), String::new(), String::new(), None, None)
                }
                SerialPortType::Unknown => {
                    (String::new(), String::new(), String::new(), None, None)
                }
            };
            PortDetail { port: p.port_name, desc, manufacturer, product, vid, pid }
        })
        .collect();
    out.sort_by(|a, b| nat_cmp(&a.port, &b.port));
    Ok(out)
}

/// 枚举串口。
///
/// 保留原有的签名与行为（其它模块正在调用），实现改为复用
/// `list_ports_detailed`，避免两份端口描述规则各自漂移。
pub fn list_ports() -> io::Result<Vec<PortEntry>> {
    Ok(list_ports_detailed()?
        .into_iter()
        .map(|d| PortEntry { port: d.port, desc: d.desc })
        .collect())
}

/// 把“打开串口失败”翻译成一句能照着做的中文提示。
///
/// 裸的 io::Error 文案对使用者没有指导意义：
/// 权限拒绝几乎总是“另一个串口助手已经打开了这个端口”，
/// 而 NotFound 多数是“设备被拔出、端口号变了”。
#[allow(dead_code)]
pub fn describe_open_error(port: &str, e: &io::Error) -> String {
    match e.kind() {
        ErrorKind::PermissionDenied => format!(
            "无法打开串口 {port}：权限被拒绝。\
             串口多半正被其它程序占用（另一个串口助手、烧录工具或终端已经打开了它），\
             请先关闭那些程序再重试。"
        ),
        ErrorKind::NotFound => format!(
            "无法打开串口 {port}：设备不存在。\
             端口可能已被拔出、重新插拔后编号变了，或当前用户没有访问权限；\
             请重新插拔设备并刷新端口列表后再试。"
        ),
        ErrorKind::TimedOut => format!(
            "无法打开串口 {port}：等待设备响应超时。\
             设备可能已掉线、被其它程序独占，或驱动状态异常；\
             请检查设备连接与驱动后重试。"
        ),
        _ => format!(
            "无法打开串口 {port}：{e}。\
             请检查设备连接是否正常、驱动是否已安装。"
        ),
    }
}

/// 让 `COM10` 排在 `COM9` 之后，而不是字符串序。
fn nat_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let head: String = s.chars().take_while(|c| !c.is_ascii_digit()).collect();
        let digits: String = s.chars().filter(char::is_ascii_digit).collect();
        (head, digits)
    };
    let (ha, da) = split(a);
    let (hb, db) = split(b);
    ha.cmp(&hb).then_with(|| da.parse::<u64>().unwrap_or(0).cmp(&db.parse::<u64>().unwrap_or(0)))
}

pub enum RxMsg {
    Data(Vec<u8>),
    Err(String),
    Closed,
}

/// 连续读到 0 字节多少次就判定对端已经关闭。
///
/// 0 字节在 POSIX 上就是 EOF（设备被拔掉、对端程序退出），
/// Windows 上读超时会返回 Err(TimedOut) 而不是 0 字节，
/// 所以连续这么多次空读基本只可能是端口真的没了。
/// 阈值取 100：免得偶发的一次空读就误报断开。
///
/// **阈值是「次数」不是「时间」**：每一轮除了 10 毫秒的退避，
/// 还要等满串口的读超时（`Link::open` 里设的 100 毫秒），
/// 所以 100 轮大约是 11 秒 —— 比「一秒多」慢一个数量级，
/// 调参时别拿秒来估算这个阈值。
const MAX_ZERO_READS: u32 = 100;

/// 连续空读的计数器。
///
/// 读不到任何字节（`Ok(0)`）在 POSIX 上就是 EOF，
/// 连续这么多次说明端口真的没了；
/// 中间只要来过一次数据，之前的空读就不算数（端口还活着）。
struct ZeroReads {
    count: u32,
}

impl ZeroReads {
    fn new() -> Self {
        ZeroReads { count: 0 }
    }

    /// 记一次空读；返回 true 表示连续空读已超过阈值，该宣布对端关闭了。
    fn note_zero(&mut self) -> bool {
        self.count = self.count.saturating_add(1);
        self.count > MAX_ZERO_READS
    }

    /// 真的来了数据：之前的空读作废。
    fn note_data(&mut self) {
        self.count = 0;
    }
}

/// 清空输入缓冲时的请求 / 回执。
///
/// 请求与执行分处两个线程：调用方在主线程说「清一下」，
/// 真正丢字节的必须是读线程 —— 只有它持有串口的读句柄，
/// 另开一个句柄去读会和读线程抢字节，表现是「明明清了，界面里还冒出一截旧数据」。
///
/// 状态用一把锁串起来，而不是用几个原子量互相配对：
/// 请求与回执必须成对出现，用锁把它们串成一条链比用原子量对拍更不容易出错，
/// 而且这里每秒最多被访问几十次，锁的开销可以忽略。
struct DrainCtl {
    inner: Mutex<DrainInner>,
}

#[derive(Default)]
struct DrainInner {
    /// 已经发出的请求计数。
    req: u64,
    /// 已经完成的请求计数（完成到 `done` 为止的请求都算做完了）。
    done: u64,
    /// 最近一次清空的结果。
    ok: bool,
}

#[allow(dead_code)]
impl DrainCtl {
    fn new() -> Self {
        DrainCtl { inner: Mutex::new(DrainInner::default()) }
    }

    /// 发起一次清空请求，返回这次请求的编号。
    fn request(&self) -> Option<u64> {
        let mut g = self.inner.lock().ok()?;
        g.req += 1;
        Some(g.req)
    }

    /// 是否还有没做的请求。
    ///
    /// 锁中毒（某个线程 panic 时把锁带坏了）时一律当作“没有请求”：
    /// 这时候连接本身已经乱了，继续处理请求没有意义。
    fn pending(&self) -> bool {
        match self.inner.lock() {
            Ok(g) => g.done < g.req,
            Err(_) => false,
        }
    }

    /// 认领一次清空请求；返回「本次清空覆盖到的请求计数」。
    ///
    /// 没有待处理的请求时返回 None。
    fn begin(&self) -> Option<u64> {
        let g = self.inner.lock().ok()?;
        if g.done >= g.req {
            return None;
        }
        Some(g.req)
    }

    /// 回报清空结果：完成到 `seq` 为止的请求。
    ///
    /// 只完成到 `seq` 是因为清空期间可能又来了新请求，
    /// 那次请求对应的是清空之后的状态，不能拿这次的结果顶替。
    fn complete(&self, seq: u64, ok: bool) {
        if let Ok(mut g) = self.inner.lock() {
            g.ok = ok;
            g.done = seq;
        }
    }

    /// 等某次请求完成并取回它的结果；等到截止时间还没完成就返回 false。
    fn wait(&self, seq: u64, deadline: Instant) -> bool {
        loop {
            {
                // 锁中毒说明状态已经不可信，直接放弃。
                let Ok(g) = self.inner.lock() else { return false };
                if g.done >= seq {
                    return g.ok;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            // 读线程最多一个读超时（100 毫秒）就会醒过来，
            // 这里轮询的间隔取得比它小一个量级就够了。
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// 等读线程完成一次清空的耐心上限。
///
/// 读线程阻塞在 read() 里时，最多要等一个读超时（100 毫秒）才会醒过来看一眼请求；
/// 清空本身还要再读到一次「没有数据」（又是一个读超时）。
/// 正常情况下 300 毫秒之内就有结果，这里给 1 秒的余量。
#[allow(dead_code)]
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// 清空时最多丢掉多少字节。
///
/// 设备持续吐数据时「清空」是没有终点的：丢到这个上限就收手，
/// 剩下的当作正常数据交给上层显示 —— 强求「清干净」只会把界面卡住。
const DRAIN_MAX_BYTES: usize = 64 * 1024;

/// 清空时最多读多少轮。
///
/// 与上面的字节上限是一道保险，防止每轮只读到几个字节时永远丢不完。
const DRAIN_MAX_ROUNDS: usize = 32;

/// 把驱动里积压的输入读出来丢掉。
///
/// 返回 true 表示「已经没有可丢的数据了」，
/// false 表示没清成（读出错，或设备一直在吐数据、丢到上限还没见底）。
///
/// 三种停止条件里前两种算成功：
/// - 读到 0 字节：没有更多数据了（POSIX 上读超时也是 0 字节）。
/// - 读超时：驱动里确实没有数据了（Windows 上超时报这个错）。
/// - 丢到上限：设备一直在吐数据，如实报告失败 —— 不能保证清干净了。
fn drain_reader<R: Read + ?Sized>(r: &mut R, scratch: &mut [u8]) -> bool {
    let mut dropped = 0usize;
    for _ in 0..DRAIN_MAX_ROUNDS {
        match r.read(scratch) {
            Ok(0) => return true,
            Ok(n) => {
                dropped += n;
                if dropped >= DRAIN_MAX_BYTES {
                    return false;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => return true,
            // 其它读错误：设备多半被拔了或者出问题了，如实报告失败。
            Err(_) => return false,
        }
    }
    false
}

/// 一条已打开的连接。写句柄留在主线程，读循环跑在独立线程。
pub struct Link {
    pub cfg: PortCfg,
    pub rx: Rx,
    writer: Option<Port>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// 清空输入缓冲的请求 / 回执通道。
    #[allow(dead_code)]
    drain: Arc<DrainCtl>,
}

impl Link {
    pub fn open(cfg: PortCfg) -> io::Result<Link> {
        let port = serialport::new(&cfg.port, cfg.baud)
            .data_bits(cfg.bits.to_sf())
            .parity(cfg.parity.to_sf())
            .stop_bits(cfg.stops.to_sf())
            .flow_control(cfg.flow.to_sf())
            .timeout(Duration::from_millis(100))
            .open()?;
        let writer = port.try_clone()?;
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        // 清空请求的通道：主线程发起，读线程执行。
        let drain = Arc::new(DrainCtl::new());
        let drain_worker = Arc::clone(&drain);

        let worker = std::thread::Builder::new()
            .name("serial-rx".into())
            .spawn(move || {
                let mut port = port;
                // 接收缓冲长期存活：循环里复用同一块内存，不再每轮重新准备；
                // 读到的字节只把真正需要的 n 个复制出去。
                // 注意 RxMsg::Data 拥有字节所有权，交出去之后没法收回，
                // 因此这份拷贝无法省掉，能省的是读缓冲本身的重复分配。
                let mut scratch = vec![0u8; 8192];
                // 连续空读的计数：用来判断对端是不是真的把端口关了。
                let mut zeros = ZeroReads::new();
                // 是否已经通知过“连接已关闭”：避免重复发两条 Closed。
                let mut closed_sent = false;
                while !flag.load(Ordering::Relaxed) {
                    // 先看有没有“清空输入缓冲”的请求：
                    // 必须由本线程执行 —— 只有它持有串口的读句柄。
                    if let Some(seq) = drain_worker.begin() {
                        let ok = drain_reader(&mut port, &mut scratch);
                        drain_worker.complete(seq, ok);
                        continue;
                    }
                    match port.read(&mut scratch) {
                        // 读到 0 字节：拿不到任何数据。
                        // 直接 continue 会让读线程空转吃满一个核，
                        // 这里退避一下再继续。
                        Ok(0) => {
                            // 连续 0 字节说明对端已经关闭了这条连接
                            // （设备被拔掉 / 对端程序退出）。
                            // 不主动上报的话，上层只能一直显示“已连接”，
                            // 要等到别的超时机制才发现端口没了。
                            if zeros.note_zero() {
                                let _ = tx.send(RxMsg::Closed);
                                closed_sent = true;
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        Ok(n) => {
                            // 真的来了数据：之前那一串空读不算数，计数清零。
                            zeros.note_data();
                            // 读这批数据的同时有人请求了清空：
                            // 这批数据正是要清掉的残留，直接丢掉，
                            // 别让它混进接收区污染下一次会话。
                            if drain_worker.pending() {
                                continue;
                            }
                            if tx.send(RxMsg::Data(scratch[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                            // 超时说明设备还活着（只是暂时没数据），
                            // 因此不打断“连续空读”的计数，但也不增加它。
                            continue;
                        }
                        Err(e) => {
                            let _ = tx.send(RxMsg::Err(e.to_string()));
                            break;
                        }
                    }
                }
                if !closed_sent {
                    let _ = tx.send(RxMsg::Closed);
                }
            })?;

        Ok(Link {
            cfg,
            rx: Arc::new(Mutex::new(rx)),
            writer: Some(writer),
            stop,
            worker: Some(worker),
            drain,
        })
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "串口已关闭"))?;
        w.write_all(data)?;
        w.flush()
    }

    /// 读线程是否还活着。
    ///
    /// 串口被拔出时读线程会自己结束（并先发一条 Err），
    /// 界面据此判断“连接还在不在”，而不是靠超时猜测。
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        match &self.worker {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }

    /// 停止读线程并释放两个句柄；幂等。
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.writer.take() {
            drop(w);
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }

    /// 清掉串口驱动里已经积压的输入字节。
    ///
    /// 设备之前吐过来的残留数据会污染下一次会话
    /// （上一条命令的尾巴、上电时的启动 banner……），调试时很烦，
    /// 所以串口助手基本都有「清空输入缓冲」这个动作。
    ///
    /// 真正的清理由**读线程**执行：只有它持有串口的读句柄，
    /// 另开一个句柄去读会和读线程抢字节，结果不可预期
    /// （表现是「明明清了，界面里还冒出来一截旧数据」）。
    ///
    /// 返回值：
    /// - `true`：积压的数据已经被丢掉（或者本来就没有数据可丢）。
    /// - `false`：没清成 —— 连接已经关闭、读线程已经不在、
    ///   读操作出错，或者设备一直在吐数据、丢到上限还没见底。
    ///
    /// **清缓冲失败不该让整条链路失败**：这里不返回错误，
    /// 只把结果告诉调用方，由调用方决定要不要提示用户。
    /// 残留数据顶多是界面里多几行旧数据，不影响后续收发。
    ///
    /// 代价：调用方最多要等 [`DRAIN_TIMEOUT`]（1 秒），
    /// 因为读线程阻塞在 read() 里时要等一个读超时（100 毫秒）才会醒过来看一眼请求，
    /// 清空本身还要再读到一次「没有数据」。
    /// 正常情况下 300 毫秒之内就有结果；要保证界面不卡就别在刷新线程里同步调用它。
    #[allow(dead_code)]
    pub fn drain_input(&mut self) -> bool {
        // 读线程已经结束（端口被拔、连接被关）：没人会来做这件事，
        // 直接告诉调用方失败，不要让它白等一秒。
        if !self.is_alive() {
            return false;
        }
        let Some(seq) = self.drain.request() else {
            return false;
        };
        self.drain.wait(seq, Instant::now() + DRAIN_TIMEOUT)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编解码必须严格互逆：编出来的字符串要能解回原来的枚举。
    #[test]
    fn enum_codes_roundtrip() {
        for f in [Flow::None, Flow::Hardware, Flow::Software] {
            assert_eq!(flow_from_code(flow_code(f)), Some(f));
        }
        for p in [Parity::None, Parity::Odd, Parity::Even] {
            assert_eq!(parity_from_code(parity_code(p)), Some(p));
        }
        for b in [Bits::Five, Bits::Six, Bits::Seven, Bits::Eight] {
            assert_eq!(bits_from_code(bits_code(b)), Some(b));
        }
        for s in [Stops::One, Stops::Two] {
            assert_eq!(stops_from_code(stops_code(s)), Some(s));
        }
        for v in [View::Text, View::Hex] {
            // View 没有实现 Debug，用 assert! 而不是 assert_eq!。
            assert!(view_from_code(view_code(v)) == Some(v));
        }
    }

    /// 取值必须与 cli.rs 的 --parity/--stops/--flow 写法一致，
    /// 这样同一个字符串既能进配置文件也能直接喂给命令行。
    #[test]
    fn codes_match_cli_syntax() {
        assert_eq!(parity_from_code("none"), Some(Parity::None));
        assert_eq!(parity_from_code("odd"), Some(Parity::Odd));
        assert_eq!(parity_from_code("even"), Some(Parity::Even));
        assert_eq!(stops_from_code("1"), Some(Stops::One));
        assert_eq!(stops_from_code("2"), Some(Stops::Two));
        assert_eq!(flow_from_code("none"), Some(Flow::None));
        assert_eq!(flow_from_code("hardware"), Some(Flow::Hardware));
        assert_eq!(flow_from_code("software"), Some(Flow::Software));
        for b in ["5", "6", "7", "8"] {
            assert!(bits_from_code(b).is_some(), "--bits 应当接受 {b}");
        }
        // 非法值必须返回 None，而不是悄悄解析成默认值。
        assert_eq!(parity_from_code("x"), None);
        assert_eq!(stops_from_code("3"), None);
        assert_eq!(flow_from_code("rts"), None);
        assert_eq!(bits_from_code("9"), None);
        assert!(view_from_code("bin").is_none());
    }

    /// 错误提示里必须带上端口名，并区分不同的失败原因。
    #[test]
    fn open_error_message_mentions_port_and_reason() {
        let denied = describe_open_error(
            "COM3",
            &io::Error::new(ErrorKind::PermissionDenied, "boom"),
        );
        assert!(denied.contains("COM3"), "提示里要带上端口名：{denied}");
        assert!(denied.contains("占用"), "权限拒绝应当提示可能被占用：{denied}");

        let missing = describe_open_error(
            "COM9",
            &io::Error::new(ErrorKind::NotFound, "boom"),
        );
        assert!(missing.contains("COM9"), "提示里要带上端口名：{missing}");
        assert!(missing.contains("不存在"), "找不到设备应当说明设备不存在：{missing}");

        let timeout = describe_open_error(
            "COM4",
            &io::Error::new(ErrorKind::TimedOut, "boom"),
        );
        assert!(timeout.contains("COM4"));
        assert!(timeout.contains("超时"), "超时要说明是超时：{timeout}");

        let other = describe_open_error(
            "COM5",
            &io::Error::new(ErrorKind::Other, "weird"),
        );
        assert!(other.contains("COM5"));
        assert!(other.contains("weird"), "未知错误要保留原始信息：{other}");
    }

    /// 连续空读要够多才判定对端关闭，少量空读不能误报。
    #[test]
    fn zero_reads_trip_only_after_the_threshold() {
        let mut z = ZeroReads::new();
        for i in 0..MAX_ZERO_READS {
            assert!(!z.note_zero(), "第 {i} 次空读还不该判定关闭");
        }
        assert!(z.note_zero(), "超过阈值之后应当判定为对端已关闭");
    }

    /// 中间来过数据就说明端口还活着：之前的空读要作废。
    #[test]
    fn zero_reads_reset_after_data() {
        let mut z = ZeroReads::new();
        for _ in 0..MAX_ZERO_READS {
            z.note_zero();
        }
        z.note_data();
        assert!(!z.note_zero(), "来过数据之后计数要重新开始");
    }

    // --------------------------------------------------------------
    // 清空输入缓冲
    // --------------------------------------------------------------

    /// 假串口：按剧本回应读请求，用来验证清空逻辑而不需要真的串口设备。
    struct FakePort {
        /// 还没吐出来的数据块。
        pending: Vec<Vec<u8>>,
        /// 数据吐完之后读操作的结局。
        end: FakeEnd,
    }

    #[derive(Clone, Copy)]
    enum FakeEnd {
        /// 读到 0 字节：没有更多数据了（POSIX 上读超时也是这个）。
        Eof,
        /// 读超时：驱动里没有数据（Windows 上超时报这个错）。
        Timeout,
        /// 读失败：设备出问题了。
        Broken,
        /// 永远有数据：模拟一个一直在吐的设备。
        Flood,
    }

    impl Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pending.is_empty() {
                return match self.end {
                    FakeEnd::Eof => Ok(0),
                    FakeEnd::Timeout => Err(io::Error::new(ErrorKind::TimedOut, "超时")),
                    FakeEnd::Broken => Err(io::Error::new(ErrorKind::Other, "设备掉了")),
                    FakeEnd::Flood => {
                        for b in buf.iter_mut() {
                            *b = 0xAA;
                        }
                        Ok(buf.len())
                    }
                };
            }
            let chunk = self.pending.remove(0);
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            Ok(n)
        }
    }

    /// 积压的数据必须被真的丢掉（而不是只读了一次就收手）。
    #[test]
    fn drain_reader_discards_all_pending_bytes() {
        let mut port = FakePort {
            pending: vec!["上一条命令的尾巴".as_bytes().to_vec(), "上电时的 banner".as_bytes().to_vec()],
            end: FakeEnd::Eof,
        };
        let mut scratch = vec![0u8; 8192];
        assert!(drain_reader(&mut port, &mut scratch), "清空应当成功");
        assert!(port.pending.is_empty(), "积压的数据应当被读干净：{:?}", port.pending);
    }

    /// 缓冲本来就是空的也算清空成功。
    #[test]
    fn drain_reader_on_empty_buffer_succeeds() {
        let mut port = FakePort { pending: vec![], end: FakeEnd::Eof };
        let mut scratch = vec![0u8; 8192];
        assert!(drain_reader(&mut port, &mut scratch), "没有数据可丢也算成功");

        let mut port = FakePort { pending: vec![], end: FakeEnd::Timeout };
        assert!(drain_reader(&mut port, &mut scratch), "读超时说明缓冲是空的，算成功");
    }

    /// 读失败必须如实报告失败，不能假装清空了。
    #[test]
    fn drain_reader_reports_read_failure() {
        let mut port = FakePort { pending: vec![], end: FakeEnd::Broken };
        let mut scratch = vec![0u8; 8192];
        assert!(!drain_reader(&mut port, &mut scratch), "读失败应当报告失败");
    }

    /// 设备一直在吐数据时不能无限期地丢下去。
    #[test]
    fn drain_reader_gives_up_on_a_flooding_device() {
        let mut port = FakePort { pending: vec![], end: FakeEnd::Flood };
        let mut scratch = vec![0u8; 8192];
        assert!(!drain_reader(&mut port, &mut scratch), "丢到上限还没见底应当报告失败");
    }

    /// 请求 / 回执的握手：读线程认领请求并回报结果。
    #[test]
    fn drain_request_is_answered_by_the_reader_thread() {
        let ctl = Arc::new(DrainCtl::new());
        let worker_ctl = Arc::clone(&ctl);
        let h = std::thread::spawn(move || {
            // 模拟读线程：等请求出现，认领它，回报结果。
            for _ in 0..1000 {
                if let Some(seq) = worker_ctl.begin() {
                    worker_ctl.complete(seq, true);
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        let seq = ctl.request().expect("请求应当被接受");
        assert!(ctl.wait(seq, Instant::now() + Duration::from_secs(2)), "应当等到读线程的回报");
        h.join().unwrap();
    }

    /// 读线程回报失败时，调用方拿到的也必须是失败。
    #[test]
    fn drain_request_reports_a_failed_drain() {
        let ctl = Arc::new(DrainCtl::new());
        let worker_ctl = Arc::clone(&ctl);
        let h = std::thread::spawn(move || {
            for _ in 0..1000 {
                if let Some(seq) = worker_ctl.begin() {
                    worker_ctl.complete(seq, false);
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        let seq = ctl.request().expect("请求应当被接受");
        assert!(!ctl.wait(seq, Instant::now() + Duration::from_secs(2)), "失败的清空应当如实报告");
        h.join().unwrap();
    }

    /// 读线程一直没回应时要按时放弃，而不是永远等下去。
    #[test]
    fn drain_request_times_out_when_nobody_answers() {
        let ctl = DrainCtl::new();
        let seq = ctl.request().expect("请求应当被接受");
        assert!(!ctl.wait(seq, Instant::now() + Duration::from_millis(50)), "等不到回应应当报告失败");
    }

    /// 清空请求到达之前不应被当成已完成。
    #[test]
    fn drain_request_stays_pending_until_completed() {
        let ctl = DrainCtl::new();
        assert!(!ctl.pending(), "没有请求时不应有挂起的清空");
        let seq = ctl.request().expect("请求应当被接受");
        assert!(ctl.pending(), "请求发出后应当是挂起状态");
        assert!(ctl.begin().is_some(), "应当能认领这次请求");
        ctl.complete(seq, true);
        assert!(!ctl.pending(), "完成之后不应再有挂起的清空");
    }
}
