//! 纯函数工具：时间戳、字节可视化、发送内容解析。

// 本模块是一组纯函数，公开 API 里有不少只被测试或界面层零星调用；
// bin 目标下没人调用的那些会报 dead_code，这里统一放行。
#![allow(dead_code)]

use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

/// 折叠后的转义序列标记（`ESC [` → `^[`）。固定 3 列，行宽才不会随转义内容乱跳。
const ESC_MARK: &[u8] = b"^[m";

/// 「自 Unix 纪元起的天数」→ 公历 `(年, 月, 日)`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

pub struct Stamp {
    pub y: i64,
    pub mo: u32,
    pub d: u32,
    pub h: u32,
    pub mi: u32,
    pub s: u32,
    pub ms: u32,
}

impl Stamp {
    pub fn hms_ms(&self) -> String {
        format!("{:02}:{:02}:{:02}.{:03}", self.h, self.mi, self.s, self.ms)
    }
    pub fn full(&self) -> String {
        format!("{}-{:02}-{:02} {}", self.y, self.mo, self.d, self.hms_ms())
    }
    pub fn compact(&self) -> String {
        self.compact_base()
    }
    /// 紧凑到毫秒：`20261022-035812.123`。
    ///
    /// 默认文件名用它：只到秒的话，同一秒里跑两次就会撞成同一个文件。
    pub fn compact_ms(&self) -> String {
        // 与 compact 同一份格式串：两处各写一遍，改了秒位忘了改毫秒位就
        // 会输出两种长度的文件名。
        format!("{}.{:03}", self.compact_base(), self.ms)
    }
    /// 紧凑时间戳的公共部分：`20261022-035812`。
    fn compact_base(&self) -> String {
        format!("{:04}{:02}{:02}-{:02}{:02}{:02}", self.y, self.mo, self.d, self.h, self.mi, self.s)
    }
}

pub fn stamp(t: SystemTime) -> Stamp {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, mo, dy) = civil_from_days(days);
    Stamp {
        y,
        mo,
        d: dy,
        h: (sod / 3_600) as u32,
        mi: (sod % 3_600 / 60) as u32,
        s: (sod % 60) as u32,
        ms: d.subsec_millis(),
    }
}

pub fn now_hms_ms() -> String {
    stamp(SystemTime::now()).hms_ms()
}

/// 整条数据的解码结果：每段都是「合法 UTF-8 字符」或「非法字节」。
///
/// 串口数据一律按 UTF-8 解读：能解码的（含中文、emoji）按原字符显示，
/// 只有真正非法的字节才降级为 `·`，因此显示宽度可能不等于字节数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    /// 合法 UTF-8 文本片段
    Text(String),
    /// 非法的原始字节
    Raw(u8),
}

/// 把字节流解码成显示单元序列，遇到非法字节逐个降级。
///
/// 与 [`for_each_atom`] 同源：原子是字符级的，单元是文本段级的，
/// 非法字节会把文本段切开，前后各自成段。
pub fn decode_units(data: &[u8]) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    let mut text = String::new();
    for a in scan_atoms(data) {
        match a.kind {
            AtomKind::Raw => {
                if !text.is_empty() {
                    units.push(Unit::Text(std::mem::take(&mut text)));
                }
                units.push(Unit::Raw(data[a.start]));
            }
            AtomKind::Plain(c) | AtomKind::Ctrl(c) => text.push(c),
        }
    }
    if !text.is_empty() {
        units.push(Unit::Text(text));
    }
    units
}

/// 是否按 `^X` 形式显示（控制字符）。此类字符占 2 列。
fn is_control_like(c: char) -> bool {
    let v = c as u32;
    v < 0x20 || v == 0x7F
}

/// 单个字符的显示列宽：控制符 2 列，中日韩全角 2 列，其余按 unicode-width。
fn char_width(c: char) -> usize {
    if is_control_like(c) {
        2
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

/// 单个显示单元的终端列宽（中文/全角算 2 列）。
pub fn unit_width(u: &Unit) -> usize {
    match u {
        // 文本单元的列宽就是逐字符的显示列宽之和，与 display_cols 同一份规则
        Unit::Text(s) => display_cols(s),
        Unit::Raw(_) => 1,
    }
}

/// 单元的显示文本：控制符用 `^M` 形式，非法字节用 `·`。
pub fn unit_text(u: &Unit, out: &mut String) {
    match u {
        Unit::Text(s) => push_text(s, out),
        Unit::Raw(_) => out.push('·'),
    }
}

/// 控制字符的显示形式：`^X`（去掉控制位后补上对应的大写字母）。
///
/// `0x0D` → `^M`。单独抽出来是因为「按文本渲染」与「按原子渲染」
/// 两处都要用它：写两遍就可能只改一处，于是同一段数据有两个显示形态。
fn push_ctrl(c: char, out: &mut String) {
    out.push('^');
    out.push(char::from_u32((c as u32) ^ 0x40).unwrap_or('?'));
}

/// 把合法文本按控制符规则写入显示串：`\r` → `^M`。
fn push_text(s: &str, out: &mut String) {
    for c in s.chars() {
        if is_control_like(c) {
            push_ctrl(c, out);
        } else {
            out.push(c);
        }
    }
}

/// 单个显示原子的形态。
#[derive(Debug, Clone, Copy)]
enum AtomKind {
    /// 普通字符：原样显示
    Plain(char),
    /// 控制字符：显示成 `^X`（占 2 列）
    Ctrl(char),
    /// 非法字节：显示成 `·`（占 1 列）
    Raw,
}

/// 显示原子：字节区间 + 终端列宽 + 显示形态。
///
/// 一次解码就同时给出「切分边界」「占几列」「显示成什么」三样东西，
/// 截断时不必再把数据解一遍。
#[derive(Debug, Clone, Copy)]
struct Atom {
    /// 在原始数据里的字节起点
    start: usize,
    /// 字节终点（不含）
    end: usize,
    /// 终端列宽
    width: usize,
    kind: AtomKind,
}

impl Atom {
    /// 把这个原子写进显示串。
    fn push_text(&self, out: &mut String) {
        match self.kind {
            AtomKind::Plain(c) => out.push(c),
            AtomKind::Ctrl(c) => push_ctrl(c, out),
            AtomKind::Raw => out.push('·'),
        }
    }
}

/// 遍历一段数据里的全部显示原子（单次解码）。
///
/// 合法 UTF-8 按字符拆开（一个文本段里可能混着换行等控制符），
/// 非法字节各自成原子；因此按它切行或截断都不会劈开宽字符。
fn for_each_atom(data: &[u8], mut f: impl FnMut(Atom)) {
    let mut i = 0usize;
    while i < data.len() {
        match std::str::from_utf8(&data[i..]) {
            Ok(s) => {
                push_text_atoms(s, i, &mut f);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    // 合法前缀必然由完整字符组成，直接按字符切
                    let s = std::str::from_utf8(&data[i..i + valid]).expect("合法前缀");
                    push_text_atoms(s, i, &mut f);
                    i += valid;
                    continue;
                }
                match e.error_len() {
                    Some(len) => {
                        for k in i..i + len {
                            f(Atom { start: k, end: k + 1, width: 1, kind: AtomKind::Raw });
                        }
                        i += len;
                    }
                    // 末尾是不完整的多字节序列：逐字节降级
                    None => {
                        for k in i..data.len() {
                            f(Atom { start: k, end: k + 1, width: 1, kind: AtomKind::Raw });
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// 把一段合法文本拆成显示原子（`base` 是它在原始数据里的字节起点）。
fn push_text_atoms(s: &str, base: usize, f: &mut impl FnMut(Atom)) {
    for (ci, c) in s.char_indices() {
        let start = base + ci;
        let (kind, width) = if is_control_like(c) {
            (AtomKind::Ctrl(c), 2)
        } else {
            (AtomKind::Plain(c), UnicodeWidthChar::width(c).unwrap_or(0))
        };
        f(Atom { start, end: start + c.len_utf8(), width, kind });
    }
}

/// 收集全部显示原子（需要按顺序回头看时用）。
fn scan_atoms(data: &[u8]) -> Vec<Atom> {
    let mut atoms = Vec::new();
    for_each_atom(data, |a| atoms.push(a));
    atoms
}

/// 折叠终端转义序列：CSI / OSC / 双字符序列一律压成 `^[m`（固定 3 列）。
///
/// Python、Node、ESP32 固件之类的设备常发带颜色的日志（`\x1b[31m…\x1b[0m`）。
/// 不处理的话，界面上就是一串 `^[[31m` 垃圾，既占地方又看不出是哪来的；
/// 折叠成 `^[m` 之后一眼就知道"这里有过转义"，列宽也稳定了。
///
/// 数据里没有 `ESC` 时直接借用原切片，常见路径零分配。
/// 想看原始字节可以切 HEX 视图，那里永远是未折叠的真值。
pub fn fold_ansi(data: &[u8]) -> Cow<'_, [u8]> {
    if !data.contains(&0x1B) {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1B {
            out.push(data[i]);
            i += 1;
            continue;
        }
        i += 1; // 吃掉 ESC
        match data.get(i) {
            None => {
                // 数据在 ESC 处被切断：不能留下半个标记误导读者
                out.extend_from_slice(ESC_MARK);
                break;
            }
            Some(&b'[') => {
                // CSI：参数区结束后是一个最终字节 0x40..=0x7E
                i += 1;
                while i < data.len() && !(0x40..=0x7E).contains(&data[i]) {
                    i += 1;
                }
                if i < data.len() {
                    i += 1; // 最终字节本身也属于序列
                }
                out.extend_from_slice(ESC_MARK);
            }
            Some(&b']') => {
                // OSC：一直到 BEL 或 ST（ESC \）才结束，中间的标题可以很长
                i += 1;
                while i < data.len() {
                    if data[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if data[i] == 0x1B && data.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(ESC_MARK);
            }
            Some(_) => {
                // 其余为 ESC Fe 序列：中间字节 0x20..=0x2F，最终字节 0x30..=0x7E。
                // 例如 `ESC ( B` 是三字节的字符集指定，只吞两个会把 'B' 漏出来。
                while i < data.len() && (0x20..=0x2F).contains(&data[i]) {
                    i += 1;
                }
                if i < data.len() && (0x30..=0x7E).contains(&data[i]) {
                    i += 1;
                }
                out.extend_from_slice(ESC_MARK);
            }
        }
    }
    Cow::Owned(out)
}

/// 把数据按 UTF-8 解码写进 `out`：控制符写成 `^X`，非法字节写成 `·`。
///
/// 只负责解码与显示，**不做 ANSI 折叠**（折叠是 [`fold_ansi`] 的独立一步）；
/// 需要「所见即所得」的调用方应先折叠再传进来。
/// [`visible_text`] 就是这么用的，因此它的结果与折叠无关的旧实现完全一致。
pub fn render_text(data: &[u8], out: &mut String) {
    for_each_atom(data, |a| a.push_text(out));
}

/// 一趟解码同时给出「显示成什么」和「占几列」。
///
/// 文本与列宽永远是同一份数据的两个侧面，分两次解码就会得到两个答案
/// （改一次规则就可能只改到一边）。所以这里把它们绑在一次遍历里。
///
/// `out` 为 `None` 时只数列宽：那种调用方不必为显示串付出一份分配。
///
/// `data` 必须是**已折叠**的数据（见 [`fold_ansi`]），折叠是独立的一步。
fn decode_display(data: &[u8], out: Option<&mut String>) -> usize {
    let mut width = 0usize;
    match out {
        Some(out) => {
            for_each_atom(data, |a| {
                a.push_text(out);
                width += a.width;
            });
        }
        None => {
            for_each_atom(data, |a| width += a.width);
        }
    }
    width
}

/// 文本视图下的显示串（UTF-8 优先，控制符 `^X`，非法字节 `·`）。
pub fn visible_text(data: &[u8]) -> String {
    let folded = fold_ansi(data);
    let mut s = String::with_capacity(folded.len() + folded.len() / 8);
    decode_display(folded.as_ref(), Some(&mut s));
    s
}

/// 纯预览文本：解码 UTF-8，非法字节与控制符用 `.`（HEX 视图右栏、日志右栏）。
pub fn preview_text(data: &[u8]) -> String {
    let folded = fold_ansi(data);
    let mut s = String::with_capacity(folded.len());
    for_each_atom(folded.as_ref(), |a| match a.kind {
        AtomKind::Plain(c) => s.push(c),
        AtomKind::Ctrl(_) => s.push('.'),
        AtomKind::Raw => s.push('.'),
    });
    s
}

/// 文本视图下整段数据的显示列数（按解码后的字符宽度）。
///
/// 与显示路径一致地折叠转义序列，否则同一段数据的宽度会有两个答案。
/// 传入已折叠的数据时是零成本（没有 ESC 直接借用）。
pub fn display_width(data: &[u8]) -> usize {
    let folded = fold_ansi(data);
    decode_display(folded.as_ref(), None)
}

/// 显示用的分行：与 [`split_lines`] 一致，但去掉行尾换行带来的那个空行。
///
/// `AT\r\n` 只显示一行，而不是「内容行 + 空行」；中间的真正空行仍会保留。
pub fn display_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = split_lines(data);
    if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// 一段数据的「显示形态」。
///
/// 界面每帧都要问同一组问题：这段数据有几条逻辑行？每行显示成什么？每行占几列？
/// 三个答案来自同一次解码，分三次问就是解三遍。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextView {
    /// 整段数据的显示文本；逻辑行之间用 `\n` 分隔。
    pub text: String,
    /// 每条逻辑行的显示文本。
    pub lines: Vec<String>,
    /// 每条逻辑行的显示列宽（与 `lines` 一一对应）。
    pub cols: Vec<u32>,
}

/// 把一段数据算成「显示形态」。
///
/// 语义与逐行调用 [`display_lines`] + [`visible_text`] + [`display_width`] 完全一致：
/// 逻辑行按数据里真实的换行切分（`\r\n` 里的 `\r` 被吃掉，行尾换行不产生空行），
/// 每行先折叠 ANSI 再解码，控制符显示成 `^X`、非法字节显示成 `·`。
///
/// 注意行内折叠与整段折叠的区别：这里按**行**折叠，因为行才是渲染单位；
/// 转义序列如果跨过换行（`ESC [ 31 \n`），整段折叠会把换行吞进序列里，
/// 而按行折叠时它留在行尾、显示成 `^J` —— 后者才符合「一行一行看」的直觉。
///
/// `text` 是各行显示文本用 `\n` 连起来的结果（不是整段数据的 `visible_text`：
/// 后者会把换行显示成 `^J`）。
pub fn build_text_view(data: &[u8]) -> TextView {
    let raw_lines = display_lines(data);
    let mut lines = Vec::with_capacity(raw_lines.len());
    let mut cols = Vec::with_capacity(raw_lines.len());
    let mut text = String::new();
    for (i, raw) in raw_lines.iter().enumerate() {
        let (line, col) = render_line(raw);
        if i > 0 {
            text.push('\n');
        }
        text.push_str(&line);
        lines.push(line);
        cols.push(col.min(u32::MAX as usize) as u32);
    }
    TextView { text, lines, cols }
}

/// 一行的显示文本与显示列宽（折叠 + 解码一趟）。
///
/// 没有 ESC 的常见路径：直接借用原切片解码，一个字节都不复制；
/// 有 ESC 才让 [`fold_ansi`] 产出折叠后的副本。
fn render_line(raw: &[u8]) -> (String, usize) {
    if !raw.contains(&0x1B) {
        let mut s = String::with_capacity(raw.len() + raw.len() / 8);
        let w = decode_display(raw, Some(&mut s));
        return (s, w);
    }
    let folded = fold_ansi(raw);
    let mut s = String::with_capacity(folded.len() + folded.len() / 8);
    let w = decode_display(folded.as_ref(), Some(&mut s));
    (s, w)
}

/// 一段数据折行后占几行。
///
/// **要统计「这条记录占几行」就用这个**，别自己写 `ceil(列宽 / width)`：
/// 宽字符放不下时只能整体挪到下一行，那一行会空出几列，
/// 于是实际行数会比那个公式多（公式是下界，不是等式）。
///
/// 口径与 [`build_text_view`] 一致：**按数据里真实的换行切出逻辑行，
/// 逐条逻辑行折行后求和**。界面画的也是逻辑行（换行是真换行，
/// 不是显示成 `^J` 的一个字符），两边的行数必须对得上，滚动才不会错位。
///
/// 不分配折行后的段（只有数据里含 ANSI 转义时，折叠本身要复制一份），
/// 逐条逻辑行数原子、累加列宽，与 [`wrap_display`] 的走法完全相同。
pub fn wrap_row_count(data: &[u8], width: usize) -> usize {
    let mut total = 0usize;
    let mut start = 0usize;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            // 去掉 \r\n 里的 \r，它属于行结束符
            let end = if i > start && data[i - 1] == b'\r' { i - 1 } else { i };
            total += wrapped_rows_of(&data[start..end], width);
            start = i + 1;
        }
    }
    // 末尾那一段：数据以换行结尾时是空的，不产出（与 `display_lines` 的 pop 一致）。
    if start < data.len() || (data.is_empty() || data.last() != Some(&b'\n')) {
        total += wrapped_rows_of(&data[start..], width);
    }
    total
}

/// 一条逻辑行折行后占几行（不分配折行段）。
///
/// 与 [`wrap_display`] 逐字走同一套规则（都出自 [`Greedy`]）：放不下且当前行已有内容就换行。
/// 控制符的显示文本是 `^X` 两个字符（各占 1 列），
/// 所以这里按两个 1 列的字符走，而不是按一个 2 列的原子走 ——
/// 否则折行边界会与实际渲染差一行。
fn wrapped_rows_of(line: &[u8], width: usize) -> usize {
    if width == 0 {
        return 1; // 宽度为 0 表示不折行
    }
    let folded = fold_ansi(line);
    let mut g = Greedy::new(width);
    for_each_atom(folded.as_ref(), |a| match a.kind {
        AtomKind::Ctrl(_) => {
            // 显示文本是 `^X`：两个各占 1 列的字符。
            g.put(1);
            g.put(1);
        }
        _ => {
            g.put(a.width);
        }
    });
    g.rows
}

/// 一段显示文本的显示列宽。
///
/// 逐字符求和：控制符在显示文本里已经渲染成 `^X`（恰好 2 列），
/// 与原字符的显示列宽一致，所以这里与逐原子累加等价。
pub fn display_cols(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 贪心折行的行状态：放不下就换行。
///
/// 「数行数」（[`wrap_rows`]、界面层的行高统计）与「切出各段」
/// （[`wrap_display`]）必须走同一套规则，否则两者会漂移：
/// 界面用行数算高度、用切分画内容，对不上就会滚动错位。
/// 规则只写在这里一处。
struct Greedy {
    /// 折行宽度（终端列）
    width: usize,
    /// 已经折出几行
    rows: usize,
    /// 当前行已占的列宽
    col: usize,
    /// 当前行是否已有内容
    has: bool,
}

impl Greedy {
    fn new(width: usize) -> Self {
        Greedy { width, rows: 1, col: 0, has: false }
    }
    /// 放入一个占 `w` 列的单元，返回是否因此换行。
    ///
    /// 当前行还空着时**必须**放进去：否则宽度比一个宽字符还窄时，
    /// 这个字符会被丢掉（界面上凭空少一段数据）。
    fn put(&mut self, w: usize) -> bool {
        let mut broke = false;
        if self.col + w > self.width && self.has {
            self.rows += 1;
            self.col = 0;
            self.has = false;
            broke = true;
        }
        self.col += w;
        self.has = true;
        broke
    }
}

/// 贪心折行的**唯一**实现。
///
/// 切分点只在这里算一次，调用方各自取用：
/// [`wrap_display`] 切出各段，[`wrap_rows`] 只数行数。
fn wrap_scan(s: &str, width: usize, mut emit: impl FnMut(&str)) {
    let mut g = Greedy::new(width);
    let mut row_start = 0usize; // 当前行的起始字节下标
    let mut row_end = 0usize; // 当前行的结束字节下标
    for (i, c) in s.char_indices() {
        if g.put(char_width(c)) {
            emit(&s[row_start..i]);
            row_start = i;
        }
        row_end = i + c.len_utf8();
    }
    emit(&s[row_start..row_end]);
}

/// 把一段**已解码好的显示文本**按显示列宽折成若干段。
///
/// 宽字符绝不劈开：占两列的汉字要么整段在前一行，要么整段在后一行。
/// 一行放不下就整体挪到下一行，宁可空出几列也不把一个字劈成两半
/// （半个汉字在终端里是方块或问号，比空列难看得多）。
///
/// `width <= 0` 时原样返回一段；空串返回一段空串 ——
/// 一条空的逻辑行在界面上仍然占一行，返回空数组会让高度算少一行。
pub fn wrap_display(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    wrap_scan(s, width, |seg| out.push(seg.to_string()));
    out
}

/// 一段**已解码好的显示文本**按显示列宽折行后占几行。
///
/// 与 [`wrap_display`] 的段数**完全一致**（两者的切分规则同出于
/// [`wrap_scan`]）：宽字符放不下时整体挪到下一行，会空出几列，
/// 行数因此比 `ceil(列宽 / 宽度)` **多**一行 —— 这正是不能拿除法公式
/// 当行数用的原因。
///
/// `cols` 是这段文本的显示列宽（调用方解码时已经知道）：放得下时
/// 直接返回 1，不必再扫一遍文本。
///
/// 只数不切：界面每帧要给几千条记录数行，全部分配一遍字符串会把 CPU 吃光。
pub fn wrap_rows(text: &str, cols: u32, width: usize) -> usize {
    if width == 0 {
        return 1; // 宽度为 0 表示不折行
    }
    if (cols as usize) <= width {
        // 放得下就是一行。列宽是解码时算好的，与折行用的是同一份数据。
        return 1;
    }
    let mut rows = 0usize;
    wrap_scan(text, width, |_| rows += 1);
    rows
}

/// 把一段**原始字节**按显示列宽折成若干段显示文本。
///
/// 折叠与解码只做一遍：不是「先 [`visible_text`] 再 [`wrap_display`]」的两遍，
/// 那会把同一段数据解两次（界面每帧几十条记录时差别很明显）。
///
/// 折行的粒度是**显示原子**：一个占两列的汉字、或者渲染成 `^M` 的控制符，
/// 都整体落在同一段里，不会被劈开。
///
/// **口径：整段数据当成一条流来折**，换行显示成 `^J`（两列）而不是真换行 ——
/// 与 [`build_text_view`] / 界面渲染的「按逻辑行折行」不是同一口径，
/// 段数也会对不上。要统计「一条记录占几行」用 [`wrap_row_count`]。
///
/// 段数：不折行时的显示列宽除以 `width` 向上取整 —— 但只有折行**没有浪费列**
/// 时才成立。宽字符整体挪到下一行时会空出几列（`width` 是奇数时每行空 1 列），
/// 那时实际段数只会**更多**，绝不会更少。
/// 想要与渲染完全一致的行数，用返回的 `Vec` 的长度，别用除法公式。
pub fn wrap_bytes(data: &[u8], width: usize) -> Vec<String> {
    let folded = fold_ansi(data);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;
    for_each_atom(folded.as_ref(), |a| {
        // width 为 0 表示不折行：整段原样返回
        if width > 0 && col + a.width > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            col = 0;
        }
        a.push_text(&mut cur);
        col += a.width;
    });
    out.push(cur);
    out
}

/// 与 [`display_lines`] 完全同义的**行数**，但不切分、不分配。
///
/// 界面每帧都要给全部记录数一遍行数：用 `display_lines().len()` 的话，
/// 每帧会为几千条记录各分配一个 Vec，CPU 直接被吃光（远程终端上尤其明显）。
/// 运行时走 [`line_count_from_nl`]（换行计数已随数据增量维护），
/// 这个函数是它的参考实现，测试里用来验证两者结果逐字节一致。
#[cfg(test)]
pub fn display_line_count(data: &[u8]) -> usize {
    line_count_from_nl(count_nl(data), data.last() == Some(&b'\n'))
}

/// 由「换行个数 + 是否以换行结尾」直接算行数，供已缓存换行数的调用方使用。
pub fn line_count_from_nl(nl: usize, ends_with_nl: bool) -> usize {
    // 行尾换行不产生空行：末尾有 \n 时最后一段是空的，display_lines 会把它 pop 掉
    if nl > 0 && ends_with_nl { nl } else { nl + 1 }
}

/// 数一段字节里的 `\n`。数据只增不减时可以做增量累计。
pub fn count_nl(data: &[u8]) -> usize {
    data.iter().filter(|&&b| b == b'\n').count()
}

/// 按**数据里真实的换行**切分：只有 `\n`（以及它前面的 `\r`）算换行。
///
/// `\r\n` 归为一个换行；单独的 `\r` 不切行，作为控制字符原样显示。
/// 终端里的每行都对应设备发出的一行，不会在屏宽处被硬折。
pub fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            // 去掉 \r\n 里的 \r，它属于行结束符
            let end = if i > start && data[i - 1] == b'\r' { i - 1 } else { i };
            out.push(&data[start..end]);
            start = i + 1;
        }
    }
    out.push(&data[start..]);
    out
}

/// 按显示列宽截断一段数据，返回 `(显示文本, 是否被截断)`。
///
/// 截断处在字符边界上，尾部用 `…` 提示还有内容，避免被误读成完整数据。
///
/// 只解码一遍：字符原子一次就同时给出边界、列宽和显示文本，
/// 累加宽度、定截断点、拼显示串都在同一趟里完成。
pub fn clip_text(data: &[u8], max_cols: usize) -> (String, bool) {
    let folded = fold_ansi(data);
    let atoms = scan_atoms(folded.as_ref());
    let total: usize = atoms.iter().map(|a| a.width).sum();
    if total <= max_cols {
        let mut s = String::with_capacity(total + total / 8);
        for a in &atoms {
            a.push_text(&mut s);
        }
        return (s, false);
    }
    const MARKER: &str = "…"; // 占 1 列
    let budget = max_cols.saturating_sub(MARKER.chars().count());
    // 找到第一个放不下的原子，用它的字节起点作为截断点
    let mut col = 0usize;
    let mut cut = folded.len();
    for a in &atoms {
        if col + a.width > budget {
            cut = a.start;
            break;
        }
        col += a.width;
    }
    let mut out = String::with_capacity(cut + MARKER.len());
    for a in &atoms {
        if a.start >= cut {
            break;
        }
        a.push_text(&mut out);
    }
    out.push_str(MARKER);
    (out, true)
}

/// 十六进制字符表（只用大写，与旧的 `format!("{:02X}")` 一致）。
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// 每个字节预展开成两个十六进制字符。
///
/// 逐字节 `format!` 会为每个字节分配一次临时字符串，HEX 视图刷一屏就是几千次分配；
/// 查表后只剩字符串本身的增长。
const HEX_PAIRS: [[u8; 2]; 256] = {
    let mut table = [[0u8; 2]; 256];
    let mut i = 0usize;
    while i < 256 {
        table[i] = [HEX_DIGITS[i >> 4], HEX_DIGITS[i & 0x0F]];
        i += 1;
    }
    table
};

/// 十六进制输出的公共核心：逐字节查表输出大写十六进制。
///
/// `sep` 是字节之间的分隔符；紧凑输出（JSON 里用）没有分隔符。
fn hex_into(data: &[u8], sep: Option<u8>, out: &mut String) {
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            if let Some(s) = sep {
                out.push(s as char);
            }
        }
        let pair = HEX_PAIRS[*b as usize];
        out.push(pair[0] as char);
        out.push(pair[1] as char);
    }
}

pub fn hex_bytes(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3);
    hex_into(data, Some(b' '), &mut s);
    s
}

/// 紧凑十六进制（字节之间没有空格），JSON 里用这个。
///
/// 同样走查表：逐字节 `format!("{:02X}")` 会为每个字节分配一次临时字符串，
/// 而 listen/send 的接收缓冲区可以到几十 MB。
pub fn hex_compact(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    hex_into(data, None, &mut s);
    s
}

/// 文本模式解析发送框内容，支持 `\r \n \t \0 \a \b \e \\ \xNN` 与续行。
pub fn parse_send_text(input: &str) -> Result<Vec<u8>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            return Err("输入以孤立的反斜杠 \\ 结尾".into());
        }
        match chars[i] {
            'n' => out.push(0x0A),
            'r' => out.push(0x0D),
            't' => out.push(0x09),
            '0' => out.push(0x00),
            'a' => out.push(0x07),
            'b' => out.push(0x08),
            'v' => out.push(0x0B),
            'f' => out.push(0x0C),
            'e' => out.push(0x1B),
            '\\' => out.push(0x5C),
            '\'' => out.push(0x27),
            '"' => out.push(0x22),
            '\n' => {} // 行尾续行：忽略换行本身
            'x' | 'X' => {
                let mut v = 0u8;
                let mut n = 0;
                while n < 2 && i + 1 < chars.len() {
                    match chars[i + 1].to_digit(16) {
                        Some(d) => {
                            v = v * 16 + d as u8;
                            i += 1;
                            n += 1;
                        }
                        None => break,
                    }
                }
                if n == 0 {
                    return Err("\\x 后需要 1~2 位十六进制数字".into());
                }
                out.push(v);
            }
            other => return Err(format!("未知转义序列 \\{other}")),
        }
        i += 1;
    }
    Ok(out)
}

/// 单个十六进制字符的数值（0-15）。
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// HEX 模式解析：`AA BB` / `AA-BB` / `AABB`，允许 0x 前缀。
///
/// 按「标记」解析：空白与 `- _ : , ;` 是分隔符，每段标记允许一个 `0x` / `0X` 前缀，
/// 去掉前缀后必须是偶数位的十六进制数字。
///
/// 早期版本先用 `replace("0x", "")` 全局删前缀，`"410x42"` 会被悄悄拼成 `4142`，
/// 用户以为发的是别的东西；现在这种标记会明确报错。
pub fn parse_send_hex(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for token in input.split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | ':' | ',' | ';')) {
        if token.is_empty() {
            continue;
        }
        // 前缀只允许出现在标记开头：标记中间夹着的 "0x" 是非法字符
        let body = token.strip_prefix("0x").or_else(|| token.strip_prefix("0X")).unwrap_or(token);
        if body.is_empty() {
            // 光秃秃的 "0x"：没有字节可贡献，忽略（与旧行为一致）
            continue;
        }
        if let Some(c) = body.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(format!("十六进制标记 {token} 里含有非十六进制字符 {c}"));
        }
        if body.len() % 2 != 0 {
            return Err(format!("十六进制标记 {token} 的位数为奇数，请补齐为偶数位"));
        }
        let bytes = body.as_bytes();
        for pair in bytes.chunks(2) {
            let hi = hex_digit(pair[0]).ok_or_else(|| format!("十六进制标记 {token} 不是合法十六进制字节"))?;
            let lo = hex_digit(pair[1]).ok_or_else(|| format!("十六进制标记 {token} 不是合法十六进制字节"))?;
            out.push(hi << 4 | lo);
        }
    }
    Ok(out)
}

pub fn parse_send(input: &str, hex: bool) -> Result<Vec<u8>, String> {
    if hex {
        parse_send_hex(input)
    } else {
        parse_send_text(input)
    }
}

/// 字节数的可读写法。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// JSON 字符串的内容部分转义（**不含**两侧引号）。
///
/// 双引号、反斜杠、换行、回车、制表符用 JSON 常规短转义；
/// 其余小于 0x20 的控制字符写成 `\u00XX`（大写十六进制）。
/// 引号由调用方自己拼，这样既能单独用，也能塞进已有的 JSON 结构里。
///
/// 语义与 `cli.rs` 里那个私有 `escape` 一致（同样把控制字符变成合法 JSON），
/// 区别是本函数不吐引号、并且对 `\n \r \t` 用短转义。
pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 不重叠匹配的公共骨架。
///
/// 「大小写敏感」与「忽略大小写」两个版本的**规则**必须一样
/// （同样的不重叠、同样的字符下标），只有比较方式不同：
/// 两份实现各写一遍，早晚会漂移成「过滤命中了但高亮没画出来」。
///
/// `eq` 是单**字节**的比较方式；`on_hit` 收到每次命中的**字符下标**区间
/// （起点含，终点不含），返回 `false` 表示「到此为止，别再找了」。
///
/// 为什么按字节扫而不是按字符扫：
/// 过滤每帧要过几千条记录、几十万字符。早期那版按字符扫，
/// 每读一个字符就把窗口 `Vec::remove(0)` 搬一次（O(n) 的元素搬移），
/// 而且每次调用都要为 needle 与窗口各分配一次 `Vec` ——
/// 一帧下来就是几万次堆分配，忽略大小写这一档实测要 10 ms/帧。
/// 按字节扫全程零分配，实测同一个用例降到 1 ms 以内。
///
/// 按字节匹配不会把一个多字节字符切成两半：UTF-8 是自同步的，
/// 合法字节序列只可能在字符边界上对齐，非 ASCII 字节走的是精确比较。
fn scan_matches(
    hay: &str,
    needle: &str,
    eq: impl Fn(u8, u8) -> bool,
    mut on_hit: impl FnMut(usize, usize) -> bool,
) -> bool {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        // 空串在每个位置都匹配，没有意义
        return false;
    }
    // needle 占几个字符：字节相等（大小写不敏感时只是字母大小写不同）
    // 意味着字符结构相同，所以命中处的字符数恒等于它。
    let needle_chars = needle.chars().count();
    let mut byte = 0usize; // 当前扫描位置（始终落在字符边界上）
    let mut char_idx = 0usize; // byte 之前的字符数
    while byte + n.len() <= h.len() {
        if eq(h[byte], n[0]) {
            let mut ok = true;
            for (k, &nb) in n.iter().enumerate().skip(1) {
                if !eq(h[byte + k], nb) {
                    ok = false;
                    break;
                }
            }
            if ok {
                if !on_hit(char_idx, char_idx + needle_chars) {
                    return true;
                }
                // 不重叠：命中后从末尾继续找
                char_idx += needle_chars;
                byte += n.len();
                continue;
            }
        }
        // 没命中：整体跳过一个字符，保持 byte 落在字符边界上
        byte += utf8_len(h[byte]);
        char_idx += 1;
    }
    false
}

/// 一个 UTF-8 字符的首字节决定它占几个字节。
///
/// 非法字节（续字节 0x80..=0xBF 或残缺序列）按 1 字节处理：
/// 宁可少跳，也不能跳过半个字符 —— 那会让字符下标对不上。
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// `needle` 在 `hay` 里所有**不重叠**出现的位置。
///
/// 返回的是**字符下标**（不是字节下标）的 `(起点, 终点)`，终点不含；
/// 字符下标才能直接对上终端里的列与光标位置。
/// `needle` 为空时返回空数组：空串在每个位置都匹配，没有意义。
pub fn match_ranges(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    scan_matches(hay, needle, |a, b| a == b, |s, e| {
        out.push((s, e));
        true
    });
    out
}

/// `needle` 在 `hay` 里是否出现，比较时**忽略 ASCII 大小写**。
///
/// 语义与 `str::contains` 一致（子串出现即真），只是 ASCII 字母
/// 不分大小写。给「搜 err」找得到「ERROR」这类事用的。
///
/// 逐字节比较，不给 hay 做 `to_lowercase`：hay 可能有几百 KB，
/// 每帧复制一份会把渲染拖垮。
/// 非 ASCII 字符（中文等）按原样比较 —— 中文没有大小写。
pub fn contains_ignore_ascii_case(hay: &str, needle: &str) -> bool {
    scan_matches(hay, needle, |a, b| a.eq_ignore_ascii_case(&b), |_, _| false)
}

/// `needle` 在 `hay` 里所有**不重叠**出现的位置，比较时**忽略 ASCII 大小写**。
///
/// 与 [`match_ranges`] 的语义完全一致（字符下标、不重叠、空 needle 返回空），
/// 只是比较时 ASCII 字母不分大小写。
///
/// 高亮用的就是它：过滤命中了却没画高亮，比不命中还让人困惑。
pub fn match_ranges_ignore_ascii_case(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    scan_matches(hay, needle, |a, b| a.eq_ignore_ascii_case(&b), |s, e| {
        out.push((s, e));
        true
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes() {
        assert_eq!(parse_send_text(r"a\r\nb").unwrap(), b"a\r\nb");
        assert_eq!(parse_send_text(r"\x41\x42").unwrap(), b"AB");
        assert_eq!(parse_send_text(r"\e[0m").unwrap(), b"\x1b[0m");
        assert!(parse_send_text(r"\q").is_err());
    }

    #[test]
    fn hex_input() {
        assert_eq!(parse_send_hex("41 42 43").unwrap(), b"ABC");
        assert_eq!(parse_send_hex("0x41-0x42").unwrap(), b"AB");
        assert!(parse_send_hex("ABC").is_err());
    }

    #[test]
    fn visible_width_and_split() {
        let data = b"A\r\n\xff\x00";
        assert_eq!(visible_text(data), "A^M^J·^@");
        assert_eq!(display_width(data), 1 + 2 + 2 + 1 + 2);
        // 换行切分后每行不含换行符本身
let lines = split_lines(data);
assert_eq!(lines.len(), 2);
assert_eq!(lines[0], b"A");
assert_eq!(lines[1], b"\xff\x00");
// 窄列截断不会把占两列的控制符劈开
let (s, cut) = clip_text(data, 4);
assert!(cut);
assert!(s.ends_with('…'));
    }

    #[test]
    fn decodes_utf8_text() {
        let data = "温度 25℃\r\n".as_bytes();
        // 中文按原字符显示，不再降级成点号
        assert_eq!(visible_text(data), "温度 25℃^M^J");
        // 温/度各 2 列，空格与 2、5 各 1 列，℃ 为歧义宽度按 1 列计，两个控制符各 2 列
        assert_eq!(display_width(data), 2 + 2 + 1 + 1 + 1 + 1 + 2 + 2);
        assert_eq!(preview_text(data), "温度 25℃..");
    }

    #[test]
    fn lone_carriage_return_does_not_break_lines() {
        // 进度条式的 \r 刷新不再把一行劈开
        assert_eq!(split_lines(b"50%\r75%\r100%").len(), 1);
        // \r\n 只算一次换行，且不切出空行
        assert_eq!(split_lines(b"a\r\nb").len(), 2);
        assert_eq!(split_lines(b"a\r\nb")[0], b"a");
        assert_eq!(display_lines(b"ok\r\n").len(), 1, "行尾换行不产生空行");
    }

    #[test]
    fn splits_only_on_real_newlines() {
        // 只有数据里真的出现换行才切行，长行不按屏宽折
        assert_eq!(split_lines(&vec![b'x'; 500]).len(), 1);
        // \r\n 与 \n 各算一次换行；孤立的 \r 不切行
        assert_eq!(split_lines(b"a\r\nb\nc\rd").len(), 3);
        assert_eq!(split_lines(b"a\r\nb\nc\rd")[2], b"c\rd");
        assert_eq!(split_lines(b"abc\n").len(), 2); // 末尾换行之后还有一行空行
        assert_eq!(split_lines(b"").len(), 1);
        let parts = split_lines("温度\r\n数据".as_bytes());
        assert_eq!(parts[0], "温度".as_bytes());
        assert_eq!(parts[1], "数据".as_bytes());
    }

    #[test]
    fn clips_by_display_width() {
        let cn = "温度传感器数据".as_bytes();
        let (s, cut) = clip_text(cn, 5);
        assert!(cut);
        assert_eq!(s, "温度…");
        let (s, cut) = clip_text(cn, 100);
        assert!(!cut);
        assert_eq!(s, "温度传感器数据");
    }

    #[test]
    fn invalid_bytes_degrade_gracefully() {
        let data: &[u8] = &[0xE4, 0xB8, 0xAD, 0xFF, 0x80]; // 「中」+ 两个非法字节
        let units = decode_units(data);
        assert!(units.iter().any(|u| matches!(u, Unit::Text(t) if t == "中")));
        assert_eq!(units.iter().filter(|u| matches!(u, Unit::Raw(_))).count(), 2);
        assert_eq!(visible_text(data), "中··");
    }

    #[test]
    fn epoch_day_zero() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    /// 不分配的行数必须和真正切分出来的行数一致 ——
    /// 界面每帧靠它算高度，算错就是「显示错位」或「滚不到底」。
    #[test]
    fn line_count_matches_actual_split() {
        let cases: [&[u8]; 8] = [
            b"",
            b"abc",
            b"abc\n",
            b"a\r\nb",
            b"a\r\nb\r\n",
            b"\n\n\n",
            b"\n",
            "温度\r\n数据\n".as_bytes(),
        ];
        for c in cases {
            assert_eq!(
                display_line_count(c),
                display_lines(c).len(),
                "行数不一致：{c:?}"
            );
        }
        // 行首换行产生的空行必须被计入，不能被当成「没有换行」
        assert_eq!(display_line_count(b"\n\n"), 2);
        assert_eq!(display_line_count(b"\n\n"), display_lines(b"\n\n").len());
    }

    /// 转义序列必须折叠成定宽标记，否则彩色日志会刷成一屏 `^[[31m` 垃圾。
    #[test]
    fn ansi_sequences_are_folded() {
        // 颜色码
        let colored = "\x1b[31mERROR\x1b[0m: 温度过高".as_bytes();
        let (text, cut) = clip_text(colored, 80);
        assert_eq!(text, "^[mERROR^[m: 温度过高");
        assert!(!cut);
        // 折叠后固定 3 列，行宽才稳定
        assert_eq!(display_width(b"\x1b[31m"), 3);

        // 光标移动 / 清屏：纯终端操作，对看数据没有意义，同样折叠
        assert_eq!(clip_text(b"\x1b[2K\x1b[1Aabc", 80).0, "^[m^[mabc");

        // OSC：到 BEL 或 ST 才结束，中间可以很长
        assert_eq!(clip_text("\x1b]0;窗口标题\x07text".as_bytes(), 80).0, "^[mtext");
        assert_eq!(clip_text("\x1b]0;标题\x1b\\text".as_bytes(), 80).0, "^[mtext");

        // 两字节序列
        assert_eq!(clip_text(b"\x1b(Babc", 80).0, "^[mabc");

        // 数据在 ESC 处被切断：不能留下半个标记，也不能吞掉后面的内容
        assert_eq!(clip_text(b"abc\x1b", 80).0, "abc^[m");

        // 没有 ESC 时原样通过（零分配的快路径）
        assert!(matches!(fold_ansi(b"plain text"), Cow::Borrowed(_)));
        assert_eq!(clip_text("温度 25℃".as_bytes(), 80).0, "温度 25℃");
    }

    /// 折叠不能改变切分与截断的正确性：转义里的字节不是可显示内容。
    #[test]
    fn folding_keeps_line_count_intact() {
        // 颜色码跨在换行两侧，行数仍按真实换行算
        let data = b"\x1b[32mok\x1b[0m\r\nsecond";
        assert_eq!(display_line_count(data), 2);
        let lines = display_lines(data);
        assert_eq!(lines.len(), 2);
        assert_eq!(visible_text(lines[0]), "^[mok^[m");
    }

    // ------------------------------------------------------------------
    // 以下是「单次解码」改造的回归护栏。
    //
    // 改造把 clip_text 从「解码三遍」压成「解码一遍」，行为必须逐字节不变。
    // 下面的 legacy_* 是**独立**的旧版实现：自己按 `from_utf8` 的
    // valid_up_to / error_len 切分字节，不复用新的 scan_atoms / decode_units。
    // （早先它们直接调用 decode_units —— 那正是被改造的那套解码器，
    //  这样的护栏只能证明「单元聚合 == 原子聚合」，证明不了解码语义没变。）
    // 新的实现必须在任何输入上与它完全一致。
    // ------------------------------------------------------------------

    /// 旧版解码单元：合法文本片段 / 非法字节。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum LegacyUnit {
        Text(String),
        Raw(u8),
    }

    /// 旧版解码：自己按 `from_utf8` 的 valid_up_to / error_len 切分。
    ///
    /// 与 `for_each_atom` 无关，是一份独立的对照实现。
    fn legacy_units(data: &[u8]) -> Vec<LegacyUnit> {
        let mut units: Vec<LegacyUnit> = Vec::new();
        let mut text = String::new();
        let mut i = 0usize;
        while i < data.len() {
            match std::str::from_utf8(&data[i..]) {
                Ok(s) => {
                    text.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let s = std::str::from_utf8(&data[i..i + valid]).expect("合法前缀");
                        text.push_str(s);
                        i += valid;
                        continue;
                    }
                    if !text.is_empty() {
                        units.push(LegacyUnit::Text(std::mem::take(&mut text)));
                    }
                    match e.error_len() {
                        Some(len) => {
                            for k in i..i + len {
                                units.push(LegacyUnit::Raw(data[k]));
                            }
                            i += len;
                        }
                        // 末尾是不完整的多字节序列：逐字节降级
                        None => {
                            for k in i..data.len() {
                                units.push(LegacyUnit::Raw(data[k]));
                            }
                            i = data.len();
                        }
                    }
                }
            }
        }
        if !text.is_empty() {
            units.push(LegacyUnit::Text(text));
        }
        units
    }

    /// 旧版里「控制字符」的判定：小于 0x20 或 0x7F。
    fn legacy_is_control(c: char) -> bool {
        let v = c as u32;
        v < 0x20 || v == 0x7F
    }

    /// 旧版里控制字符的显示形式：`^X`。
    fn legacy_push_control(c: char, out: &mut String) {
        out.push('^');
        out.push(char::from_u32((c as u32) ^ 0x40).unwrap_or('?'));
    }

    /// 旧版里单个字符的显示列宽。
    fn legacy_char_width(c: char) -> usize {
        if legacy_is_control(c) {
            2
        } else {
            UnicodeWidthChar::width(c).unwrap_or(0)
        }
    }

    /// 旧版显示串：解码成单元，再逐单元拼。
    fn legacy_visible_text(data: &[u8]) -> String {
        let folded = fold_ansi(data);
        let data = folded.as_ref();
        let mut s = String::with_capacity(data.len() + data.len() / 8);
        for u in legacy_units(data) {
            match u {
                LegacyUnit::Text(t) => {
                    for c in t.chars() {
                        if legacy_is_control(c) {
                            legacy_push_control(c, &mut s);
                        } else {
                            s.push(c);
                        }
                    }
                }
                LegacyUnit::Raw(_) => s.push('·'),
            }
        }
        s
    }

    /// 旧版列宽：单元宽度求和。
    fn legacy_display_width(data: &[u8]) -> usize {
        let folded = fold_ansi(data);
        let data = folded.as_ref();
        legacy_units(data)
            .iter()
            .map(|u| match u {
                LegacyUnit::Text(t) => t.chars().map(legacy_char_width).sum(),
                LegacyUnit::Raw(_) => 1,
            })
            .sum()
    }

    /// 旧版字符原子：`(字节起点, 字节终点, 显示列宽)`。
    fn legacy_char_atoms(data: &[u8]) -> Vec<(usize, usize, usize)> {
        let mut atoms = Vec::new();
        let mut off = 0usize;
        for u in legacy_units(data) {
            match &u {
                LegacyUnit::Text(s) => {
                    for (ci, c) in s.char_indices() {
                        atoms.push((off + ci, off + ci + c.len_utf8(), legacy_char_width(c)));
                    }
                    off += s.len();
                }
                LegacyUnit::Raw(_) => {
                    atoms.push((off, off + 1, 1));
                    off += 1;
                }
            }
        }
        atoms
    }

    /// 旧版截断：宽度一遍、原子一遍、显示串一遍。
    fn legacy_clip_text(data: &[u8], max_cols: usize) -> (String, bool) {
        let folded = fold_ansi(data);
        let data = folded.as_ref();
        if legacy_display_width(data) <= max_cols {
            return (legacy_visible_text(data), false);
        }
        const MARKER: &str = "…";
        let budget = max_cols.saturating_sub(MARKER.chars().count());
        let mut col = 0usize;
        let mut cut = data.len();
        for (s, _e, w) in legacy_char_atoms(data) {
            if col + w > budget {
                cut = s;
                break;
            }
            col += w;
        }
        let mut out = legacy_visible_text(&data[..cut]);
        out.push_str(MARKER);
        (out, true)
    }

    /// 伪随机字节（线性同余），用来造「乱七八糟」的输入。
    fn pseudo_random_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v.push((s >> 33) as u8);
        }
        v
    }

    /// 覆盖各种「有意思」的输入：宽字符、控制符、非法字节、转义序列。
    fn interesting_data() -> Vec<Vec<u8>> {
        let mut cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"plain ascii".to_vec(),
            b"A\r\n\xff\x00".to_vec(),
            b"\x00\x01\x02\x7f".to_vec(),
            b"\xe4\xb8\xad\xff\x80".to_vec(),   // 「中」+ 两个非法字节
            b"\xf0\x9f\x98\x80".to_vec(),       // emoji
            b"\xe4\xb8".to_vec(),                // 末尾不完整的多字节序列
            b"\x1b[31mERROR\x1b[0m: \xe6\xb8\xa9\xe5\xba\xa6".to_vec(),
            b"\x1b]0;title\x07text".to_vec(),
            b"\x1b]0;title\x1b\\text".to_vec(),
            b"\x1b(Babc".to_vec(),
            b"abc\x1b".to_vec(),
            b"50%\r75%\r100%".to_vec(),
        ];
        cases.push("温度传感器数据".as_bytes().to_vec());
        cases.push("温度 25℃".as_bytes().to_vec());
        for seed in 0..8u64 {
            cases.push(pseudo_random_bytes(48, seed));
        }
        cases
    }

    /// 单次解码改造不能改变任何一次显示结果。
    #[test]
    fn single_pass_matches_legacy_behaviour() {
        for data in interesting_data() {
            assert_eq!(visible_text(&data), legacy_visible_text(&data), "显示串不一致：{data:?}");
            assert_eq!(
                display_width(&data),
                legacy_display_width(&data),
                "列宽不一致：{data:?}"
            );
            for max_cols in 0..24usize {
                assert_eq!(
                    clip_text(&data, max_cols),
                    legacy_clip_text(&data, max_cols),
                    "截断不一致：{data:?} @ {max_cols}"
                );
            }
        }
    }

    /// 原子必须首尾相接、覆盖整段数据，否则截断点会落在半个字符上。
    #[test]
    fn atoms_tile_the_data() {
        for data in interesting_data() {
            let folded = fold_ansi(&data);
            let atoms = scan_atoms(folded.as_ref());
            let mut expect = 0usize;
            for a in &atoms {
                assert_eq!(a.start, expect, "原子不连续：{data:?}");
                assert!(a.end > a.start, "原子长度为 0：{data:?}");
                expect = a.end;
            }
            assert_eq!(expect, folded.len(), "原子没盖住整段数据：{data:?}");
        }
    }

    /// 截断边界：宽字符不劈开、控制符占 2 列、非法字节显示成 `·`。
    #[test]
    fn clip_text_respects_wide_and_control_atoms() {
        // 宽字符占 2 列：放不下半个「度」就停，尾部补 …
        assert_eq!(clip_text("温度数据".as_bytes(), 4), ("温…".into(), true));
        assert_eq!(clip_text("温度数据".as_bytes(), 5), ("温度…".into(), true));
        // 刚好放得下就不截断
        assert_eq!(clip_text("温度".as_bytes(), 4), ("温度".into(), false));

        // 控制符占 2 列（显示成 ^X），不能只塞进半个
        assert_eq!(clip_text(b"\rAB", 3), ("^M…".into(), true));
        assert_eq!(clip_text(b"\rAB", 4), ("^MAB".into(), false));

        // 非法字节显示成 ·，占 1 列
        assert_eq!(clip_text(b"\xff\xff", 1), ("…".into(), true));
        assert_eq!(clip_text(b"\xff\xff", 2), ("··".into(), false));

        // 空数据
        assert_eq!(clip_text(b"", 0), (String::new(), false));
        assert_eq!(clip_text(b"", 8), (String::new(), false));
    }

    /// 查表版 hex_bytes 必须和逐字节 `format!("{:02X}")` 完全一致。
    #[test]
    fn hex_bytes_matches_per_byte_format() {
        let legacy = |data: &[u8]| {
            let mut s = String::with_capacity(data.len() * 3);
            for (i, b) in data.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(&format!("{:02X}", b));
            }
            s
        };
        // 全部 256 个字节值
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(hex_bytes(&all), legacy(&all));
        assert_eq!(hex_bytes(&all[..1]), "00");
        assert_eq!(hex_bytes(&all[..4]), "00 01 02 03");
        assert_eq!(hex_bytes(&[]), "");
        assert_eq!(hex_bytes(&[0xAB, 0xCD]), "AB CD");
        // 伪随机数据
        let rnd = pseudo_random_bytes(300, 42);
        assert_eq!(hex_bytes(&rnd), legacy(&rnd));
    }

    /// 紧凑十六进制（JSON 里用这个）：查表版本与逐字节 `format!` 一样。
    #[test]
    fn hex_compact_matches_per_byte_format() {
        let legacy = |data: &[u8]| {
            let mut s = String::with_capacity(data.len() * 2);
            for b in data {
                s.push_str(&format!("{:02X}", b));
            }
            s
        };
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(hex_compact(&all), legacy(&all));
        assert_eq!(hex_compact(&[]), "");
        assert_eq!(hex_compact(&[0xAB, 0xCD]), "ABCD");
        assert_eq!(hex_compact(&[0x00, 0x0A, 0xFF]), "000AFF");
        let rnd = pseudo_random_bytes(300, 42);
        assert_eq!(hex_compact(&rnd), legacy(&rnd));
    }

    /// HEX 输入按标记解析：非法标记必须报错，而且要说清楚是哪个标记出的问题。
    #[test]
    fn hex_input_errors_name_the_offending_token() {
        // 旧实现会把 "410x42" 悄悄拼成 4142，现在必须报错
        let err = parse_send_hex("410x42").unwrap_err();
        assert!(err.contains("410x42"), "错误信息里要带上出错的标记：{err}");

        // 奇数位
        let err = parse_send_hex("ABC").unwrap_err();
        assert!(err.contains("ABC"), "错误信息里要带上出错的标记：{err}");
        let err = parse_send_hex("41 4").unwrap_err();
        assert!(err.contains('4'), "错误信息里要带上出错的标记：{err}");

        // 非十六进制字符
        let err = parse_send_hex("ZZ").unwrap_err();
        assert!(err.contains("ZZ"), "错误信息里要带上出错的标记：{err}");
        let err = parse_send_hex("41-gg").unwrap_err();
        assert!(err.contains("gg"), "错误信息里要带上出错的标记：{err}");
    }

    /// 既有行为一个都不能丢。
    #[test]
    fn hex_input_accepts_the_usual_shapes() {
        assert_eq!(parse_send_hex("41 42 43").unwrap(), b"ABC");
        assert_eq!(parse_send_hex("0x41-0x42").unwrap(), b"AB");
        assert_eq!(parse_send_hex("41 42 43").unwrap(), b"ABC");
        assert_eq!(parse_send_hex("AABB").unwrap(), vec![0xAA, 0xBB]);
        assert_eq!(parse_send_hex("0x41 0x42").unwrap(), b"AB");
        assert_eq!(parse_send_hex("41,42;43").unwrap(), b"ABC");
        assert_eq!(parse_send_hex("41_42").unwrap(), b"AB");
        assert_eq!(parse_send_hex("41:42").unwrap(), b"AB");
        assert_eq!(parse_send_hex("  41   42  ").unwrap(), b"AB");
        assert_eq!(parse_send_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(parse_send_hex("0x41").unwrap(), b"A");
    }

    /// JSON 转义：引号、反斜杠、换行、回车、制表符与其它控制字符。
    #[test]
    fn escape_json_escapes_the_usual_suspects() {
        assert_eq!(escape_json(""), "");
        assert_eq!(escape_json("普通文本"), "普通文本");
        assert_eq!(escape_json(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_json(r"a\b"), r"a\\b");
        assert_eq!(escape_json("a\nb"), r"a\nb");
        assert_eq!(escape_json("a\rb"), r"a\rb");
        assert_eq!(escape_json("a\tb"), r"a\tb");
        // 其余小于 0x20 的控制字符写成 \u00XX
        assert_eq!(escape_json("\u{1}"), r"\u0001");
        assert_eq!(escape_json("\u{0b}"), r"\u000B");
        assert_eq!(escape_json("\u{1f}"), r"\u001F");
        // 引号和反斜杠本身不会被重复转义
        assert_eq!(escape_json(r#"\""#), r#"\\\""#);
    }

    /// 匹配位置必须是字符下标，且不重叠。
    #[test]
    fn match_ranges_returns_char_indices() {
        assert_eq!(match_ranges("abcabc", "bc"), vec![(1, 3), (4, 6)]);
        // 字符下标不是字节下标：「温」在 UTF-8 里占 3 字节，'b' 在它后面第 2 个字符位
        assert_eq!(match_ranges("温abc", "b"), vec![(2, 3)]);
        assert_eq!(match_ranges("温温温", "温"), vec![(0, 1), (1, 2), (2, 3)]);
        // 不重叠：命中后从末尾继续找
        assert_eq!(match_ranges("aaaa", "aa"), vec![(0, 2), (2, 4)]);
        assert_eq!(match_ranges("aaaa", "aaa"), vec![(0, 3)]);
        // 空 needle 没有意义
        assert_eq!(match_ranges("abc", ""), Vec::<(usize, usize)>::new());
        assert_eq!(match_ranges("", "a"), Vec::<(usize, usize)>::new());
        assert_eq!(match_ranges("abc", "d"), Vec::<(usize, usize)>::new());
        // 命中范围按字符切片能还原出 needle
        let hay = "温度传感器温度";
        for (s, e) in match_ranges(hay, "温度") {
            assert_eq!(hay.chars().collect::<Vec<_>>()[s..e], "温度".chars().collect::<Vec<_>>());
        }
    }

    // ------------------------------------------------------------------
    // 「显示形态」与折行。
    //
    // 界面要同时拿到「每行显示成什么」和「每行占几列」，
    // 分两次问就是解两遍；build_text_view 把它合成一次。
    // 下面的用例要求它与既有的 display_lines / visible_text / display_width
    // 逐行完全一致 —— 不一致就意味着界面上一处显示、一处排版，宽度对不上。
    // ------------------------------------------------------------------

    /// 刁钻输入集合：换行、孤立 \r、ANSI、非法字节、中文与 emoji、空数据。
    fn tricky_data() -> Vec<Vec<u8>> {
        let mut cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),                                 // 空数据
            b"plain ascii".to_vec(),
            b"A\r\n\xff\x00".to_vec(),                   // \r\n + 非法字节 + 控制符
            b"\x00\x01\x02\x7f".to_vec(),                 // 纯控制字符
            b"\xe4\xb8\xad\xff\x80".to_vec(),             // 「中」+ 两个非法字节
            b"\xf0\x9f\x98\x80".to_vec(),                 // emoji
            b"\xe4\xb8".to_vec(),                         // 末尾不完整的多字节序列
            b"\x1b[31mERROR\x1b[0m: \xe6\xb8\xa9\xe5\xba\xa6".to_vec(),
            b"\x1b]0;title\x07text".to_vec(),
            b"\x1b]0;title\x1b\\text".to_vec(),
            b"\x1b(Babc".to_vec(),
            b"abc\x1b".to_vec(),                         // 数据在 ESC 处被切断
            b"50%\r75%\r100%".to_vec(),                  // 孤立 \r：不切行
            b"abc\n".to_vec(),                           // 末尾换行：不产生空行
            b"\n".to_vec(),                               // 只有换行
            b"a\r\nb\r\n".to_vec(),
            b"\x1b[31\nrest".to_vec(),                    // 转义序列跨过换行
        ];
        cases.push("温度传感器数据\n第二行".as_bytes().to_vec());
        cases.push(b"emoji \xf0\x9f\x98\x80 end".to_vec());
        cases
    }

    /// build_text_view 必须与「逐行调用旧接口」完全一致。
    #[test]
    fn text_view_matches_existing_paths() {
        for data in tricky_data() {
            let view = build_text_view(&data);
            let raw_lines = display_lines(&data);

            assert_eq!(view.lines.len(), raw_lines.len(), "行数不一致：{data:?}");
            assert_eq!(view.cols.len(), view.lines.len(), "列宽与行数对不上：{data:?}");

            for (i, raw) in raw_lines.iter().enumerate() {
                assert_eq!(view.lines[i], visible_text(raw), "行文本不一致：{data:?} 第 {i} 行");
                assert_eq!(
                    view.cols[i] as usize,
                    display_width(raw),
                    "行列宽不一致：{data:?} 第 {i} 行"
                );
                // 与「单次解码」之前的对照实现也要一致（解码语义没被改过）
                assert_eq!(view.lines[i], legacy_visible_text(raw), "行文本与旧实现不一致：{data:?}");
                assert_eq!(
                    view.cols[i] as usize,
                    legacy_display_width(raw),
                    "行列宽与旧实现不一致：{data:?}"
                );
                // 显示文本的列宽必须等于原字节的列宽：
                // 控制符渲染成 ^X（2 列）、非法字节渲染成 ·（1 列），宽度对得上
                assert_eq!(
                    display_cols(&view.lines[i]),
                    view.cols[i] as usize,
                    "显示列宽与显示文本对不上：{data:?} 第 {i} 行"
                );
            }

            // text 就是各行显示文本用 \n 连起来
            assert_eq!(view.text, view.lines.join("\n"), "整体文本不一致：{data:?}");
        }
    }

    /// 空数据与末尾换行的边界：一条空的逻辑行仍然是「一行」。
    #[test]
    fn text_view_edge_cases() {
        let v = build_text_view(b"");
        assert_eq!(v.lines, vec![String::new()]);
        assert_eq!(v.cols, vec![0]);
        assert_eq!(v.text, "");

        let v = build_text_view(b"abc\n");
        assert_eq!(v.lines, vec!["abc".to_string()]);
        assert_eq!(v.cols, vec![3]);
        assert_eq!(v.text, "abc");

        // 中间的空行要留着
        let v = build_text_view(b"a\n\nb");
        assert_eq!(v.lines, vec!["a".to_string(), String::new(), "b".to_string()]);
        assert_eq!(v.cols, vec![1, 0, 1]);
        assert_eq!(v.text, "a\n\nb");
    }

    /// 折行不能把宽字符劈开。
    #[test]
    fn wrap_display_never_splits_wide_chars() {
        // 宽度恰好等于一个汉字的 2 列：每行正好一个汉字
        assert_eq!(wrap_display("温度", 2), vec!["温", "度"]);
        // 宽度比一个汉字还窄：汉字宁可超宽也不劈开
        assert_eq!(wrap_display("温度", 1), vec!["温", "度"]);
        // 奇数宽度：2 列的汉字放不满 3 列，每行只能放一个
        assert_eq!(wrap_display("温度数据", 3), vec!["温", "度", "数", "据"]);
        // 混合宽度：ASCII 填缝，宽字符保持完整
        let segs = wrap_display("ab温度cd", 4);
        assert_eq!(segs, vec!["ab温", "度cd"]);
        assert_eq!(segs.concat(), "ab温度cd");
    }

    /// 空串与非法宽度：不 panic、不丢字符。
    #[test]
    fn wrap_display_handles_degenerate_widths() {
        // 空串返回一段空串 —— 一条空的逻辑行在界面上仍然占一行
        assert_eq!(wrap_display("", 10), vec![String::new()]);
        assert_eq!(wrap_display("", 0), vec![String::new()]);
        // 宽度 0：原样返回一段
        assert_eq!(wrap_display("温度abc", 0), vec!["温度abc".to_string()]);
        // 宽度 1：ASCII 一个字一行
        assert_eq!(wrap_display("abc", 1), vec!["a", "b", "c"]);
        // 宽度 1 遇到宽字符：不丢字符
        assert_eq!(wrap_display("温", 1), vec!["温"]);
        assert_eq!(wrap_display("温度", 1).concat(), "温度");
    }

    /// 段数公式：显示列宽 / width 向上取整。
    ///
    /// 前提是折行**没有浪费列**：宽字符整体挪到下一行时会空出几列
    /// （奇数宽度下每行空 1 列），那时实际段数只会更多，不会更少
    /// （见 wrap_bytes_wide_chars_may_waste_columns）。
    #[test]
    fn wrap_bytes_segment_count_follows_column_budget() {
        let cases: &[(&[u8], usize)] = &[
            (b"hello world", 5),
            (b"hello world", 1),
            (b"hello world", 3),
            (b"hello world", 100),
            (b"abc", 3),
            (b"abcdef", 3),
            (b"\x1b[31mred\x1b[0m text", 4),
            // 宽度是宽字符的整数倍时不会浪费列，公式依然成立
            ("温度数据".as_bytes(), 2),
            ("温度数据".as_bytes(), 4),
            ("温度数据".as_bytes(), 8),
        ];
        for (data, width) in cases {
            let cols = display_width(data);
            let segs = wrap_bytes(data, *width);
            // 列宽为 0 的行返回一段空串
            let expect = if cols == 0 { 1 } else { cols.div_ceil(*width) };
            assert_eq!(segs.len(), expect, "段数不一致：{data:?} @ {width}");
            // 所有段拼起来等于不折行时的显示文本
            assert_eq!(segs.concat(), visible_text(data), "内容不一致：{data:?} @ {width}");
            // 每段的列宽都不超过 width
            for seg in &segs {
                assert!(display_cols(seg) <= *width, "段超宽：{seg:?}");
            }
        }
    }

    /// 折行后的内容必须与不折行时逐字相同（不丢字符、不改字符）。
    #[test]
    fn wrap_bytes_preserves_content() {
        for data in tricky_data() {
            for width in [1usize, 2, 3, 5, 8, 40] {
                let segs = wrap_bytes(&data, width);
                assert_eq!(
                    segs.concat(),
                    visible_text(&data),
                    "内容不一致：{data:?} @ {width}"
                );
            }
        }
    }

    /// 宽字符遇到放不下的余数时只能整体挪到下一行，于是那一行会空出几列：
    /// 段数因此**多于** ceil(列宽 / width)。这是「不劈宽字符」的必然代价 ——
    /// 想让段数严格等于那个公式，就只能把汉字劈成两半，那更糟。
    ///
    /// 所以界面要统计「这条记录占几行」时，应当用折行结果的长度，
    /// 而不是 ceil(列宽 / width) 这个公式。
    #[test]
    fn wrap_bytes_wide_chars_may_waste_columns() {
        let data = "温度数据".as_bytes();
        let cols = display_width(data); // 8 列
        let segs = wrap_bytes(data, 3); // 奇数宽度：每行放得下一个汉字
        assert_eq!(segs, vec!["温", "度", "数", "据"]);
        assert!(segs.len() > cols.div_ceil(3), "这里必须比公式多，否则测试没意义");
        assert_eq!(segs.concat(), "温度数据");
        // 宽字符一个都没被劈开
        for seg in &segs {
            assert_eq!(seg.chars().count(), 1);
        }
    }

    /// wrap_bytes 与 wrap_display 对同一段文本给出一致的折法（普通文本）。
    #[test]
    fn wrap_bytes_agrees_with_wrap_display() {
        for data in [b"hello world".as_slice(), b"abcdefghij".as_slice(), b"a b c".as_slice()] {
            let text = visible_text(data);
            for width in [1usize, 2, 3, 4, 7, 20] {
                assert_eq!(
                    wrap_bytes(data, width),
                    wrap_display(&text, width),
                    "两种折法不一致：{data:?} @ {width}"
                );
            }
        }
    }

    /// 统计行数必须与实际渲染出来的段数一致，否则滚动会错位。
    #[test]
    fn wrap_row_count_matches_rendered_segments() {
        // 口径：按逻辑行折行后求和 —— 与 build_text_view / 界面渲染同源。
        for data in tricky_data() {
            for width in [1usize, 2, 3, 5, 8, 40] {
                let view = build_text_view(&data);
                let expected: usize = view
                    .lines
                    .iter()
                    .map(|l| wrap_display(l, width).len())
                    .sum();
                assert_eq!(
                    wrap_row_count(&data, width),
                    expected,
                    "行数与逐逻辑行折出的段数对不上：{data:?} @ {width}"
                );
            }
        }
    }

    /// `wrap_row_count` 与 `wrap_bytes` 的口径不同：一个按逻辑行折，
    /// 一个把整段当成一条流折（换行显示成 `^J`）。
    ///
    /// 两者在**单行**数据上必须一致；多行数据上 `wrap_bytes` 会少算行数。
    #[test]
    fn wrap_row_count_and_wrap_bytes_differ_on_multiline_data() {
        // 单行：两者一致。
        let single = b"0123456789";
        for width in [1usize, 3, 5, 20] {
            assert_eq!(
                wrap_row_count(single, width),
                wrap_bytes(single, width).len(),
                "单行数据上两者应一致 @ {width}"
            );
        }
        // 多行：逻辑行有几条就至少占几行。
        assert_eq!(wrap_row_count(b"ab\ncd", 40), 2, "两条逻辑行至少占两行");
        assert_eq!(wrap_row_count(b"line1\nline2\nline3", 40), 3);
        assert_eq!(wrap_row_count(b"A\r\n\xff\x00", 40), 2);
        // 末尾换行不产生空行。
        assert_eq!(wrap_row_count(b"ab\n", 40), 1);
    }

    // ------------------------------------------------------------------
    // 「只数行数」与「切出各段」必须同源。
    //
    // 界面用行数算高度、用切分画内容：两者一旦漂移，
    // 滚到底就会缺一截（或多出空行）。所以这两个函数必须共用
    // 同一套贪心规则（wrap_scan / Greedy）。
    // ------------------------------------------------------------------

    /// 刁钻的显示文本：空串、纯 ASCII、纯中文、宽字符、控制符、折叠标记。
    fn tricky_texts() -> Vec<String> {
        vec![
            String::new(),
            "plain ascii".into(),
            "abc".into(),
            "温度传感器数据".into(),
            "ab温度cd".into(),
            "温".into(),                        // 宽度 1 时放不下的宽字符
            "温 度".into(),                     // 宽字符之间夹空格
            "^M^J".into(),                      // 控制符渲染成 ^X
            "^[mERROR^[m: 温度过高".into(),      // 折叠后的转义标记
            "混合 mixed 温度 123".into(),
            "　全角空格".into(),
        ]
    }

    /// 只数行数的 wrap_rows 必须与真正切段出来的 wrap_display 完全一致。
    #[test]
    fn wrap_rows_matches_wrap_display_on_tricky_texts() {
        for t in tricky_texts() {
            let cols = display_cols(&t);
            for width in [0usize, 1, 2, 3, 4, 5, 7, 12, 40] {
                assert_eq!(
                    wrap_rows(&t, cols as u32, width),
                    wrap_display(&t, width).len(),
                    "行数不一致：{t:?} @ {width}"
                );
            }
        }
    }

    /// 宽字符放不下时整体挪到下一行，行数会比「列宽 ÷ 宽度」**多**。
    ///
    /// 这正是不能拿除法公式当行数用的原因：算少了就滚不到底。
    #[test]
    fn wrap_rows_counts_the_wasted_columns_of_wide_chars() {
        let t = "温度数据"; // 8 列
        let cols = display_cols(t);
        assert_eq!(cols, 8);
        // 宽度 3：每行只放得下一个汉字 → 4 行，比 ceil(8/3)=3 多一行
        assert_eq!(wrap_rows(t, cols as u32, 3), 4);
        assert!(wrap_rows(t, cols as u32, 3) > cols.div_ceil(3));
        // 与切段结果一致
        assert_eq!(wrap_rows(t, cols as u32, 3), wrap_display(t, 3).len());
    }

    /// 宽度 0 表示不折行；列宽放得下时就是一行。
    #[test]
    fn wrap_rows_handles_degenerate_widths() {
        assert_eq!(wrap_rows("abc", 3, 0), 1, "宽度 0 表示不折行");
        assert_eq!(wrap_rows("", 0, 10), 1, "空行在界面上仍然占一行");
        assert_eq!(wrap_rows("abc", 3, 3), 1, "正好放得下就是一行");
        assert_eq!(wrap_rows("abc", 3, 2), 2);
    }

    /// 逐条逻辑行数出来的行数，必须与按字节口径数出来的完全一样。
    ///
    /// 界面走的是「显示文本」这条路径，而日志/统计走的是「原始字节」那条；
    /// 两条路径给出的行数必须一致，否则同一段数据在两处高度不同。
    #[test]
    fn wrap_rows_over_lines_matches_the_byte_level_counter() {
        for data in tricky_data() {
            let view = build_text_view(&data);
            for width in [1usize, 2, 3, 5, 8, 40] {
                let expect: usize = view
                    .lines
                    .iter()
                    .zip(view.cols.iter())
                    .map(|(l, &c)| wrap_rows(l, c, width))
                    .sum();
                assert_eq!(
                    expect,
                    wrap_row_count(&data, width),
                    "行数与字节口径不一致：{data:?} @ {width}"
                );
                // 与逐行切段的段数之和也要一致
                let segs: usize = view.lines.iter().map(|l| wrap_display(l, width).len()).sum();
                assert_eq!(expect, segs, "行数与切段数不一致：{data:?} @ {width}");
            }
        }
    }

    /// 忽略大小写的匹配：ASCII 字母不分大小写，其余照旧。
    #[test]
    fn case_insensitive_contains() {
        assert!(contains_ignore_ascii_case("ERROR: 温度过高", "error"));
        assert!(contains_ignore_ascii_case("error: 温度过高", "Error"));
        assert!(contains_ignore_ascii_case("ErRoR", "eRrOr"));
        assert!(!contains_ignore_ascii_case("警告", "error"));
        // 中文没有大小写，按原样比较
        assert!(contains_ignore_ascii_case("温度过高", "温度"));
        assert!(!contains_ignore_ascii_case("温度过高", "温度低"));
        // 空 needle 没有意义
        assert!(!contains_ignore_ascii_case("abc", ""));
        assert!(!contains_ignore_ascii_case("", "a"));
    }

    /// 忽略大小写的高亮区间：字符下标、不重叠。
    #[test]
    fn case_insensitive_match_ranges() {
        assert_eq!(
            match_ranges_ignore_ascii_case("ERROR error", "error"),
            vec![(0, 5), (6, 11)]
        );
        // 字符下标不是字节下标：「温」在 UTF-8 里占 3 字节
        assert_eq!(match_ranges_ignore_ascii_case("温ERROR", "error"), vec![(1, 6)]);
        // 不重叠：命中后从末尾继续找
        assert_eq!(match_ranges_ignore_ascii_case("aaaa", "AA"), vec![(0, 2), (2, 4)]);
        assert_eq!(match_ranges_ignore_ascii_case("aaaa", "aaa"), vec![(0, 3)]);
        // 中文照旧
        assert_eq!(
            match_ranges_ignore_ascii_case("温度传感器温度", "温度"),
            vec![(0, 2), (5, 7)]
        );
        // 空 needle 没有意义
        assert_eq!(
            match_ranges_ignore_ascii_case("abc", ""),
            Vec::<(usize, usize)>::new()
        );
        assert_eq!(
            match_ranges_ignore_ascii_case("", "a"),
            Vec::<(usize, usize)>::new()
        );
        // 与大小写敏感版一致的情形：needle 里没有字母
        assert_eq!(
            match_ranges_ignore_ascii_case("温度 123", "123"),
            match_ranges("温度 123", "123")
        );
        // 命中区间按字符切片能还原出 needle（忽略大小写）
        let hay = "温ABCabc";
        for (s, e) in match_ranges_ignore_ascii_case(hay, "abc") {
            let chars: Vec<char> = hay.chars().collect();
            let got: String = chars[s..e].iter().collect();
            assert!(got.eq_ignore_ascii_case("abc"), "命中区间应当还原出关键字：{got}");
        }
    }

    /// 大小写敏感版的行为不能因为去重而改变。
    #[test]
    fn case_sensitive_match_ranges_is_unchanged() {
        assert_eq!(match_ranges("abcabc", "bc"), vec![(1, 3), (4, 6)]);
        assert_eq!(match_ranges("温abc", "b"), vec![(2, 3)]);
        assert_eq!(match_ranges("AAAA", "a"), Vec::<(usize, usize)>::new());
        assert_eq!(match_ranges("aaaa", "aa"), vec![(0, 2), (2, 4)]);
        assert_eq!(match_ranges("abc", ""), Vec::<(usize, usize)>::new());
    }
}
