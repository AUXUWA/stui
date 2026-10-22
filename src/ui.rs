//! 纯渲染层：只读取 `App` 状态并画到终端，不做业务决策。

use std::cell::Cell;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Dir, Focus, Row, Modal, ROWS};
use crate::fmt;
use crate::port::View;

const ACCENT: Color = Color::Rgb(94, 234, 212);
const ACCENT_DIM: Color = Color::Rgb(45, 122, 116);
const RX_COLOR: Color = Color::Rgb(125, 195, 255);
const TX_COLOR: Color = Color::Rgb(255, 196, 122);
const SYS_COLOR: Color = Color::Rgb(160, 170, 190);
const DIM: Color = Color::Rgb(120, 130, 150);
const OK: Color = Color::Rgb(126, 226, 140);
const WARN: Color = Color::Rgb(250, 204, 100);
const ERR: Color = Color::Rgb(255, 120, 120);
const SEL_BG: Color = Color::Rgb(35, 45, 60);
const HEX_ASCII: Color = Color::Rgb(214, 140, 214);
const PANEL_BORDER: Color = Color::Rgb(70, 80, 100);
/// 过滤命中的高亮底色：比正文亮得多，一眼能看出命中的是哪几个字。
const HILITE: Color = Color::Rgb(255, 226, 128);
/// 当前记录（屏幕顶上顶着的那条）的底色。
///
/// 过滤命中导航会把视图挪到某一条命中上，用户得一眼看出自己停在哪一条；
/// 底色比高亮暗得多，不会跟关键字高亮抢眼。
const CURRENT_BG: Color = Color::Rgb(28, 38, 56);

/// 行首前缀列宽：`HH:MM:SS.mmm ` (13) + `RX`/`TX`/`··` (3) + 箭头与空格 (2)。
/// 必须与 `prefix_spans` 实际写入的列数一致，否则超长行会把这个差值吃掉。
/// 关掉毫秒时时间戳短 4 列，用空格补齐，数据列才不会左右跳。
const PREFIX: usize = 17;
/// 左栏标签列宽（含标签后的间隔）。
const LABEL_COLS: usize = 10;
/// 接收缓冲上限（与 `app` 里的 `MAX_BYTES` 一致；那边是私有常量）。
const MAX_BUFFER_BYTES: u64 = 4 * 1024 * 1024;
/// 十六进制字符表（与大写的 `format!("{:02X}")` 完全等价）。
const HEX_DIGITS: [u8; 16] = *b"0123456789ABCDEF";

/// 每个字节预展开成两个十六进制字符。
///
/// 逐字节 `format!("{:02X}")` 会为每个字节分配一次临时字符串，
/// HEX 视图刷一屏就是几千次分配；查表后只剩字符串本身的增长。
const HEX_PAIRS: [[u8; 2]; 256] = {
    let mut table = [[0u8; 2]; 256];
    let mut i = 0usize;
    while i < 256 {
        table[i] = [HEX_DIGITS[i >> 4], HEX_DIGITS[i & 0x0F]];
        i += 1;
    }
    table
};

fn dir_color(d: Dir) -> Color {
    match d {
        Dir::Rx => RX_COLOR,
        Dir::Tx => TX_COLOR,
        Dir::Sys => SYS_COLOR,
    }
}

/// 按**显示列宽**截断字符串，超出部分用 `…` 收尾。
///
/// 中文一个字占两列：按字符个数截断会把预留宽度算少，
/// 值就会顶出面板（连选中的 `◀▶` 提示一起被裁掉）。
/// 语义与 [`clip_columns`] 一致，这里保留一个独立名字是为了读起来更直白。
fn truncate(s: &str, w: usize) -> String {
    clip_columns(s, w)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 64 || area.height < 14 {
        let msg = "窗口太小：请放大到至少 64x14";
        f.render_widget(
            Paragraph::new(msg).alignment(Alignment::Center).wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    header(f, app, rows[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(38), Constraint::Min(0)])
        .split(rows[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        // 快捷键面板 9 行（7 行内容 + 上下边框）：数据区焦点下要摆下 7 条。
        .constraints([Constraint::Min(5), Constraint::Length(9)])
        .split(body[0]);
    params_pane(f, app, left[0]);
    hints_pane(f, app, left[1]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(body[1]);
    right_pane(f, app, right[0], right[1]);
    // 导航请求只在终端页被消费：非终端页签上把它清掉，
    // 别让一个悬着的请求在用户切回终端页时突然生效。
    if app.tab != 0 {
        app.nav_dir = None;
    }

    status_bar(f, app, rows[2]);

    if app.modal.is_some() {
        modal(f, app, area);
    } else {
        // 弹窗关掉了：把详情滚动位置复位，下次打开从头看。
        DETAIL_KEY.with(|k| k.set((usize::MAX, false)));
        DETAIL_SCROLL.with(|s| s.set(0));
    }
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" stui ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(" 串口终端工具 ")];
    if app.settings_drift() {
        spans.push(Span::styled(
            " 参数已修改 ",
            Style::default().fg(Color::Black).bg(WARN).add_modifier(Modifier::BOLD),
        ));
    }
    if app.connected() {
        spans.push(Span::styled(" ● 已连接 ", Style::default().fg(OK)));
    } else {
        spans.push(Span::styled(" ○ 未连接 ", Style::default().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn block(title: &str, focused: bool) -> Block<'static> {
    Block::default().borders(Borders::ALL).border_style(Style::default().fg(if focused { PANEL_BORDER } else { Color::Rgb(55, 62, 78) })).title(Span::styled(
        format!(" {title} "),
        Style::default().fg(if focused { ACCENT } else { DIM }),
    ))
}

// ---------- 左栏 ----------

fn params_pane(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Params;
    let inner_w = area.width.saturating_sub(2) as usize;

    let mut items: Vec<ListItem> = Vec::with_capacity(ROWS.len());
    for (i, (row, (label, _))) in ROWS.iter().zip(crate::app::ROW_LABEL).enumerate() {
        let selected = i == app.sel;
        let (value, style) = describe(app, *row, selected && focused);
        let mark = if selected { "▸ " } else { "  " };
        // 标签固定 LABEL_COLS 列，值紧随其后，视觉上对齐成一列。
        let label_span = Span::styled(
            format!("{mark}{label}"),
            Style::default().fg(if selected && focused { ACCENT } else { DIM }),
        );
        // 选中时给 "◀▶" 预留 3 列，否则长设备名会把可修改的提示顶掉，
        // 用户就不知道 ←→ 还能切值了。
        let reserve = if selected && focused { 3 } else { 0 };
        // 「◀▶」提示在值的末尾：先把它摘出来，长值才不会把它挤掉。
        let (base, hint) = match value.strip_suffix(" ◀▶") {
            Some(b) => (b.to_string(), " ◀▶"),
            None => (value, ""),
        };
        let value_span = Span::styled(
            format!(
                "{}{}",
                truncate(&base, inner_w.saturating_sub(2 + LABEL_COLS + reserve)),
                hint
            ),
            style,
        );
        let spans = if selected {
            vec![label_span.style(Style::default().add_modifier(Modifier::BOLD)), value_span]
        } else {
            vec![label_span, value_span]
        };
        items.push(ListItem::new(Line::from(spans)));
    }

    let list = List::new(items)
        .block(block("串口参数", focused))
        .highlight_style(Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol("");
    let mut state = ListState::default().with_selected(Some(app.sel));
    f.render_stateful_widget(list, area, &mut state);
}

/// 返回该行的取值文本与其配色。
fn describe(app: &App, row: Row, active: bool) -> (String, Style) {
    let arrows = if active { " ◀▶" } else { "" };
    let white = Style::default().fg(Color::White);
    match row {
        Row::Status => {
            if let Some(l) = &app.link {
                (
                    format!("● {} {}", l.cfg.port, l.cfg.summary()),
                    Style::default().fg(OK),
                )
            } else {
                ("○ 未连接".to_string(), Style::default().fg(DIM))
            }
        }
        Row::Port => {
            let v = match app.ports.get(app.port_idx) {
                Some(p) if !p.desc.is_empty() => format!("{} · {}{}", p.port, p.desc, arrows),
                Some(p) => format!("{}{}", p.port, arrows),
                None => "（无）按 r 刷新".to_string(),
            };
            (v, white)
        }
        Row::Baud => (format!("{}{}", crate::port::BAUDS[app.baud_idx], arrows), white),
        Row::Bits => (format!("{}{}", app.bits.label(), arrows), white),
        Row::Parity => (format!("{}{}", app.parity.label(), arrows), white),
        Row::Stops => (format!("{}{}", app.stops.label(), arrows), white),
        Row::Flow => (format!("{}{}", app.flow.label(), arrows), white),
        Row::View => (format!("{}{}", app.view.label(), arrows), white),
        Row::Interval => (
            format!("{} ms{}{}", app.interval_ms, arrows, if app.repeating { " ⟳" } else { "" }),
            white,
        ),
        Row::Toggle => (
            if app.connected() { "■ 关闭串口" } else { "▶ 打开串口" }.to_string(),
            if app.connected() {
                Style::default().fg(WARN)
            } else {
                Style::default().fg(OK)
            },
        ),
        Row::Refresh => ("按 r 重新枚举".to_string(), white),
    }
}

fn hints_pane(f: &mut Frame, app: &App, area: Rect) {
    // 排除模式里「命中」与「显示」正好相反：n/N 跳的是**屏幕上有的**记录，
    // 排除模式下就是没命中关键字的那些 —— 提示行要跟着说清楚。
    let nav_hint = if app.filter_invert {
        "n/N 跳未命中"
    } else {
        "n/N 跳命中"
    };
    let lines: Vec<String> = match (app.focus, app.tab) {
        (Focus::Params, _) => vec![
            "↑↓  选择     ←→  修改".into(),
            "Enter  打开/关闭   r  刷新".into(),
            "Tab → 输入框（可直接打字）".into(),
            "c 清接收   / 过滤   p 暂停".into(),
            "Ctrl+R 重连  Ctrl+T 毫秒".into(),
            "F1 帮助   Shift+F 刷新率   q 退出".into(),
        ],
        (Focus::Input, 0) => vec![
            "直接输入，Enter 发送".into(),
            "Tab → 数据区 / ← 参数栏".into(),
            "↑↓ PgUp/PgDn 也能滚动数据".into(),
            "Ctrl+↑↓ 翻发送历史".into(),
            "Ctrl+H HEX  Ctrl+L 循环  Esc 发 ESC".into(),
            "Ctrl+End 恢复自动跟随".into(),
        ],
        (Focus::Stream, 0) => vec![
            "↑↓ PgUp/PgDn  滚动（到底自动跟随）".into(),
            "End / Ctrl+End / a  恢复自动跟随".into(),
            "p  暂停跟随（数据照收）".into(),
            "/ 关键字过滤   v 文本/HEX   w 折行".into(),
            "I 忽略大小写   ! 排除匹配".into(),
            format!("{nav_hint}   Enter 详情   l 记录"),
            "x 全清   c 只清接收   Ctrl+R 重连".into(),
        ],
        _ => vec![
            "↑↓ PgUp/PgDn  滚动".into(),
            "1 / 2 / 3 / 4  切换页签".into(),
            "Ctrl+R 重连  Ctrl+T 毫秒".into(),
            "o  打开/关闭串口".into(),
        ],
    };
    let items = lines
        .iter()
        .map(|l| ListItem::new(Line::from(Span::styled(l.clone(), Style::default().fg(DIM)))))
        .collect::<Vec<_>>();
    f.render_widget(List::new(items).block(block("快捷键", false)), area);
}

// ---------- 右栏 ----------

/// 右侧面板是否处于焦点态（输入框档或数据区档）。
fn right_focused(app: &App) -> bool {
    app.focus != Focus::Params
}

fn right_pane(f: &mut Frame, app: &mut App, tabs_area: Rect, content: Rect) {
    let titles = [" 1 终端 ", " 2 日志 ", " 3 统计 ", " 4 帮助 "];
    let tabs = Tabs::new(titles)
        .select(app.tab)
        .divider(Span::styled("│", Style::default().fg(PANEL_BORDER)))
        .highlight_style(Style::default().fg(ACCENT).bg(SEL_BG).add_modifier(Modifier::BOLD))
        .style(Style::default().fg(DIM));
    f.render_widget(tabs, tabs_area);

    match app.tab {
        0 => terminal_tab(f, app, content),
        1 => log_tab(f, app, content),
        2 => stats_tab(f, app, content),
        _ => help_tab(f, app, content),
    }
}

/// HEX 视图每行放几个字节，以及是否显示偏移与 ASCII 栏。
///
/// 行格式：`偏移(5) + 十六进制(hex_cols) + 两空格 + |ASCII|(n+4)`。
///
/// 判定用的是**真实列数**：早先按 `3n+8` 估算，少算了偏移列与 ASCII 段的宽度，
/// 选出来的排版根本放不下，ASCII 栏会被面板从中间裁掉（连右竖线都没了）。
///
/// 三档的阈值各不相同，一档比一档窄：
/// 先去掉 ASCII 栏，再去掉偏移，最后才是纯十六进制列表。
fn hex_layout(width: usize) -> (usize, bool, bool) {
    hex_layout_in(PREFIX, width)
}

/// 与 [`hex_layout`] 一样，但行首前缀的列数可以不是 [`PREFIX`]。
///
/// 详情弹窗里没有行首前缀，宽度全留给内容，所以这里把前缀宽度做成参数。
fn hex_layout_in(prefix: usize, width: usize) -> (usize, bool, bool) {
    for n in [16usize, 8, 4] {
        if prefix + OFFSET_COLS + hex_cols(n) + ascii_cols(n) <= width {
            return (n, true, true);
        }
    }
    for n in [16usize, 8, 4] {
        if prefix + OFFSET_COLS + hex_cols(n) <= width {
            return (n, true, false);
        }
    }
    for n in [16usize, 8, 4] {
        if prefix + hex_cols(n) <= width {
            return (n, false, false);
        }
    }
    (1, false, false)
}

/// 偏移列宽：`0000 ` 形式（4 位十六进制 + 1 个空格）。
const OFFSET_COLS: usize = 5;

/// 十六进制段的列数：每字节 2 列，字节之间 1 列空格，
/// 超过 8 个字节时中间再多一个分组空格。
fn hex_cols(n: usize) -> usize {
    3 * n - 1 + usize::from(n > 8)
}

/// ASCII 段的列数：两个空格 + 左竖线 + 至多 n 列内容 + 右竖线。
///
/// 内容按 `n` 列裁剪（见 [`hex_spans`]），所以这一段最多这么宽。
fn ascii_cols(n: usize) -> usize {
    n + 4
}



/// 数据区的排版参数。
///
/// 「一条记录占几行」与「这条记录画成什么样」必须用同一份参数：
/// 两边的算法一旦分叉，滚动位置就会错位（滚不到底，或底部多出空行）。
struct StreamLayout {
    /// 当前视图：文本还是 HEX。
    view: View,
    /// HEX 视图每行放几个字节。
    per_line: usize,
    /// 数据区的内容宽度（面板内宽减去行首前缀 [`PREFIX`]）。
    content_w: usize,
    /// 是否把超长的行折开显示。
    wrap: bool,
}

/// 一条记录在当前排版下占多少个视觉行。
///
/// 折行开关决定算法，但**统计与渲染必须走同一个口径**：
/// 统计少了行，滚到底就会缺一截；统计多了行，底部会多出空行。
fn record_line_count(rec: &crate::app::Record, layout: &StreamLayout) -> usize {
    match layout.view {
        // HEX 视图每行固定若干字节，宽度早就按面板挑过了（见 `hex_layout`），
        // 折行开关对它没有影响。
        View::Hex => rec.hex_lines(layout.per_line),
        // 不折行：一条逻辑行就是一行，超宽的部分截断并标 `…`。
        View::Text if !layout.wrap => rec.text_lines(),
        // 折行：交给记录自己算 —— 它按宽度缓存结果，宽度不变时是零计算。
        //
        // 之前这里是逐条记录重算一遍（每条逻辑行扫一次字符），
        // 开销随缓冲里的**内容**线性增长：缓冲越大，每帧越慢。
        // 缓存之后每条记录只剩一次查表，开销与内容多少无关。
        View::Text => rec.visual_rows(layout.content_w),
    }
}

/// 一批记录一共占多少行。
fn total_lines(rows: &[usize]) -> usize {
    rows.iter().sum()
}

/// 过滤命中导航：找到下一条 / 上一条命中的记录。
///
/// 返回它在**过滤后**序列里的序号，以及它的行偏移（滚到那里就以它开头）。
/// 找不到就返回 `None`：只清掉导航请求，不动滚动位置。
fn nav_target(
    matched: &[&crate::app::Record],
    rows: &[usize],
    top: usize,
    dir: i8,
) -> Option<(usize, usize)> {
    if matched.is_empty() {
        return None;
    }
    let cur = top.min(matched.len() - 1);
    let target = if dir > 0 {
        cur + 1
    } else if dir < 0 {
        cur.checked_sub(1)?
    } else {
        return None;
    };
    if target >= matched.len() {
        return None;
    }
    // 目标记录的行偏移：它前面那些记录的行数之和。
    let offset: usize = rows[..target].iter().sum();
    Some((target, offset))
}

/// 一条记录在文本视图里的各行，按需产出，**不预先切分**。
///
/// 语义与 `fmt::display_lines` 完全一致：按数据里真实的 `\n` 切行、
/// 去掉 `\r\n` 里的 `\r`，并丢掉末尾换行带来的那个空行。
///
/// 区别只在于不分配：`display_lines` 每调用一次就为整条记录分配一个 Vec，
/// 一条几千行的记录会让单帧多出几千个切片 —— 而界面上只看得到几十行。
struct RecordLines<'a> {
    data: &'a [u8],
    /// 下一次切分的起点（字节下标）。
    pos: usize,
    /// 数据以换行结尾时，末尾那一段空行不产出（与 `display_lines` 的 pop 一致）。
    drop_tail: bool,
    done: bool,
}

impl<'a> RecordLines<'a> {
    fn new(data: &'a [u8]) -> Self {
        RecordLines {
            data,
            pos: 0,
            drop_tail: data.last() == Some(&b'\n'),
            done: false,
        }
    }
}

impl<'a> Iterator for RecordLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        match self.data[self.pos..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let i = self.pos + rel;
                // 去掉 \r\n 里的 \r，它属于行结束符
                let end = if i > self.pos && self.data[i - 1] == b'\r' { i - 1 } else { i };
                let piece = &self.data[self.pos..end];
                self.pos = i + 1;
                Some(piece)
            }
            None => {
                self.done = true;
                if self.drop_tail {
                    None
                } else {
                    Some(&self.data[self.pos..])
                }
            }
        }
    }
}

/// HEX 视图下每行若干字节的分块。
///
/// 空数据也要占一行：与 `Record::hex_lines` 的 `max(1)` 保持一致，
/// 否则总行数与实际画出来的行数会对不上。
fn hex_chunks<'a>(data: &'a [u8], per_line: usize) -> impl Iterator<Item = &'a [u8]> {
    let per = per_line.max(1);
    std::iter::once(data).take(usize::from(data.is_empty())).chain(data.chunks(per))
}

/// 终端页：上面是收发合一的流，下面直接是发送输入框。
fn terminal_tab(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Input;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3), Constraint::Length(3)])
        .split(area);
    stream_pane(f, app, rows[0]);
    input_pane(f, app, rows[1], focused);
    input_status_pane(f, app, rows[2]);
}

fn stream_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let mut title = format!(
        " 收发 · {} · {} · 缓冲 {} ",
        app.view.label(),
        if app.paused {
            "已暂停"
        } else if app.auto_scroll {
            "自动滚动"
        } else {
            "手动滚动"
        },
        fmt::human_bytes(app.buffered)
    );
    // 过滤态必须一眼可见，否则「数据不见了」会被当成程序坏了。
    // 开关标记紧跟在关键字后面：只看关键字分不清当前是「显示命中的」
    // 还是「排除命中的」，这两种模式看到的东西正好相反。
    let flags = app.filter_flags();
    if !app.filter.is_empty() {
        let marks = if flags.is_empty() {
            String::new()
        } else {
            format!("[{flags}]")
        };
        title = format!("过滤 {}{} · ", app.filter, marks) + &title[1..];
    } else if !flags.is_empty() {
        // 关键字为空时两个开关不起作用，但用户可能刚按过开关：
        // 让它留在标题里，免得「按了没反应」。
        title = format!("过滤选项 [{flags}] · ") + &title[1..];
    }
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    if inner_w <= PREFIX || inner_h == 0 {
        f.render_widget(block(&title, right_focused(app)), area);
        return;
    }

    let text_w = inner_w - PREFIX;
    let (per_line, show_offset, show_ascii) = hex_layout(inner_w);
    let layout = StreamLayout {
        view: app.view,
        per_line,
        content_w: text_w,
        wrap: app.wrap,
    };
    // 一帧只做**一次**过滤扫描：命中的记录先收成一份，
    // 行数统计与绘制都复用它（早先两边各扫一遍，字节扫描开销翻倍）。
    // 这一份是**缓存**出来的：详情弹窗要同一条记录与同一个总数，
    // 缓存让一帧只扫一遍，而不是数据区一遍、弹窗里再两遍。
    let matched: Vec<&crate::app::Record> = app.filtered_records();
    // 每条记录占几行：统计与画行共用 `record_line_count`，
    // 两边算法一致，滚动位置才不会错位。
    let rows: Vec<usize> = matched.iter().map(|r| record_line_count(r, &layout)).collect();
    let total = total_lines(&rows);
    let max_top = total.saturating_sub(inner_h);
    // 过滤命中导航：先把起点算好，写回 App 要等借用结束（渲染之后）。
    let nav = match app.nav_dir {
        Some(d) => nav_target(&matched, &rows, app.top_record, d),
        None => None,
    };
    // `clamp_scroll` 要改 app，而 `matched` 还借着 app：
    // 先按同一套规则算出起点，渲染结束后再把结果写回 app。
    let start = match nav {
        Some((_, offset)) => offset.min(max_top),
        None => app.clamped_scroll(max_top),
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    // `skipped` 始终是「当前这条记录的行偏移」：可视范围之外的记录整条跳过，
    // 画完一条也必须推进 —— 不推进的话，后面的记录会拿同一条记录的偏移去定位，
    // 每条记录的第一行被切掉、行数少的记录甚至整条看不见。
    let mut skipped = 0usize;
    // 屏幕顶上顶着的那条记录（过滤后序列里的序号）。
    let mut first_visible: Option<usize> = None;
    'outer: for (idx, rec) in matched.iter().enumerate() {
        // 用缓存的行数定位：可视范围之外的记录整条跳过，绝不切分。
        let n = rows[idx];
        if skipped + n <= start {
            skipped += n;
            continue;
        }
        // 这条记录里有 begin 行落在可视范围之上（滚动到记录中间时会遇到）。
        // 后续记录的行偏移已经超过 start，begin 必须是 0：
        // 用减法会下溢 panic，所以这里用饱和减法。
        let begin = start.saturating_sub(skipped);
        // 顶上那条记录要有标记，用户才知道自己停在哪一条。
        let current = first_visible.is_none();
        if current {
            first_visible = Some(idx);
        }
        match layout.view {
            // 折行：超长的逻辑行按内容宽度拆成若干段，一段一行。
            View::Text if layout.wrap => {
                let view = rec.text_view();
                let mut skip = begin;
                // `zip` 而不是 `enumerate` + `cols.get(i)`：
                // `Record::compute_visual_rows` 就是这么遍历的，
                // 两边遍历的逻辑行集合必须一模一样，否则行数会差几行。
                for (i, (line, &cols)) in view.lines.iter().zip(view.cols.iter()).enumerate() {
                    // 这条逻辑行占几行：与 `Record::visual_rows` 用的是同一个
                    // 函数（`fmt::wrap_rows`），统计与渲染才不会分家。
                    let rows_here = fmt::wrap_rows(line, cols, layout.content_w);
                    if skip >= rows_here {
                        // 整条逻辑行都在可视范围之上：连折行都不用折。
                        skip -= rows_here;
                        continue;
                    }
                    let segs = fmt::wrap_display(line, layout.content_w);
                    // 段数就是这条逻辑行实际画出来的行数，必须与上面统计的一致：
                    // 不一致说明 `Record::visual_rows` 与 `fmt::wrap_display`
                    // 的口径分家了，滚动会错位。
                    debug_assert_eq!(
                        segs.len(),
                        rows_here,
                        "折行段数与行数统计不一致：{} 段 vs {rows_here} 行",
                        segs.len()
                    );
                    // 命中区间按整条逻辑行算，再映射到各折行段：
                    // 关键字被折行边界切开时也能高亮得出来。
                    let hl = highlight_spans_wrapped(
                        line,
                        &segs,
                        &app.filter,
                        app.filter_ci,
                        Style::default().fg(dir_color(rec.dir)),
                    );
                    for (j, (_seg, hspans)) in segs.iter().zip(hl).enumerate() {
                        if j < skip {
                            continue; // 可视范围之上的那一段
                        }
                        skip = 0;
                        if lines.len() >= inner_h {
                            break 'outer;
                        }
                        // 只有记录的第一行带行首前缀，其余行缩进对齐：
                        // 同一条记录的每一行都带时间戳，会看着像好几条记录。
                        let is_head = i == 0 && j == 0;
                        let mut spans = if is_head {
                            prefix_spans(rec, app.show_ms)
                        } else {
                            vec![Span::raw(" ".repeat(PREFIX))]
                        };
                        spans.extend(hspans);
                        lines.push(mark_current(Line::from(spans), current));
                    }
                }
            }
            // 文本：只在数据真实换行处分行；HEX：固定每行若干字节
            View::Text => {
                let mut it = RecordLines::new(&rec.data);
                let mut i = 0usize;
                for _ in 0..begin {
                    if it.next().is_none() {
                        break;
                    }
                    i += 1;
                }
                for chunk in it {
                    if lines.len() >= inner_h {
                        break 'outer;
                    }
                    let is_head = i == 0;
                    i += 1;
                    let mut spans = if is_head {
                        prefix_spans(rec, app.show_ms)
                    } else {
                        vec![Span::raw(" ".repeat(PREFIX))]
                    };
                    // 超出面板宽度的部分截断并用 … 提示，不擅自折行
                    let (text, truncated) = fmt::clip_text(chunk, text_w);
                    let base = Style::default().fg(if truncated { DIM } else { dir_color(rec.dir) });
                    spans.extend(highlight_spans(&text, &app.filter, app.filter_ci, base));
                    lines.push(mark_current(Line::from(spans), current));
                }
            }
            View::Hex => {
                let mut it = hex_chunks(&rec.data, per_line);
                let mut i = 0usize;
                for _ in 0..begin {
                    if it.next().is_none() {
                        break;
                    }
                    i += 1;
                }
                for chunk in it {
                    if lines.len() >= inner_h {
                        break 'outer;
                    }
                    let is_head = i == 0;
                    let offset = i * per_line;
                    i += 1;
                    let mut spans = if is_head {
                        prefix_spans(rec, app.show_ms)
                    } else {
                        vec![Span::raw(" ".repeat(PREFIX))]
                    };
                    spans.extend(hex_spans(chunk, offset, show_offset, show_ascii, per_line));
                    lines.push(mark_current(Line::from(spans), current));
                }
            }
        }
        // 画完这条记录：推进到下一条记录的行偏移。
        // 少了这一步，后面的记录会继续用上一条的偏移定位（错位）。
        skipped += n;
    }
    if lines.is_empty() {
        // 空结果有三种情形，说错哪一种都会把用户带偏：
        //  1. 本来就没有数据 —— 说「已排除」等于断言了一件没发生的事；
        //  2. 正向模式没命中 —— 说「已排除」会让人以为关键字写错了；
        //  3. 排除模式而且**每条都命中** —— 这时才是真的「全被排除」。
        let total = app.total_record_count();
        let hits = app.filter_hit_count();
        let hint = if total == 0 {
            "  （暂无数据，连接串口后收发都会显示在这里）".to_string()
        } else if !app.filter.is_empty() && app.filter_invert && hits > 0 {
            format!("  （已排除「{}」，没有可显示的记录，按 Esc 取消过滤）", app.filter)
        } else if !app.filter.is_empty() {
            format!("  （没有匹配「{}」的记录，按 Esc 取消过滤）", app.filter)
        } else {
            "  （暂无数据，连接串口后收发都会显示在这里）".to_string()
        };
        lines.push(Line::from(Span::styled(hint, Style::default().fg(DIM))));
    } else if let Some(idx) = first_visible {
        // 标题里写明当前是第几条：光靠底色不够明显。
        title.push_str(&format!(" · 第 {} 条", idx + 1));
    }

    let p = Paragraph::new(lines).block(block(&title, right_focused(app)));
    f.render_widget(p, area);

    // 借用 `matched` 的期间不能改 App，渲染结束后再把收敛结果写回去。
    if let Some((idx, offset)) = nav {
        // 导航是一次手动定位：自动跟随时画面会立刻被拉回底部，跳转就白跳了。
        app.auto_scroll = false;
        app.scroll_top = offset;
        app.top_record = idx;
    }
    // 导航请求只消费一次：找没找到都不再留着。
    app.nav_dir = None;
    app.clamp_scroll(max_top);
    app.view_h = inner_h;
    // 顶上那条记录的序号：详情弹窗默认打开的就是它。
    if let Some(idx) = first_visible {
        app.top_record = idx;
    }
}

/// 给当前记录的行加一层底色：用户一眼能看出自己停在哪一条。
fn mark_current(line: Line<'static>, current: bool) -> Line<'static> {
    if current {
        line.style(Style::default().bg(CURRENT_BG))
    } else {
        line
    }
}

/// 发送输入框：与收发流同页，边看边发。
fn input_pane(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let inner_w = area.width.saturating_sub(2) as usize;
    // 光标位置与横向滚动都按**显示列宽**算：中文一个字占两列，
    // 按字符个数算会让光标落在字的中间，滚动量也会算少。
    let cursor_col: usize = app
        .input
        .chars()
        .take(app.cursor)
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let scroll_x = cursor_col.saturating_sub(inner_w.saturating_sub(1));
    // 标题随模式变化，顺带把输入格式提示也放进来，省掉一整行状态区
    let title = if app.hex_mode {
        " HEX 输入 ▶ Enter 发送 · 41 42 43 或 0x41-0x42 ".to_string()
    } else {
        " 输入 ▶ Enter 发送 · 支持 \\r \\n \\xNN 转义 · 中文按 UTF-8 ".to_string()
    };
    // 不能开 wrap：开了 wrap 的话 ratatui 会走 WordWrapper，
    // 横向滚动（scroll）根本不生效，光标滚出窗口就再也看不见了。
    let para = Paragraph::new(Line::from(Span::styled(
        app.input.clone(),
        Style::default().fg(Color::White),
    )))
    .block(block(&title, focused))
    .scroll((0, scroll_x as u16));
    f.render_widget(para, area);
    if focused && inner_w > 0 {
        let x = area.x + 1 + ((cursor_col - scroll_x) as u16).min(inner_w as u16 - 1);
        f.set_cursor_position((x, area.y + 1));
    }
}

/// 发送状态：解析结果 + 循环发送开关。
///
/// 这里必须留 3 行（上下边框 + 1 行内容），之前按 2 行分配导致内容区为 0 行、
/// 状态一个字都显示不出来。窄窗口下按可用列数逐段省略，保证关键信息不被截断。
fn input_status_pane(f: &mut Frame, app: &App, area: Rect) {
    let w = area.width.saturating_sub(2) as usize;
    let parse = app.parsed_len();
    let line = match &parse {
        Ok(n) => {
            let mut spans = vec![
                Span::styled("待发送 ", Style::default().fg(DIM)),
                Span::styled(
                    format!("{n} 字节"),
                    Style::default().fg(if *n > 0 { OK } else { DIM }),
                ),
            ];
            // 循环状态按可用列数分档：先只显示开关，再加间隔，最后加提示
            if w >= 22 {
                spans.push(Span::styled(" 循环 ", Style::default().fg(DIM)));
                spans.push(Span::styled(
                    if app.repeating { "开" } else { "关" },
                    Style::default().fg(if app.repeating { WARN } else { DIM }),
                ));
            }
            if w >= 34 {
                spans.push(Span::styled(
                    format!(" {} ms", app.interval_ms),
                    Style::default().fg(if app.repeating { WARN } else { DIM }),
                ));
            }
            // 快捷键提示约占 24 列，宽面板才显示
            if w >= 56 {
                spans.push(Span::styled("   Ctrl+[ / Ctrl+] 调整间隔", Style::default().fg(DIM)));
            }
            Line::from(spans)
        }
        Err(e) => Line::from(vec![
            Span::styled("✗ ", Style::default().fg(ERR)),
            Span::styled(fmt::clip_text(e.as_bytes(), w.saturating_sub(2)).0, Style::default().fg(ERR)),
        ]),
    };
    f.render_widget(Paragraph::new(line).block(block("状态", false)), area);
}

/// 行首前缀：时间戳 + 方向标记。收发同页，用 RX/TX 明确区分来源。
///
/// `show_ms` 关掉时时间戳只有 `HH:MM:SS`（少 4 列），用空格补齐，
/// 保证行首前缀恒为 `PREFIX` 列 —— 否则数据列会随着开关左右跳。
fn prefix_spans(rec: &crate::app::Record, show_ms: bool) -> Vec<Span<'static>> {
    let color = dir_color(rec.dir);
    let (tag, arrow) = match rec.dir {
        Dir::Rx => ("RX", "▾"), // 收到的数据向下流入
        Dir::Tx => ("TX", "▴"), // 发出的数据向上离开
        Dir::Sys => ("··", " "),
    };
    // 时间戳用记录里缓存好的串：每帧为每一行格式化一次时间戳是白费力气
    // （时间戳永不改变，缓存下来就只剩一次分配）。
    let full = rec.stamp_text();
    let ts = if show_ms {
        full.to_string()
    } else {
        // 只到秒就是毫秒串的前 8 个字符：`HH:MM:SS`。
        full.get(..8).unwrap_or(full).to_string()
    };
    // 「 RX」(3) + 箭头与空格(2) 是固定的，剩下的是时间戳与它的补白
    let pad = PREFIX.saturating_sub(UnicodeWidthStr::width(ts.as_str()) + 5);
    vec![
        Span::styled(ts, Style::default().fg(DIM)),
        Span::styled(" ".repeat(pad), Style::default().fg(DIM)),
        Span::styled(format!(" {tag}{arrow} "), Style::default().fg(color)),
    ]
}

/// 命中片段的样式：亮底黑字，和正文一比就能跳出来。
fn hilite_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(HILITE)
        .add_modifier(Modifier::BOLD)
}

/// 时间戳文本：带毫秒时是 `HH:MM:SS.mmm`，不带时是 `HH:MM:SS`。
fn hms_text(st: &fmt::Stamp, show_ms: bool) -> String {
    if show_ms {
        st.hms_ms()
    } else {
        format!("{:02}:{:02}:{:02}", st.h, st.mi, st.s)
    }
}

/// 当前时刻的时间戳文本（按毫秒开关）。
fn now_text(show_ms: bool) -> String {
    hms_text(&fmt::stamp(std::time::SystemTime::now()), show_ms)
}

/// 把一段显示文本按过滤关键字拆成「命中 / 普通」两种样式的 Span。
///
/// 过滤之后满屏都是数据，没有高亮就没法一眼看出命中的是哪几个字。
/// 区间来自 `fmt::match_ranges`（字符下标，不是字节下标）。
/// 高亮作用在**截断之后**的文本上：被截掉的半个词不该再被点亮。
///
/// `ci` 为真时改用忽略大小写的匹配：过滤判命中用的也是同一个口径
/// （见 `App::match_filter`），两边不一致就会出现「算命中却没高亮」。
fn highlight_spans(text: &str, filter: &str, ci: bool, base: Style) -> Vec<Span<'static>> {
    if filter.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let ranges = match_ranges_for(filter, text, ci);
    highlight_ranges(text, &ranges, base)
}

/// 按当前的大小写口径找出关键字的所有命中区间。
///
/// 大小写敏感与不敏感两套比较都在 `fmt` 里，这里只做选择，
/// 免得界面层自己写一份比较逻辑（写一份就多一处可能与判命中分家的地方）。
fn match_ranges_for(filter: &str, text: &str, ci: bool) -> Vec<(usize, usize)> {
    if ci {
        fmt::match_ranges_ignore_ascii_case(text, filter)
    } else {
        fmt::match_ranges(text, filter)
    }
}

/// 按给定的命中区间（字符下标）拆 Span：区间之外用底色，区间之内用高亮。
///
/// 折行时命中区间是「整条逻辑行」上算出来的，映射到段内偏移后走这里。
fn highlight_ranges(text: &str, ranges: &[(usize, usize)], base: Style) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Span<'static>> = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut pos = 0usize;
    for (s, e) in ranges {
        let (s, e) = (*s, *e);
        if s > pos {
            out.push(Span::styled(chars[pos..s].iter().collect::<String>(), base));
        }
        out.push(Span::styled(chars[s..e].iter().collect::<String>(), hilite_style()));
        pos = e;
    }
    if pos < chars.len() {
        out.push(Span::styled(chars[pos..].iter().collect::<String>(), base));
    }
    out
}

/// 折行时的高亮：整条逻辑行算一次命中区间，再按字符偏移映射到各折行段。
///
/// **必须按整条逻辑行算**：关键字正好被折行边界切开时，
/// 逐段各算各的就会一个格子都高亮不出来 ——
/// 而过滤是按整条逻辑行判命中的，于是出现「算命中却看不见高亮」。
///
/// 返回值与 `segs` 一一对应：第 j 段是该段自己的 Span 列表。
fn highlight_spans_wrapped(
    line: &str,
    segs: &[String],
    filter: &str,
    ci: bool,
    base: Style,
) -> Vec<Vec<Span<'static>>> {
    let ranges = if filter.is_empty() {
        Vec::new()
    } else {
        match_ranges_for(filter, line, ci)
    };
    let mut out: Vec<Vec<Span<'static>>> = Vec::with_capacity(segs.len());
    // 当前段在逻辑行里的字符起点（`match_ranges` 给的是整行的字符下标）。
    let mut seg_start = 0usize;
    for seg in segs {
        let seg_end = seg_start + seg.chars().count();
        // 与本段相交的命中区间，换算成段内偏移。
        let local: Vec<(usize, usize)> = ranges
            .iter()
            .filter(|(s, e)| *s < seg_end && *e > seg_start)
            .map(|(s, e)| (s.max(&seg_start) - seg_start, e.min(&seg_end) - seg_start))
            .collect();
        out.push(highlight_ranges(seg, &local, base));
        seg_start = seg_end;
    }
    out
}

/// HEX 视图的一行。
///
/// `ascii_cols` 是 ASCII 段允许占的列数：预览文本按它裁剪，
/// 否则整行会超出面板宽度，右边的竖线被面板切掉。
fn hex_spans(
    chunk: &[u8],
    offset: usize,
    show_offset: bool,
    show_ascii: bool,
    ascii_cols: usize,
) -> Vec<Span<'static>> {
    let mut hex = String::with_capacity(chunk.len() * 3 + 4);
    for (i, b) in chunk.iter().enumerate() {
        if i > 0 {
            hex.push(' ');
            if i == 8 && chunk.len() > 8 {
                hex.push(' ');
            }
        }
        // 查表拼十六进制：逐字节 format! 会为每个字节分配一次临时字符串
        let pair = HEX_PAIRS[*b as usize];
        hex.push(pair[0] as char);
        hex.push(pair[1] as char);
    }
    let mut spans = Vec::with_capacity(3);
    if show_offset {
        spans.push(Span::styled(format!("{:04X} ", offset), Style::default().fg(DIM)));
    }
    spans.push(Span::styled(hex, Style::default().fg(ACCENT)));
    if show_ascii {
        // 预览文本按列宽裁剪（中文一个字两列，不能按字符数算）。
        let preview = fmt::preview_text(chunk);
        spans.push(Span::styled(
            format!("  |{}|", clip_columns(&preview, ascii_cols)),
            Style::default().fg(HEX_ASCII),
        ));
    }
    spans
}

fn log_tab(f: &mut Frame, app: &mut App, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    // 只数条数，不为每条事件分配字符串：日志页一屏只显示几十条，
    // 但事件可能上千条，先全渲染再截取等于白干。
    let total = app.records.iter().filter(|r| r.dir == Dir::Sys).count();
    let max_top = total.saturating_sub(inner_h);
    app.log_scroll = app.log_scroll.min(max_top);

    let ts_w = if app.show_ms { 13 } else { 9 }; // 时间戳 + 一个空格
    let mut body: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    for r in app
        .records
        .iter()
        .filter(|r| r.dir == Dir::Sys)
        .skip(app.log_scroll)
        .take(inner_h)
    {
        let st = r.stamp();
        let ts = hms_text(&st, app.show_ms);
        let msg = String::from_utf8_lossy(&r.data);
        body.push(Line::from(vec![
            Span::styled(format!("{ts} "), Style::default().fg(DIM)),
            Span::styled(
                clip_columns(msg.as_ref(), inner_w.saturating_sub(ts_w)),
                Style::default().fg(SYS_COLOR),
            ),
        ]));
    }
    if body.is_empty() {
        body.push(Line::from(Span::styled("  （暂无事件）", Style::default().fg(DIM))));
    }
    let title = format!(" 事件日志 · 共 {} 条 · 接收 {} 字节 ", total, app.rx_bytes);
    f.render_widget(Paragraph::new(body).block(block(&title, right_focused(app))), area);
}

// ---------- 统计页 ----------

/// 缓冲占用条的长度（格数）。
const BAR_CELLS: usize = 16;
/// 统计页的标签列宽（含标签与值之间的间隔）。
const STATS_LABEL: usize = 12;

/// 按显示列宽左对齐补空格（中文占两列，`format!` 的宽度是按字符数算的）。
fn pad_cols(s: &str, cols: usize) -> String {
    let mut out = s.to_string();
    let w = UnicodeWidthStr::width(s);
    for _ in w..cols {
        out.push(' ');
    }
    out
}

/// 会话时长：小时 / 分钟 / 秒。
fn format_uptime(d: std::time::Duration) -> String {
    let s = d.as_secs();
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h} 小时 {m:02} 分 {sec:02} 秒")
    } else {
        format!("{m} 分 {sec:02} 秒")
    }
}

/// 统计页：把本次会话的量、速率、占用摆成一张对齐的表。
///
/// 这一页只读 `App`，一个数字都不自己算 —— 计数与速率都由 `App` 维护，
/// 这里只负责把它们摆得能一眼看懂（换了视图模式、开了过滤都要看得见）。
fn stats_tab(f: &mut Frame, app: &mut App, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    let cfg = app.cfg();
    let port = app
        .ports
        .get(app.port_idx)
        .map(|p| {
            if p.desc.is_empty() {
                p.port.clone()
            } else {
                format!("{} · {}", p.port, p.desc)
            }
        })
        .unwrap_or_else(|| "（无，按 r 刷新）".to_string());

    // 缓冲占用条：画得出来就画满一格的比例
    let ratio = app.buffered as f64 / MAX_BUFFER_BYTES as f64;
    let filled = ((ratio * BAR_CELLS as f64).round() as usize).min(BAR_CELLS);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_CELLS - filled));
    let percent = (ratio * 100.0).min(100.0);

    let scroll_state = if app.paused {
        "已暂停"
    } else if app.auto_scroll {
        "自动滚动"
    } else {
        "手动滚动"
    };

    let rows: Vec<(&str, String, Color)> = vec![
        ("会话时长", format_uptime(app.uptime()), ACCENT),
        (
            "接收字节",
            format!("{} · {} 包", fmt::human_bytes(app.rx_bytes), app.rx_packets),
            RX_COLOR,
        ),
        ("发送字节", fmt::human_bytes(app.tx_bytes), TX_COLOR),
        (
            "当前速率",
            format!("RX {} · TX {}", app.rate_text(), app.tx_rate_text()),
            SYS_COLOR,
        ),
        (
            "缓冲占用",
            format!(
                "{bar}  {} / {} ({percent:.0}%)",
                fmt::human_bytes(app.buffered),
                fmt::human_bytes(MAX_BUFFER_BYTES),
            ),
            WARN,
        ),
        ("缓冲记录", format!("{} 条", app.records.len()), DIM),
        ("串口端口", port, Color::White),
        (
            "串口参数",
            format!("{} · {}", cfg.summary(), cfg.flow_label()),
            Color::White,
        ),
        (
            "当前视图",
            format!("{} · {scroll_state}", app.view.label()),
            Color::White,
        ),
        (
            "过滤关键字",
            if app.filter.is_empty() {
                "（无）".to_string()
            } else {
                app.filter.clone()
            },
            if app.filter.is_empty() { DIM } else { HILITE },
        ),
        (
            // 过滤的两个开关单独列一行：只看关键字看不出当前是
            // 「显示命中的」还是「排除命中的」，这两种模式结果正好相反。
            "过滤选项",
            {
                let mut opts: Vec<&str> = Vec::new();
                if app.filter_ci {
                    opts.push("忽略大小写");
                }
                if app.filter_invert {
                    opts.push("排除命中");
                }
                if opts.is_empty() {
                    "（无）".to_string()
                } else {
                    opts.join(" · ")
                }
            },
            if app.filter_ci || app.filter_invert { HILITE } else { DIM },
        ),
        (
            "自动重连",
            if app.auto_reconnect { "开" } else { "关" }.to_string(),
            if app.auto_reconnect { OK } else { DIM },
        ),
        ("界面刷新率", format!("{} 帧/秒", app.fps), Color::White),
        (
            "记录文件",
            app.recorder
                .as_ref()
                .map(|r| r.path.display().to_string())
                .unwrap_or_else(|| "未开启".into()),
            Color::White,
        ),
    ];

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        " 本次会话 ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    for (label, value, color) in rows {
        lines.push(Line::from(vec![
            Span::styled(pad_cols(label, STATS_LABEL), Style::default().fg(DIM)),
            Span::styled(
                clip_columns(&value, inner_w.saturating_sub(STATS_LABEL)),
                Style::default().fg(color),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " 计数与速率由 App 维护，本页只做展示",
        Style::default().fg(DIM),
    )));

    // 与帮助页共用滚动位置：页签切换时会被复位
    let max_top = lines.len().saturating_sub(inner_h);
    app.log_scroll = app.log_scroll.min(max_top);
    let body: Vec<Line<'static>> = lines.into_iter().skip(app.log_scroll).collect();
    f.render_widget(Paragraph::new(body).block(block("统计", right_focused(app))), area);
}

fn help_tab(f: &mut Frame, app: &mut App, area: Rect) {
    let groups: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "焦点（Tab 在三档之间循环）",
            vec![
                ("Tab", "参数栏 → 输入框 → 数据区 → 参数栏"),
                ("Ctrl+Tab", "直接跳到数据区"),
                ("Shift+Tab", "反向切换"),
                ("Esc", "不在输入框时，焦点收回参数栏"),
            ],
        ),
        (
            "通用（输入框以外都生效）",
            vec![
                ("Ctrl+R", "自动重连开关：掉线后每 2 秒自动重试"),
                ("Ctrl+T", "时间戳显示到毫秒 / 只到秒"),
                ("1 2 3 4", "切换页签：终端 / 日志 / 统计 / 帮助"),
                ("F1/F2/F3", "直接打开帮助页"),
            ],
        ),
        (
            "输入框（焦点在输入框时）",
            vec![
                ("直接输入", "可打印字符都是待发送数据，中文按 UTF-8"),
                ("Enter", "发送（支持 \\r \\n \\xNN 转义）"),
                ("Esc", "发送 0x1B 字节"),
                ("↑ ↓ PgUp PgDn", "输入框是单行的，上下键直接用来滚动数据区"),
                ("Ctrl+↑↓", "翻发送历史（翻过最新一条会恢复原来的输入）"),
                ("Ctrl+U", "清空输入框"),
                ("Ctrl+H", "文本 / HEX 输入切换"),
                ("Ctrl+L", "循环发送开关（Ctrl+[ / Ctrl+] 调间隔）"),
                ("Ctrl+O", "发送整个文件（输入路径后回车）"),
            ],
        ),
        (
            "数据区（焦点在数据区时）",
            vec![
                ("RX ▾ / TX ▴", "行首标记，区分收到的数据与发出的数据"),
                ("行合并", "设备零散发来的字符会合并成一行，遇到换行或空闲才断开"),
                ("↑ ↓ PgUp PgDn", "滚动；Home 跳到开头，End 到底并恢复跟随"),
                ("a", "自动跟随最新数据"),
                ("p", "暂停 / 恢复跟随（数据照收，画面不动）"),
                ("v", "文本 / HEX 视图切换"),
                ("x", "清空缓冲（收发 + 日志全清）"),
                ("c", "只清空接收区，保留发送记录与事件日志"),
                ("/", "按关键字过滤，Esc 取消过滤"),
                ("I", "过滤忽略 ASCII 大小写（标题里的 i 标记）"),
                ("!", "反向过滤：排除命中关键字的记录（标题里的 ! 标记）"),
                ("w", "长行自动折行：超宽的行折开显示，不再截断"),
                ("n / N", "跳到下一条 / 上一条过滤命中的记录（排除模式下是未命中的记录）"),
                ("Enter", "看当前记录的完整内容（详情弹窗，↑↓ 翻记录）"),
                ("l", "开始 / 停止记录到 logs/ 目录"),
                ("o", "打开 / 关闭串口"),
                ("Shift+F", "切换界面刷新上限（5 / 10 / 20 / 30 帧）"),
            ],
        ),
        (
            "参数栏",
            vec![
                ("↑ ↓", "选择设置项"),
                ("← →", "修改取值（波特率、校验、视图等）"),
                ("Enter", "打开 / 关闭串口"),
                ("r", "重新枚举串口"),
            ],
        ),
    ];

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        "stui · 终端串口工具（q 退出）",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    for (name, items) in groups {
        lines.push(Line::from(Span::styled(
            format!("── {name} "),
            Style::default().fg(ACCENT_DIM).add_modifier(Modifier::BOLD),
        )));
        for (k, d) in items {
            lines.push(Line::from(vec![
                Span::raw("  "),
                // 键位列留 14 列：「↑ ↓ PgUp PgDn」正好 12 个字符，
                // 按 12 列排版会紧贴后面的说明，挤成一团看不出分隔。
                Span::styled(format!("{k:<14}"), Style::default().fg(Color::White)),
                Span::styled(d.to_string(), Style::default().fg(DIM)),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!(
            "本次会话：{} · 接收 {} · 发送 {} · 记录文件 {}",
            format_uptime(app.uptime()),
            fmt::human_bytes(app.rx_bytes),
            fmt::human_bytes(app.tx_bytes),
            app.recorder.as_ref().map(|r| r.path.display().to_string()).unwrap_or_else(|| "未开启".into())
        ),
        Style::default().fg(SYS_COLOR),
    )));

    let inner_h = area.height.saturating_sub(2) as usize;
    let max_top = lines.len().saturating_sub(inner_h);
    app.log_scroll = app.log_scroll.min(max_top);
    let body: Vec<Line<'static>> = lines.into_iter().skip(app.log_scroll).collect();
    f.render_widget(Paragraph::new(body).block(block("帮助", right_focused(app))), area);
}

// ---------- 底部状态条 ----------

/// 状态栏的一段内容。
///
/// 窄窗口下按「从右往左」的顺序整段省略：先丢帧率，再丢速率，
/// 再丢 TX，最后丢 RX —— 左边的连接信息与右边的时钟永远保留。
struct StatusSeg {
    text: String,
    style: Style,
}

impl StatusSeg {
    fn new(text: String, style: Style) -> StatusSeg {
        StatusSeg { text, style }
    }
    /// 这一段占多少显示列宽（中文算两列）。
    fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }
}

/// 串口打不开或掉线时，给出**原因**。
///
/// App 里没有「最近一次错误」这样的字段：失败信息只落在事件日志
/// （SYS 记录）与 3 秒就消失的 toast 里。事件日志是持久的，
/// 所以这里从 SYS 记录里倒着找最近的一条失败信息。
///
/// **这里按消息的字面形状来认**（`读取出错：…` / `打开 … 失败：…`），
/// 等于把 App 的文案约定复制了一份到界面层 —— 是权宜之计：
/// App 一旦改文案，这里就认不出来了。正解是 App 暴露一个
/// `last_error: Option<String>` 字段，界面直接读它。
///
/// 倒着找有两个讲究：
/// - 越新的消息越贴近当前这次连接的问题（旧的失败早就修好了）；
/// - 遇到「已打开」就停：那是本次连接的起点，再往前的失败与当前无关。
///
/// 只看尾部若干条：断开之后不会再有数据进来，失败消息一定在附近；
/// 翻整个缓冲既慢，又可能翻到几小时前的旧账。
fn last_failure_reason(app: &App) -> Option<String> {
    /// 往回看多少条记录。断开/打开失败的消息总在尾部附近。
    const LOOKBACK: usize = 200;
    /// 原因最多占多少列：状态栏宽度有限，原因太长会把别的信息挤没。
    const MAX_COLS: usize = 40;
    let mut closed: Option<String> = None;
    for rec in app.records.iter().rev().take(LOOKBACK) {
        if rec.dir != Dir::Sys {
            continue;
        }
        // SYS 消息一定是 UTF-8；不是就直接跳过，不为一次检查分配字符串。
        let Ok(msg) = std::str::from_utf8(&rec.data) else {
            continue;
        };
        let msg = msg.trim();
        if msg.contains("已打开") || msg.contains("成功") {
            // 越过了本次连接的起点：更早的失败与当前无关
            break;
        }
        // 真正的原因：读出错（拔线 / 设备复位）、打开失败、自动重连失败。
        // 「枚举串口失败」与「日志写入失败」不算 ——
        // 前者是列不出端口，后者是磁盘问题，都与连不连得上无关。
        let is_failure = msg.contains("出错")
            || (msg.contains("失败") && !msg.contains("枚举") && !msg.contains("日志"));
        if is_failure {
            return Some(clip_columns(msg, MAX_COLS));
        }
        if closed.is_none() && msg.contains("已关闭") && !msg.contains("手动断开") {
            closed = Some(clip_columns(msg, MAX_COLS));
        }
    }
    closed
}

fn status_bar(f: &mut Frame, app: &App, area: Rect) {
    let conn = if let Some(l) = &app.link {
        format!(" ● {} · {} · {}流控 ", l.cfg.port, l.cfg.summary(), l.cfg.flow_label())
    } else {
        // 没连上时把原因摆出来：只写「未连接」的话，用户只能自己猜
        // 是设备没插、参数不对还是端口被占。
        //
        // 原因放在连接信息**开头**：状态栏窄了以后是按列裁的，
        // 留在开头才不会被裁掉；「按 o 打开串口」这个操作提示在
        // 左栏参数表里还有一份，丢了不致命。
        match last_failure_reason(app) {
            Some(reason) => format!(" ○ 未连接 · {reason} · 按 o 打开串口 "),
            None => " ○ 未连接 · 按 o 打开串口 ".to_string(),
        }
    };
    // 中间各段按显示顺序排好，越靠右越先被省略
    let segs = vec![
        StatusSeg::new(
            conn,
            Style::default().fg(if app.connected() { Color::White } else { DIM }),
        ),
        StatusSeg::new(
            format!("RX {} / {} 包 · ", fmt::human_bytes(app.rx_bytes), app.rx_packets),
            Style::default().fg(RX_COLOR),
        ),
        StatusSeg::new(
            format!("TX {} · ", fmt::human_bytes(app.tx_bytes)),
            Style::default().fg(TX_COLOR),
        ),
        StatusSeg::new(
            format!("速率 RX {} / TX {} · ", app.rate_text(), app.tx_rate_text()),
            Style::default().fg(SYS_COLOR),
        ),
        StatusSeg::new(format!("{}fps ", app.fps), Style::default().fg(DIM)),
    ];

    let (right_text, right_color): (String, Color) = match &app.toast {
        Some((msg, err, _)) => (format!(" {} ", msg), if *err { ERR } else { OK }),
        None => match &app.recorder {
            Some(_) => (
                format!(" ⏺ {} · {} ", "REC", now_text(app.show_ms)),
                WARN,
            ),
            None => (format!(" {}", now_text(app.show_ms)), DIM),
        },
    };

    // 这一条是所有错误反馈的唯一通道，中文按「显示列宽」而不是字符数来算，
    // 否则中文提示会被终端从右侧裁掉（之前 80 列只剩前半句）。
    let width = area.width as usize;
    let right_w = UnicodeWidthStr::width(right_text.as_str());
    let left_budget = width.saturating_sub(right_w);

    // 逐段省略：从最右边开始砍，砍到放得下为止
    let mut keep = segs.len();
    let mut used: usize = segs.iter().map(|s| s.width()).sum();
    while keep > 1 && used > left_budget {
        keep -= 1;
        used -= segs[keep].width();
    }
    // 剩下的连接信息自己就可能很长（端口名 + 参数），最后再按列裁一刀
    let rest_w: usize = segs[1..keep].iter().map(|s| s.width()).sum();
    let conn_text = clip_columns(&segs[0].text, left_budget.saturating_sub(rest_w));
    let conn_w = UnicodeWidthStr::width(conn_text.as_str());

    let mut spans = vec![Span::styled(conn_text, segs[0].style)];
    for seg in &segs[1..keep] {
        spans.push(Span::styled(seg.text.clone(), seg.style));
    }
    let pad = left_budget.saturating_sub(conn_w + rest_w);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right_text, Style::default().fg(right_color)));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(24, 28, 38))),
        area,
    );
}

/// 按显示列宽裁剪字符串，超出部分用 `…` 收尾。
fn clip_columns(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w <= max_cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > max_cols.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

// ---------- 弹窗 ----------

// 详情弹窗正文的滚动偏移（行）与它对应的记录。
//
// 弹窗的状态放在 `App` 里没有专门的字段，而按键分发也在 `App` 那边，
// 渲染层拿不到「用户滚了多少行」。这里用线程内的静态量保存，
// 由 `detail_scroll` 调整；换记录 / 换 HEX 时复位。
//
// 用线程内而不是进程级：测试是并行的，进程级的偏移会互相串。
thread_local! {
    static DETAIL_SCROLL: Cell<usize> = const { Cell::new(0) };
    /// 上一次画的是哪条记录：内容变了就把滚动位置复位。
    static DETAIL_KEY: Cell<(usize, bool)> = const { Cell::new((usize::MAX, false)) };
    /// 上一次渲染时正文能显示几行：翻页按它来。
    static DETAIL_ROWS: Cell<usize> = const { Cell::new(0) };
}

/// 详情弹窗正文的滚动（行）。
///
/// 按键处理在 `App` 那边，弹窗里的翻页要靠它调用这个入口
/// （每次一行；负数往上翻）。
// 按键分发在 `App` 那边，它还没接这个入口；先放行免得报未使用。
#[allow(dead_code)]
pub fn detail_scroll(delta: isize) {
    DETAIL_SCROLL.with(|c| {
        let v = c.get() as isize + delta;
        c.set(v.max(0) as usize);
    });
}

/// 详情弹窗翻页（`dir` 为 -1 往上一页，1 往下一页）。
///
/// 页高取自上一次渲染时正文实际能显示的行数，
/// 所以终端改大小之后翻页的步长也跟着变。
#[allow(dead_code)]
pub fn detail_scroll_page(dir: isize) {
    let rows = DETAIL_ROWS.with(|r| r.get().max(1)) as isize;
    detail_scroll(dir * rows);
}

fn modal(f: &mut Frame, app: &App, area: Rect) {
    // 详情弹窗的排版与输入框弹窗完全不同，单独画。
    if let Some(Modal::Detail { idx, hex }) = app.modal.as_ref() {
        detail_modal(f, app, area, *idx, *hex);
        return;
    }
    // 两种弹窗布局一致，只是提示语与标题不同
    let (buf, hint, title) = match app.modal.as_ref().unwrap() {
        Modal::FilePath(b) => (
            b.clone(),
            "输入要发送的文件路径（相对当前目录亦可）".to_string(),
            "发送文件（Enter 确认 / Esc 取消）".to_string(),
        ),
        Modal::Filter(b) => {
            // 提示语要跟着两个开关走：反向过滤看到的记录与正向正好相反，
            // 还写着「只显示包含该关键字的记录」会直接把人带偏。
            let mode = if app.filter_invert {
                "排除包含该关键字的记录"
            } else {
                "只显示包含该关键字的记录"
            };
            let ci = if app.filter_ci { "（忽略大小写）" } else { "" };
            let hint = format!("{mode}{ci}（留空 = 不过滤）");
            let title = if app.filter_invert {
                "反向过滤（Enter 应用 / Esc 取消）".to_string()
            } else {
                "过滤（Enter 应用 / Esc 取消）".to_string()
            };
            (b.clone(), hint, title)
        }
        Modal::Detail { .. } => unreachable!("详情弹窗在上面已经处理掉了"),
    };
    let w = (area.width * 3 / 5).max(40);
    let h = 5;
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let body = vec![
        Line::from(Span::styled(hint, Style::default().fg(DIM))),
        Line::from(Span::styled("> ", Style::default().fg(ACCENT))),
        Line::from(Span::styled(buf, Style::default().fg(Color::White))),
    ];
    f.render_widget(Paragraph::new(body).block(block(&title, true)), rect);
}

/// 记录详情弹窗：把一条记录的完整内容摊开看。
///
/// `idx` 是记录在**过滤后**序列里的序号：过滤生效时弹窗里只会出现命中的记录。
/// 文本模式显示完整的显示文本（不截断），HEX 模式显示原始字节的十六进制视图。
fn detail_modal(f: &mut Frame, app: &App, area: Rect, idx: usize, hex: bool) {
    let Some(rec) = app.filtered_record(idx) else {
        // 记录已经没了（缓冲被清、过滤刚变）：宁可不画，也别画一个空壳。
        return;
    };
    // 弹窗约为屏幕的 80% 宽、80% 高。
    let w = ((area.width as usize) * 4 / 5).max(30).min(area.width as usize) as u16;
    let h = ((area.height as usize) * 4 / 5).max(10).min(area.height as usize) as u16;
    let rect = Rect {
        x: area.x + ((area.width as usize - w as usize) / 2) as u16,
        y: area.y + ((area.height as usize - h as usize) / 2) as u16,
        width: w,
        height: h,
    };
    let inner_w = (rect.width as usize).saturating_sub(2);
    let inner_h = (rect.height as usize).saturating_sub(2);

    // 正文：一行一行摊开，内容多到装不下时才需要滚动。
    let mut content: Vec<Line<'static>> = Vec::new();
    if hex {
        // HEX：偏移 + 十六进制 + ASCII 预览，每行几个字节按弹窗宽度自适应。
        let (per, show_offset, show_ascii) = hex_layout_in(0, inner_w);
        for (i, chunk) in rec.data.chunks(per.max(1)).enumerate() {
            let offset = i * per;
            // ASCII 预览的内容宽度就是每行的字节数（整段还要另占 4 列）。
            let spans = hex_spans(chunk, offset, show_offset, show_ascii, per);
            content.push(Line::from(spans));
        }
    } else {
        // 文本：完整的显示形态，按弹窗宽度折行，不截断。
        for line in rec.text_view().lines.iter() {
            for seg in fmt::wrap_display(line, inner_w) {
                content.push(Line::from(Span::styled(seg, Style::default().fg(dir_color(rec.dir)))));
            }
        }
    }
    let content_len = content.len();

    // 换记录 / 换 HEX 时把滚动位置复位：上一条的位置对这一条没有意义。
    DETAIL_KEY.with(|k| {
        if k.get() != (idx, hex) {
            k.set((idx, hex));
            DETAIL_SCROLL.with(|s| s.set(0));
        }
    });
    let skip = DETAIL_SCROLL.with(|s| s.get()).min(content_len.saturating_sub(1));

    let title = format!(
        " 记录详情 · {} · {} · {} 字节 ",
        rec.dir.tag(),
        rec.stamp_text(),
        rec.data.len()
    );
    f.render_widget(Clear, rect);
    // 先画边框与标题，正文再盖在内框里（正文区不含边框，不会压到标题）。
    f.render_widget(Paragraph::new(Vec::<Line>::new()).block(block(&title, true)), rect);

    // 最后一行留给按键提示，正文区比内框矮一行。
    let body_h = inner_h.saturating_sub(1);
    // 记下正文能显示几行：翻页的步长按它来。
    DETAIL_ROWS.with(|r| r.set(body_h));
    let body: Vec<Line<'static>> = content.into_iter().skip(skip).take(body_h).collect();
    let body_rect = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: body_h as u16,
    };
    f.render_widget(Paragraph::new(body), body_rect);

    // 提示行：说清能按什么键，以及现在看到的是第几条、第几行。
    let total = app.filtered_count();
    let hint = format!(
        " ↑↓ 翻记录 · v 切 HEX · Esc/Enter 关闭 · 第 {}/{} 条 · 行 {}/{} ",
        idx + 1,
        total.max(idx + 1),
        (skip + 1).min(content_len.max(1)),
        content_len.max(1)
    );
    let hint_rect = Rect {
        x: rect.x + 1,
        y: rect.y + rect.height.saturating_sub(2),
        width: rect.width.saturating_sub(2),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(DIM)))),
        hint_rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Modal};
    use crate::port::PortEntry;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn demo_app() -> App {
        let mut app = App::new();
        app.ports.push(PortEntry { port: "COM25".into(), desc: "STLink VCP".into() });
        app.feed_rx(b"Hello\r\nWorld\x00");
        app.commit_tx(b"PING");
        app.push_sys("已打开 COM25");
        app
    }

    /// 逐字符回显场景：设备一个字节一次地发，应该合并成一行而不是每字一行。
    fn char_by_char_app() -> App {
        let mut app = App::new();
        app.ports.push(PortEntry { port: "COM25".into(), desc: "STLink VCP".into() });
        for b in b"AT+VERSION?\r\n" {
            app.feed_rx(&[*b]);
        }
        app.commit_tx(b"AT+VERSION?\r\n");
        app
    }

    /// 用 TestBackend 离屏渲染一帧，返回纯文本内容。
    ///
    /// 宽字符（中文等）占两个格子，ratatui 把后一格 reset 成空格，
    /// 因此这里按显示宽度跳过多余的格子，还原终端上的真实观感。
    fn render(app: &mut App, w: u16, h: u16) -> String {
        use unicode_width::UnicodeWidthStr;
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
        term.draw(|f| draw(f, app)).expect("draw");
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0u16;
                for x in 0..buf.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = buf[(x, y)].symbol();
                    line.push_str(sym);
                    skip = UnicodeWidthStr::width(sym).saturating_sub(1) as u16;
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 离屏渲染一帧并把缓冲区本身交出来：要检查配色（高亮）时用。
    fn render_buffer(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
        term.draw(|f| draw(f, app)).expect("draw");
        term.backend().buffer().clone()
    }

    /// 真机固件回归：COM25 上的设备每 2 秒发一行 `[heartbeat] N x 2000 ms ...`。
    /// 用真实抓到的字节、刻意打散成不等长的小块喂进去，结果必须仍然是一行一行。
    #[test]
    fn real_device_heartbeat_lines_stay_separate() {
        let samples = [
            "[heartbeat] 502 x 2000 ms (drop=0, err=0)\r\n",
            "[heartbeat] 503 x 2000 ms (drop=0, err=0)\r\n",
            "[heartbeat] 504 x 2000 ms (drop=0, err=0)\r\n",
        ];
        let mut app = App::new();
        // 模拟最坏情况：每次 read 只回 1 个字节
        for s in samples {
            for b in s.as_bytes() {
                app.feed_rx(&[*b]);
            }
        }
        let text = render(&mut app, 110, 32);
        let rows = text.lines().filter(|l| l.contains("RX▾")).count();
        assert_eq!(rows, 3, "三行心跳应显示为三行，而不是被拆散或粘连");
        for n in ["502", "503", "504"] {
            assert!(
                text.lines().any(|l| l.contains("RX▾") && l.contains(&format!("] {n} x 2000"))),
                "第 {n} 行应完整显示"
            );
        }
    }

    /// 过滤态必须在标题里可见，空结果也要说清楚是"没匹配"而不是"没数据"。
    #[test]
    fn filter_state_is_visible_in_the_pane() {
        let mut app = demo_app();
        app.apply_filter("Hello".into());
        let text = render(&mut app, 120, 30);
        assert!(text.contains("过滤 Hello"), "标题应显示过滤关键字：\n{text}");
        assert!(text.contains("Hello"), "命中的记录仍应显示");

        // 过滤到没有命中时，提示要指向"取消过滤"而不是让人以为串口坏了
        app.apply_filter("不存在的关键字".into());
        let text = render(&mut app, 120, 30);
        assert!(text.contains("没有匹配"), "空结果提示不对：\n{text}");
        assert!(text.contains("Esc"), "空结果提示要告诉用户怎么退出过滤");
    }

    /// 过滤标题必须反映当前的两个开关。
    ///
    /// 只看关键字分不清当前是「显示命中的」还是「排除命中的」——
    /// 这两种模式看到的内容正好相反，用户按 `!` 之后必须一眼看出来。
    #[test]
    fn filter_title_shows_the_active_filter_options() {
        let mut app = demo_app();
        app.apply_filter("Hello".into());
        // 默认两个开关都关：标题里不该出现标记
        let text = render(&mut app, 120, 30);
        assert!(text.contains("过滤 Hello"), "标题应显示过滤关键字：\n{text}");
        assert!(!text.contains("[i]"), "开关没开时不该出现标记：\n{text}");

        // 忽略大小写
        app.toggle_filter_ci();
        let text = render(&mut app, 120, 30);
        assert!(text.contains("过滤 Hello"), "标题应显示过滤关键字：\n{text}");
        assert!(text.contains("[i]"), "忽略大小写应在标题里可见：\n{text}");

        // 反向过滤：两个标记都要在
        app.toggle_filter_invert();
        let text = render(&mut app, 120, 30);
        assert!(text.contains("[i!]"), "两个开关都开时标记应一起出现：\n{text}");
    }

    /// 忽略大小写时，高亮必须跟着命中。
    ///
    /// 过滤判命中与画高亮是两处代码：口径不一致就会出现
    /// 「算命中却一个格子都没高亮」，比不命中还让人困惑。
    #[test]
    fn ignore_case_filter_highlights_the_hit() {
        let mut app = App::new();
        app.feed_rx(b"ERROR boot failed\r\n");
        app.apply_filter("error".into());
        app.toggle_filter_ci();
        assert_eq!(app.stream_filtered().count(), 1, "忽略大小写后应当命中");

        let buf = render_buffer(&mut app, 120, 30);
        let mut cells = 0usize;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].bg == HILITE {
                    cells += 1;
                }
            }
        }
        assert!(cells >= 5, "「ERROR」应被高亮，实际只有 {cells} 格高亮");
    }

    /// 反向过滤（排除模式）：命中的记录不显示，没命中的显示。
    #[test]
    fn invert_filter_hides_the_matching_records() {
        let mut app = App::new();
        app.feed_rx(b"ERROR one\r\n");
        app.feed_rx(b"INFO two\r\n");
        app.apply_filter("ERROR".into());
        assert_eq!(app.stream_filtered().count(), 1, "正向过滤只留命中的");

        app.toggle_filter_invert();
        assert_eq!(app.stream_filtered().count(), 1, "反向过滤只剩没命中的");
        let text = render(&mut app, 120, 30);
        assert!(text.contains("INFO"), "没命中的记录应显示：\n{text}");
        // 关键字本身会出现在标题里，所以要查记录内容而不是关键字。
        assert!(
            !text.contains("ERROR one"),
            "命中的记录应被排除：\n{text}"
        );
    }

    /// 排除模式下没有可显示的记录时，提示文案要说「已排除」。
    ///
    /// 说「没有匹配」会让人以为关键字写错了，其实每一条都匹配、都被排除了。
    #[test]
    fn invert_filter_empty_result_says_excluded() {
        let mut app = App::new();
        app.feed_rx(b"ERROR one\r\n");
        app.feed_rx(b"ERROR two\r\n");
        app.apply_filter("ERROR".into());
        app.toggle_filter_invert();
        let text = render(&mut app, 120, 30);
        assert!(text.contains("已排除"), "排除模式的空结果提示不对：\n{text}");
        assert!(text.contains("没有可显示的记录"), "排除模式的空结果提示不对：\n{text}");
    }

    /// 排除模式但**一条记录都没有**时，提示不能说「已排除」。
    ///
    /// 断言意图：这时根本没排除掉任何东西 —— 关键字谁都没命中，
    /// 说「已排除」等于断言了一件没发生的事。
    #[test]
    fn invert_filter_without_any_record_says_no_data() {
        let mut app = App::new();
        app.apply_filter("ERROR".into());
        app.toggle_filter_invert();
        let text = render(&mut app, 120, 30);
        assert!(text.contains("暂无数据"), "没有数据时应说暂无数据：\n{text}");
        assert!(
            !text.contains("已排除"),
            "没有记录时不该说「已排除」：\n{text}"
        );
    }

    /// 快捷键面板要列出本轮新增的两个键（I / !）。
    ///
    /// 断言意图：面板是**常驻可见**的那份按键说明，用户按不到也看不见的键
    /// 等于没有这个功能。
    #[test]
    fn shortcut_panel_lists_the_filter_switches() {
        let mut app = App::new();
        app.focus = Focus::Stream;
        let text = render(&mut app, 120, 30);
        assert!(
            text.contains("忽略大小写"),
            "快捷键面板应列出 I（忽略大小写）：\n{text}"
        );
        assert!(
            text.contains("排除匹配"),
            "快捷键面板应列出 !（排除匹配）：\n{text}"
        );
    }

    /// 排除模式下 n/N 跳的是**没命中**的记录，提示行要跟着说。
    #[test]
    fn shortcut_panel_says_what_n_navigates_in_invert_mode() {
        let mut app = App::new();
        app.focus = Focus::Stream;
        let text = render(&mut app, 120, 30);
        assert!(text.contains("跳命中"), "正向模式应说跳命中：\n{text}");

        app.toggle_filter_invert();
        let text = render(&mut app, 120, 30);
        assert!(
            text.contains("跳未命中"),
            "排除模式应说跳未命中（跳的是屏幕上有的那些）：\n{text}"
        );
    }

    /// 一帧里过滤只能扫一遍。
    ///
    /// 断言意图：数据区要一份过滤结果、详情弹窗又要一条记录与一个总数，
    /// 三处各扫一遍就把每帧的开销放大三倍。缓存把它们并成一次。
    #[test]
    fn a_frame_scans_the_filter_only_once() {
        let mut app = demo_app();
        app.apply_filter("Hello".into());
        app.modal = Some(Modal::Detail { idx: 0, hex: false });
        // 收一条新数据：过滤缓存作废，这一帧必须重新扫。
        app.feed_rx(b"more\r\n");
        let before = app.filter_scan_count();
        let _ = render(&mut app, 120, 30);
        let scans = app.filter_scan_count() - before;
        assert_eq!(scans, 1, "一帧里过滤只应扫一遍（数据区与弹窗共用缓存）");
    }

    /// 过滤面板（弹窗）的标题与提示要跟着开关走。
    ///
    /// 反向过滤看到的记录与正向正好相反，弹窗还写着「只显示包含该关键字的记录」
    /// 会直接把人带偏。
    #[test]
    fn filter_modal_reflects_the_active_options() {
        let mut app = App::new();
        app.modal = Some(Modal::Filter(String::new()));
        let text = render(&mut app, 120, 40);
        assert!(text.contains("过滤"), "弹窗标题应写明是过滤：\n{text}");
        assert!(text.contains("只显示包含该关键字的记录"), "正向提示不对：\n{text}");
        assert!(!text.contains("排除包含"), "正向时不该出现排除的提示：\n{text}");

        app.toggle_filter_invert();
        let text = render(&mut app, 120, 40);
        assert!(text.contains("排除包含该关键字的记录"), "反向提示不对：\n{text}");
        assert!(!text.contains("只显示包含"), "反向时不该再显示正向的提示：\n{text}");

        app.toggle_filter_ci();
        let text = render(&mut app, 120, 40);
        assert!(text.contains("忽略大小写"), "忽略大小写应写进提示：\n{text}");
    }

    /// 暂停时标题要写明，否则画面不动会被当成卡死。
    #[test]
    fn paused_state_is_visible_in_the_pane() {
        let mut app = demo_app();
        app.toggle_pause();
        let text = render(&mut app, 120, 30);
        assert!(text.contains("已暂停"), "暂停态应显示在标题：\n{text}");
        app.follow_tail();
        assert!(render(&mut app, 120, 30).contains("自动滚动"));
    }

    /// 设备发彩色日志（带 ANSI 转义）时不能显示成一屏转义垃圾。
    #[test]
    fn ansi_colored_output_renders_readably() {
        let mut app = App::new();
        app.feed_rx("\x1b[31mERROR\x1b[0m: 温度过高\r\n".as_bytes());
        app.feed_rx("INFO 正常运行\r\n".as_bytes());
        let text = render(&mut app, 120, 30);
        assert!(text.contains("^[mERROR^[m: 温度过高"), "转义应折叠成定宽标记：\n{text}");
        assert!(text.contains("INFO 正常运行"));
        assert!(!text.contains("[31m"), "不应残留原始转义参数：\n{text}");
    }

    /// 渲染耗时基准：真实的日志流（心跳行 + 中文 + 少量非法字节），
    /// 缓冲区接近上限时的每帧开销。
    ///
    /// 这是防性能回退的护栏 —— 远程终端上每帧都要过一次网络，
    /// 单帧渲染要是退化到毫秒级，界面就会跟着卡。
    #[test]
    fn render_stays_fast_on_a_full_buffer() {
        let mut app = App::new();
        for i in 0..4_000 {
            app.feed_rx(format!("[heartbeat] {i} x 2000 ms (drop=0, err=0)\r\n").as_bytes());
        }
        // 混一点中文和非法字节，覆盖全部解码分支
        for i in 0..200 {
            app.feed_rx(format!("温度 {i}℃ 传感器 0xFF 正常\r\n").as_bytes());
        }
        assert!(app.records.len() > 4_000, "样本量要够");

        let mut term = Terminal::new(TestBackend::new(200, 50)).expect("TestBackend");
        let start = std::time::Instant::now();
        let frames = 50;
        for _ in 0..frames {            // 每帧都动一下数据，模拟持续收数（否则会被脏标记挡掉）
            app.feed_rx(b"tick\r\n");
            term.draw(|f| draw(f, &mut app)).expect("draw");
        }
        let elapsed = start.elapsed();
        eprintln!("[渲染计时] {frames} 帧共 {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_millis(2500),
            "{frames} 帧耗时 {elapsed:?}，单帧 {} ms，渲染路径疑似退化",
            elapsed.as_millis() * 100 / frames as u128
        );
    }

    /// 把一帧渲染结果写到 target/stui-frame.txt，便于无终端时检查布局。
    #[test]
    fn snapshot_frame_to_file() {
        let mut app = char_by_char_app();
        app.rx_bytes = 128;
        app.tx_bytes = 12;
        app.rx_packets = 13;
        let text = render(&mut app, 110, 32);
        if let Ok(dir) = std::env::current_dir() {
            let dir = dir.join("target");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("stui-frame.txt"), &text);
            // 三种焦点各存一份，方便逐个核对高亮与提示
            for focus in [Focus::Input, Focus::Stream] {
                app.focus = focus;
                let t = render(&mut app, 110, 32);
                let _ = std::fs::write(dir.join(format!("stui-frame-{focus:?}.txt")), t);
            }
            app.focus = Focus::Params;
            // 统计页与「关掉毫秒」的终端页各存一份，用来核对排版
            app.tab = 2;
            let _ = std::fs::write(dir.join("stui-frame-stats.txt"), render(&mut app, 110, 32));
            app.tab = 0;
            app.show_ms = false;
            let _ = std::fs::write(dir.join("stui-frame-noms.txt"), render(&mut app, 110, 32));
            app.show_ms = true;
            // 过滤态：核对高亮片段的排版
            app.apply_filter("VERSION".into());
            let _ = std::fs::write(dir.join("stui-frame-filter.txt"), render(&mut app, 110, 32));
            // 过滤开关都打开：核对标题里的标记与统计页的选项行
            app.toggle_filter_ci();
            app.toggle_filter_invert();
            let _ = std::fs::write(dir.join("stui-frame-filter-opts.txt"), render(&mut app, 110, 32));
            app.tab = 2;
            let _ = std::fs::write(dir.join("stui-frame-stats-opts.txt"), render(&mut app, 110, 32));
            app.tab = 0;
            app.toggle_filter_ci();
            app.toggle_filter_invert();
            // 窄窗口：核对状态栏的分档省略与右侧时钟
            app.toast = None;
            let _ = std::fs::write(dir.join("stui-frame-narrow.txt"), render(&mut app, 64, 20));
            // 帮助页：核对新增按键说明的排版
            app.tab = 3;
            let _ = std::fs::write(dir.join("stui-frame-help.txt"), render(&mut app, 110, 40));
            app.tab = 0;
            // 折行：核对超长行被折开、后续行缩进对齐
            app.wrap = true;
            let mut long = App::new();
            long.wrap = true;
            long.feed_rx(&"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz\r\n".repeat(3).into_bytes());
            let _ = std::fs::write(dir.join("stui-frame-wrap.txt"), render(&mut long, 110, 32));
            // 详情弹窗：核对标题、正文与底部提示的排版
            app.wrap = false;
            app.modal = Some(Modal::Detail { idx: 0, hex: false });
            let _ = std::fs::write(dir.join("stui-frame-detail.txt"), render(&mut app, 110, 32));
            app.modal = Some(Modal::Detail { idx: 0, hex: true });
            let _ = std::fs::write(dir.join("stui-frame-detail-hex.txt"), render(&mut app, 110, 32));
            app.modal = None;
        }
        assert!(text.contains("stui"));
    }

    /// 逐字符回显必须合并成一行：这是线上真实修掉的缺陷。
    #[test]
    fn char_by_char_rx_and_tx_merge_into_one_line_each() {
        let mut app = char_by_char_app();
        let text = render(&mut app, 110, 32);
        let rx_lines = text.lines().filter(|l| l.contains("RX▾")).count();
        let tx_lines = text.lines().filter(|l| l.contains("TX▴")).count();
        assert_eq!(rx_lines, 1, "逐字符到达的接收数据应合并为一行");
        assert_eq!(tx_lines, 1, "一次发送应显示为一行");
        assert!(text.contains("AT+VERSION?"), "应看到合并后的完整命令");
        // 行尾的 \r\n 不应多出一个空行
        assert!(
            !text.lines().any(|l| l.contains("RX▾") && l.trim_end().ends_with("RX▾")),
            "行尾换行不应产生空的 RX 行"
        );
        assert!(
            !text.lines().any(|l| l.contains("TX▴") && l.trim_end().ends_with("TX▴")),
            "行尾换行不应产生空的 TX 行"
        );
    }

    #[test]
    fn renders_every_tab() {
        for tab in 0..4 {
            let mut app = demo_app();
            app.tab = tab;
            let text = render(&mut app, 120, 40);
            assert!(text.contains("stui"), "tab {tab} 缺少标题");
            assert!(text.contains("串口参数"), "tab {tab} 缺少参数面板");
        }
    }

    /// 页签从 3 个变成 4 个：新增「统计」，帮助挪到第 4 个。
    #[test]
    fn tab_bar_lists_four_tabs() {
        let mut app = demo_app();
        let text = render(&mut app, 120, 40);
        for t in ["1 终端", "2 日志", "3 统计", "4 帮助"] {
            assert!(text.contains(t), "页签栏缺少「{t}」：\n{text}");
        }
    }

    /// 统计页必须把关键字段都摆出来，否则这一页只是装饰。
    #[test]
    fn stats_tab_shows_the_key_numbers() {
        let mut app = demo_app();
        app.tab = 2;
        let text = render(&mut app, 140, 44);
        for key in [
            "会话时长",
            "接收字节",
            "发送字节",
            "当前速率",
            "缓冲占用",
            "缓冲记录",
            "串口端口",
            "串口参数",
            "当前视图",
            "过滤关键字",
            "自动重连",
            "界面刷新率",
            "记录文件",
        ] {
            assert!(text.contains(key), "统计页缺少「{key}」：\n{text}");
        }
        // 没开记录文件时要写明「未开启」
        assert!(text.contains("未开启"), "未记录时应写明未开启：\n{text}");
        // 端口信息来自枚举结果
        assert!(text.contains("COM25"), "应显示当前端口：\n{text}");
    }

    /// 过滤命中的片段必须带高亮底色：光有数据看不出命中了什么。
    #[test]
    fn filter_hit_is_highlighted() {
        let mut app = demo_app();
        app.apply_filter("Hello".into());
        let buf = render_buffer(&mut app, 120, 30);
        // 命中的整段文字应连续高亮
        let mut run = 0usize;
        let mut best = 0usize;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].bg == HILITE {
                    run += 1;
                    best = best.max(run);
                } else {
                    run = 0;
                }
            }
        }
        assert!(best >= 5, "「Hello」应整段高亮，最长连续高亮 {best} 格");
        // 高亮的文字本身要在画面上看得见
        let text = render(&mut app, 120, 30);
        assert!(text.contains("Hello"), "命中的记录仍应显示：\n{text}");
    }

    /// 关闭毫秒后行首前缀仍必须是 17 列，否则数据列会左右跳。
    #[test]
    fn prefix_width_is_stable_with_and_without_milliseconds() {
        let app = demo_app();
        let rec = app.stream().next().expect("应有记录");
        for show_ms in [true, false] {
            let w: usize = prefix_spans(rec, show_ms)
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert_eq!(w, PREFIX, "行首前缀必须恒为 {PREFIX} 列（show_ms={show_ms}）");
        }
    }

    /// 真正画出来时，数据列的位置也不能因为开关毫秒而改变。
    #[test]
    fn data_column_does_not_move_when_milliseconds_are_off() {
        let mut app = demo_app();
        let with = render(&mut app, 120, 30);
        app.show_ms = false;
        let without = render(&mut app, 120, 30);
        let col = |t: &str| t.lines().find_map(|l| l.find("Hello")).expect("应看到数据");
        assert_eq!(col(&with), col(&without), "关掉毫秒后数据列不应错位");
    }

    /// 惰性分行是界面层自己写的，必须与 `fmt::display_lines` 逐行一致：
    /// 一旦对不上，显示高度就会算错（滚不到底或多出空行）。
    #[test]
    fn lazy_line_split_matches_display_lines() {
        let cases: Vec<&[u8]> = vec![
            b"",
            b"abc",
            b"abc\n",
            b"a\r\nb",
            b"a\r\nb\r\n",
            b"\n\n\n",
            b"\n",
            b"\r\n",
            b"50%\r75%\r100%",
            b"line1\r\nline2\r\n",
            "温度\r\n数据\n".as_bytes(),
        ];
        for c in cases {
            let mine: Vec<&[u8]> = RecordLines::new(c).collect();
            assert_eq!(mine, fmt::display_lines(c), "分行不一致：{c:?}");
        }
    }

    /// HEX 分块的条数必须与缓存的行数一致，否则总行数对不上。
    #[test]
    fn hex_chunk_count_matches_cached_line_count() {
        for len in [0usize, 1, 5, 15, 16, 17, 100] {
            let data = vec![b'x'; len];
            let n = hex_chunks(&data, 16).count();
            assert_eq!(
                n,
                data.len().div_ceil(16).max(1),
                "长度 {len} 的分块数与缓存行数不一致"
            );
        }
    }

    #[test]
    fn text_view_wraps_only_on_newlines() {
        let mut app = demo_app();
        let text = render(&mut app, 120, 40);
        // \r\n 是真实换行，所以 Hello 与 World 各占一行，而不是显示成 ^M
        let hello_row = text.lines().find(|l| l.contains("Hello")).expect("Hello 行");
        let world_row = text.lines().find(|l| l.contains("World")).expect("World 行");
        assert!(!hello_row.contains("World"), "换行后不应再出现 World");
        assert!(!hello_row.contains('^'), "真实换行不该显示成 ^M");
        // 行内控制符仍用 ^X 形式
        assert!(world_row.contains("World^@"), "NUL 应显示为 ^@");
    }

    #[test]
    fn text_view_clips_long_line_without_wrapping() {
        let mut app = App::new();
        app.feed_rx(&"X".repeat(500).into_bytes()); // 无换行的超长行
        let text = render(&mut app, 100, 20);
        let row = text.lines().find(|l| l.contains('…')).expect("截断提示");
        assert!(row.contains("XXXX"), "应显示数据前缀");
        // 500 个 X 没有换行，必须只占一行（HEX 标签里的 X 不算）
        let data_rows = text.lines().filter(|l| l.contains("XXXX")).count();
        assert_eq!(data_rows, 1, "无换行的超长数据不应被折成多行");
        assert!(!text.lines().any(|l| l.contains("X…X")), "不应在行内插入换行");
    }

    #[test]
    fn hex_view_shows_offset_and_ascii() {
        let mut app = demo_app();
        app.view = View::Hex;
        let text = render(&mut app, 120, 40);
        assert!(text.contains("0000"));
        assert!(text.contains("|Hello"));
        assert!(text.contains("48 65 6C 6C 6F"));
    }

    /// HEX 视图的 ASCII 栏必须完整：右竖线要在。
    ///
    /// 排版判定早先少算了偏移列与 ASCII 段的宽度，选出来的排版放不下，
    /// ASCII 栏会被面板从中间裁掉（连右竖线都没了）。
    #[test]
    fn hex_ascii_column_is_not_cut_in_half() {
        let mut app = App::new();
        app.view = View::Hex;
        app.feed_rx(b"ABCDEFGH");
        for w in [120u16, 110, 100, 90, 80, 70] {
            let text = render(&mut app, w, 30);
            for row in text.lines().filter(|l| l.contains("41 42")) {
                let pipes = row.matches('|').count();
                if pipes == 0 {
                    continue; // 这一档没有 ASCII 栏
                }
                assert!(pipes >= 2, "{w} 列下 ASCII 栏的右竖线被裁掉了：{row:?}");
                let start = row.find('|').expect("有竖线");
                let end = row.rfind('|').expect("有竖线");
                let ascii = &row[start + 1..end];
                assert!(
                    !ascii.is_empty() && "ABCDEFGH".starts_with(ascii),
                    "{w} 列下 ASCII 栏内容不对：{ascii:?}"
                );
            }
        }
    }

    /// 窄面板（70 列）下第四个字节不能被切掉。
    ///
    /// 早先第二档（保留偏移、去掉 ASCII）漏算了偏移列的 5 列，
    /// 70 列时这一档只画得出 3 个字节。
    #[test]
    fn narrow_hex_view_keeps_the_last_byte() {
        let mut app = App::new();
        app.view = View::Hex;
        app.feed_rx(b"ABCD");
        let text = render(&mut app, 70, 30);
        let row = text.lines().find(|l| l.contains("41 42")).expect("应当有十六进制行");
        assert!(row.contains("41 42 43 44"), "第 4 个字节不该被切掉：{row:?}");
    }

    /// 中文输入时光标要落在正确的**列**上：一个汉字占两列。
    #[test]
    fn cursor_column_counts_display_width() {
        let mut app = App::new();
        app.focus = Focus::Input;
        app.input = "温度测试".into();
        app.cursor = app.input.chars().count();
        let mut term = Terminal::new(TestBackend::new(120, 30)).expect("TestBackend");
        term.draw(|f| draw(f, &mut app)).expect("draw");
        let pos = term.get_cursor_position().expect("应当有光标位置");
        // 左栏 38 列 + 输入框左边框 1 列 = 从第 39 列开始；
        // 「温度测试」4 个字 = 8 个显示列。
        assert_eq!(pos.x, 38 + 1 + 8, "中文输入的光标列算错了");
    }

    /// 左栏各行的值必须对齐成一列（第 0 行的标签不能是空串）。
    #[test]
    fn params_pane_values_are_aligned() {
        let mut app = demo_app();
        app.focus = Focus::Params;
        let text = render(&mut app, 120, 30);
        let col_of = |line: &str, needle: &str| -> usize {
            let idx = line.find(needle).expect("这一行里应当有该内容");
            UnicodeWidthStr::width(&line[..idx])
        };
        let status_line = text.lines().find(|l| l.contains("状态")).expect("应当有状态行");
        let port_line = text.lines().find(|l| l.contains("COM25")).expect("应当有端口行");
        let status_col = col_of(status_line, "○");
        let port_col = col_of(port_line, "COM25");
        assert_eq!(status_col, port_col, "第 0 行与其它行的值列没有对齐");
    }

    /// 左栏的值按**显示列宽**截断：中文值一个字两列，
    /// 而且选中的 `◀▶` 提示不能被长值挤掉。
    #[test]
    fn params_value_truncates_by_columns_and_keeps_the_hint() {
        let mut app = App::new();
        app.ports.push(PortEntry {
            port: "COM25".into(),
            desc: "中文设备描述很长很长很长".into(),
        });
        app.focus = Focus::Params;
        app.sel = 1; // 端口行
        let text = render(&mut app, 120, 30);
        let row = text.lines().find(|l| l.contains("COM25")).expect("应当有端口行");
        assert!(row.contains("◀▶"), "选中的提示不该被长值挤掉：{row:?}");
        // 左栏内宽 36 列（面板 38 列减去两条边框）
        let inner = row.split('│').nth(1).expect("应当有左栏内容");
        assert!(
            UnicodeWidthStr::width(inner) <= 36,
            "左栏这一行超出了内宽：{inner:?}"
        );
    }

    #[test]
    fn empty_buffer_renders_placeholder() {
        let mut app = App::new();
        let text = render(&mut app, 100, 30);
        assert!(text.contains("暂无数据"));
    }

    #[test]
    fn narrow_window_shows_hint() {
        let mut app = demo_app();
        let text = render(&mut app, 40, 10);
        assert!(text.contains("窗口太小"));
    }

    /// 最小窗口（64x14）也要画得出来：快捷键面板多占一行之后，
    /// 参数栏被挤小，但布局不能崩。
    #[test]
    fn smallest_window_still_renders() {
        let mut app = demo_app();
        app.focus = Focus::Stream;
        let text = render(&mut app, 64, 14);
        assert!(!text.contains("窗口太小"), "64x14 是支持的最小尺寸：\n{text}");
    }

    #[test]
    fn modal_renders_path_prompt() {
        let mut app = demo_app();
        app.modal = Some(Modal::FilePath("firmware.bin".into()));
        let text = render(&mut app, 120, 40);
        assert!(text.contains("发送文件"));
        assert!(text.contains("firmware.bin"));
    }

    #[test]
    fn long_record_wraps_without_overflow() {
        let mut app = App::new();
        // 单条超长记录，必须按列宽换行且不越界
        app.feed_rx(&vec![b'x'; 4096]);
        for w in [64u16, 80, 100, 200] {
            let text = render(&mut app, w, 30);
            assert_eq!(text.lines().count(), 30);
            assert!(text.lines().all(|l| l.chars().count() <= w as usize));
        }
    }

    #[test]
    fn scrolling_past_the_end_is_safe() {
        let mut app = demo_app();
        for _ in 0..50 {
            app.scroll_top += 10;
        }
        app.auto_scroll = false;
        let text = render(&mut app, 100, 30);
        assert!(text.contains("stui"));
    }

    /// 各种窗口尺寸下渲染都不能 panic，也不能越界写单元格。
    ///
    /// 布局里有多处减法（内容宽度、前缀宽度、可视高度），宽度/高度贴近
    /// 下限时最容易出现下溢或错位，这里把边界逐个压一遍。
    #[test]
    fn survives_boundary_sizes() {
        for (w, h) in [
            (64u16, 14u16),
            (64, 15),
            (65, 16),
            (70, 14),
            (80, 20),
            (100, 17),
            (120, 14),
            (200, 60),
        ] {
            for tab in 0..4 {
                for focus in [Focus::Params, Focus::Input, Focus::Stream] {
                    let mut app = demo_app();
                    app.tab = tab;
                    app.focus = focus;
                    app.input = "AT+VERSION?\\r\\n".into();
                    app.cursor = app.input.chars().count();
                    let text = render(&mut app, w, h);
                    assert_eq!(
                        text.lines().count(),
                        h as usize,
                        "{w}x{h} tab={tab} 行数不对"
                    );
                    assert!(
                        text.lines().all(|l| l.chars().count() <= w as usize),
                        "{w}x{h} tab={tab} focus={focus:?} 有行超出宽度"
                    );
                }
            }
        }
    }

    /// 状态区必须真的有内容（曾因只分到 2 行、上下边框占满而完全不可见）。
    #[test]
    fn status_line_is_visible() {
        for (w, h) in [(100u16, 24u16), (80, 20), (64, 16)] {
            let mut app = demo_app();
            app.input = "AT+温度\\r\\n".into();
            let text = render(&mut app, w, h);
            assert!(
                text.contains("待发送"),
                "{w}x{h} 状态区没有渲染出「待发送」"
            );
            assert!(text.contains("循环"), "{w}x{h} 状态区缺少循环开关状态");
        }
    }

    /// 串口打不开时，状态栏要给出**原因**。
    ///
    /// 只写「未连接」的话，用户只能自己猜是设备没插、参数不对还是端口被占。
    /// 原因取自事件日志里 App 记下的那条失败消息。
    #[test]
    fn status_bar_shows_the_reason_of_a_failed_open() {
        let mut app = App::new();
        // App 打开失败时记的就是这条 SYS 日志
        app.push_sys("打开 COM25 失败：设备不存在或已被占用");
        let text = render(&mut app, 140, 30);
        let status = text.lines().last().expect("状态栏");
        assert!(
            status.contains("失败"),
            "状态栏应给出打开失败的原因：\n{status}"
        );
        assert!(status.contains("未连接"), "状态栏仍应说明未连接：\n{status}");
    }

    /// 掉线（拔线 / 设备复位）的原因同样要显示。
    #[test]
    fn status_bar_shows_the_reason_of_a_disconnect() {
        let mut app = App::new();
        app.push_sys("已打开 COM25 · 115200");
        app.push_sys("读取出错：设备已拔出");
        let text = render(&mut app, 140, 30);
        let status = text.lines().last().expect("状态栏");
        assert!(
            status.contains("出错"),
            "状态栏应给出掉线的原因：\n{status}"
        );
    }

    /// 没连过串口就不要凭空显示原因。
    #[test]
    fn status_bar_has_no_reason_before_any_connection_attempt() {
        let mut app = App::new();
        let text = render(&mut app, 140, 30);
        let status = text.lines().last().expect("状态栏");
        assert!(status.contains("未连接"), "状态栏应说明未连接：\n{status}");
        assert!(
            !status.contains("失败"),
            "没试过打开就不该显示失败原因：\n{status}"
        );
    }

    /// 超长数据 + 滚动到极限，也不应 panic 或产生越界行。
    #[test]
    fn scroll_extremes_with_huge_stream() {
        let mut app = App::new();
        for i in 0..5_000 {
            app.feed_rx(format!("line-{i:06} 数据\\r\\n").as_bytes());
        }
        app.view_h = 12;
        for delta in [-100000isize, -1, 0, 1, 100000] {
            app.scroll_by(delta);
            let text = render(&mut app, 90, 20);
            assert!(text.lines().all(|l| l.chars().count() <= 90));
        }
    }

    /// 非法 UTF-8、半个多字节序列、超长无换行数据都不能让渲染崩溃。
    #[test]
    fn hostile_bytes_do_not_panic() {
        let mut app = App::new();
        app.feed_rx(&[0xE4, 0xB8, 0xAD, 0xFF, 0xFE, 0x00, 0x80]);
        app.feed_rx(&vec![0xFFu8; 10_000]);
        app.feed_rx("😀 混合 emoji 与中文".as_bytes());
        app.commit_tx(&[0x1B, b'[', b'3', b'1', b'm']);
        let text = render(&mut app, 72, 18);
        assert!(text.contains("stui"));
    }

    // ---------- 长行折行 ----------

    /// 数据区的内容宽度：屏宽 − 左栏 38 − 面板边框 2 − 行首前缀 17。
    const CONTENT_W_100: usize = 100 - 38 - 2 - 17;

    /// 折行开启：一条超长的逻辑行要折成多行显示，而且不再出现 `…`。
    #[test]
    fn wrap_splits_a_long_line_into_several_rows() {
        let mut app = App::new();
        app.wrap = true;
        app.feed_rx(&vec![b'x'; 500]);
        let text = render(&mut app, 100, 30);
        // 只数数据行：左栏的快捷键说明里也有「x」，不能算进来。
        let rows = text.lines().filter(|l| l.contains("xxxx")).count();
        assert!(rows > 1, "折行后一条长记录应显示成多行，实际 {rows} 行：\n{text}");
        assert!(!text.contains('…'), "折行时不该再截断：\n{text}");
    }

    /// 折行关闭：行为与从前完全一致 —— 一行截断并标 `…`。
    #[test]
    fn wrap_off_keeps_truncating_the_long_line() {
        let mut app = App::new();
        app.wrap = false;
        app.feed_rx(&vec![b'x'; 500]);
        let text = render(&mut app, 100, 30);
        assert!(text.contains('…'), "不折行时应当照旧截断：\n{text}");
        let rows = text.lines().filter(|l| l.contains("xxxx")).count();
        assert_eq!(rows, 1, "不折行时一条长记录只能占一行，实际 {rows} 行");
    }

    /// 折行时「统计几行」与「画几行」必须是同一个算法：
    /// 统计少了滚不到底，统计多了底部会多出空行。
    #[test]
    fn wrap_row_count_matches_the_rendered_rows() {
        let mut app = App::new();
        app.wrap = true;
        let mut ascii = b"0123456789".repeat(12);
        ascii.extend_from_slice(b"\r\n");
        app.feed_rx(&ascii); // 一条 120 列的 ASCII 长行
        let mut cjk = "温度".repeat(30).into_bytes();
        cjk.extend_from_slice(b"\r\n");
        app.feed_rx(&cjk); // 一条全是宽字符的长行
        let recs: Vec<_> = app.stream().collect();
        assert_eq!(recs.len(), 2, "两段数据应当各成一条记录");
        for rec in recs {
            for w in [20usize, 43, 60] {
                let layout = StreamLayout { view: View::Text, per_line: 16, content_w: w, wrap: true };
                let counted = record_line_count(rec, &layout);
                let rendered: usize = rec
                    .text_view()
                    .lines
                    .iter()
                    .map(|l| fmt::wrap_display(l, w).len())
                    .sum();
                assert_eq!(counted, rendered, "宽度 {w} 下统计的行数与实际折出的段数不一致");
            }
        }
    }

    /// 「列宽 ÷ 宽度向上取整」只是**下界**：宽字符整体挪到下一行时会空出几列，
    /// 实际行数会更多。统计必须用实际折出来的段数，否则滚不到底。
    #[test]
    fn the_ceiling_formula_is_only_a_lower_bound() {
        // 39 个汉字（78 列）+ 一个字母 = 79 列，再来一个汉字就放不下了：
        // 折行时空出的那一列会让行数比 ceil(列宽 / 宽度) 多一行。
        let text = format!("{}a{}a温", "温".repeat(39), "温".repeat(39));
        let cols = fmt::display_cols(&text);
        let width = 80usize;
        let naive = cols.div_ceil(width);
        let actual = fmt::wrap_display(&text, width).len();
        assert!(actual > naive, "向上取整只能当下界：naive={naive} actual={actual}");
        assert_eq!(
            fmt::wrap_rows(&text, cols as u32, width),
            actual,
            "统计的行数必须与实际折出来的段数一致"
        );
    }

    /// 折行时一条记录占几行，界面上就画几行（不多不少）。
    #[test]
    fn wrap_record_occupies_exactly_the_counted_rows() {
        let mut app = App::new();
        app.wrap = true;
        app.feed_rx(&vec![b'x'; 500]);
        let rec = app.stream().next().expect("应有记录");
        let layout = StreamLayout {
            view: View::Text,
            per_line: 16,
            content_w: CONTENT_W_100,
            wrap: true,
        };
        let counted = record_line_count(rec, &layout);
        let text = render(&mut app, 100, 30);
        // 只数数据行：左栏的快捷键说明里也有「x」，不能算进来。
        let drawn = text.lines().filter(|l| l.contains("xxxx")).count();
        assert_eq!(drawn, counted, "画了 {drawn} 行，统计的是 {counted} 行");
    }

    /// 折行时滚到底要真的看到最后一条记录（行数统计与渲染一致）。
    #[test]
    fn wrap_scrolling_reaches_the_last_record() {
        let mut app = App::new();
        app.wrap = true;
        for i in 0..10 {
            app.feed_rx(format!("{}{:04}\r\n", "x".repeat(200), i).as_bytes());
        }
        app.follow_tail();
        let text = render(&mut app, 100, 30);
        assert!(text.contains("0009"), "滚到底应当看到最后一条记录：\n{text}");
    }

    /// 折行模式下滚到底：**每一条**可见记录的头部（时间戳与 RX 标记）都要完整。
    ///
    /// 曾经有个缺陷：渲染层画完一条记录后没有推进行偏移，于是视口里第一条
    /// （半露的）记录之后的所有记录都拿错了行偏移 —— 每条记录的第一行被切掉，
    /// 时间戳与 RX 标记消失，行数少的记录甚至整条看不见，滚到底时最后一条
    /// 根本显示不出来。折行把这个缺陷放大成「整屏错位」。
    #[test]
    fn wrap_bottom_keeps_every_record_head_visible() {
        let mut app = App::new();
        app.wrap = true;
        for i in 0..10 {
            app.feed_rx(format!("{}{:04}\r\n", "x".repeat(200), i).as_bytes());
        }
        app.follow_tail();
        let text = render(&mut app, 100, 30);
        // 折行后每条记录只有第一行带 RX 前缀（续行是缩进的空格），
        // 所以「RX▾」的个数就等于视口里完整显示了头部的记录条数。
        let heads = text.lines().filter(|l| l.contains("RX▾")).count();
        assert!(heads >= 3, "视口里应至少完整看到三条记录的头部，实际 {heads}：\n{text}");
        // 最后一条记录的内容也得在
        assert!(text.contains("0009"), "滚到底应当看到最后一条记录：\n{text}");
    }

    // ---------- 记录详情弹窗 ----------

    /// 详情弹窗要能显示记录的**完整**内容：内容比弹窗高时，滚动到底能看到尾部。
    #[test]
    fn detail_modal_shows_the_tail_of_a_long_record() {
        let mut app = App::new();
        app.feed_rx(b"HEAD\r\n");
        let mut data = vec![b'A'; 4000];
        data.extend_from_slice(b"TAILMARK");
        data.push(b'\n');
        app.feed_rx(&data);
        app.modal = Some(Modal::Detail { idx: 1, hex: false });

        let head = render(&mut app, 120, 40);
        assert!(head.contains("记录详情"), "弹窗标题应写明这是记录详情：\n{head}");
        assert!(head.contains("RX"), "标题应写明方向：\n{head}");
        assert!(!head.contains("TAILMARK"), "内容装不下时尾部不该出现：\n{head}");

        for _ in 0..200 {
            detail_scroll(1);
        }
        let tail = render(&mut app, 120, 40);
        assert!(tail.contains("TAILMARK"), "滚动到底应当看到尾部内容：\n{tail}");
        assert!(tail.contains("Esc/Enter"), "弹窗底部应有按键提示：\n{tail}");
    }

    /// 翻页：一次翻一页（页高取自上次渲染时正文实际能显示的行数）。
    #[test]
    fn detail_modal_page_scroll_moves_by_a_page() {
        let mut app = App::new();
        let mut data = Vec::new();
        for i in 0..200 {
            data.extend_from_slice(format!("line-{i:03}\r\n").as_bytes());
        }
        app.feed_rx(&data);
        app.modal = Some(Modal::Detail { idx: 0, hex: false });

        let first = render(&mut app, 120, 40);
        assert!(first.contains("line-000"), "第一页应当从第一行开始");
        assert!(!first.contains("line-100"), "第一页不该看到第 100 行");

        detail_scroll_page(1);
        let second = render(&mut app, 120, 40);
        // 弹窗 120x40 屏 → 弹窗高 32 → 内框 30 → 正文 29 行。
        assert!(second.contains("line-029"), "翻页后应当看到第二页的第一行：\n{second}");
    }

    /// HEX 模式的详情弹窗显示的是原始字节（偏移 + 十六进制 + ASCII）。
    #[test]
    fn detail_modal_hex_shows_the_raw_bytes() {
        let mut app = App::new();
        app.feed_rx(b"ABCDEF");
        app.modal = Some(Modal::Detail { idx: 0, hex: true });
        let text = render(&mut app, 120, 40);
        assert!(text.contains("41 42 43 44 45 46"), "HEX 视图应显示原始字节：\n{text}");
        assert!(text.contains("0000"), "HEX 视图应有偏移：\n{text}");
    }

    /// HEX 弹窗的排版不能超出弹窗宽度：ASCII 栏算宽错了就会被裁掉右竖线。
    #[test]
    fn detail_modal_hex_fits_the_popup_width() {
        let mut app = App::new();
        app.feed_rx(&[b'A'; 100]);
        app.modal = Some(Modal::Detail { idx: 0, hex: true });
        let text = render(&mut app, 120, 40);
        for row in text.lines().filter(|l| l.contains("41 42")) {
            assert!(row.matches('|').count() >= 2, "ASCII 栏的右竖线被裁掉了：{row:?}");
        }
        assert!(
            text.lines().all(|l| l.chars().count() <= 120),
            "弹窗内容超出了屏宽"
        );
    }

    // ---------- 过滤命中导航 ----------

    /// `n` / `N` 把视图挪到下一条 / 上一条命中的记录上。
    #[test]
    fn nav_jumps_to_the_next_matching_record() {
        let mut app = App::new();
        for i in 0..100 {
            app.feed_rx(format!("HIT-{i:03}\r\n").as_bytes());
        }
        app.apply_filter("HIT".into());

        app.nav_dir = Some(1);
        render(&mut app, 120, 30);
        assert_eq!(app.nav_dir, None, "导航请求应被消费掉");
        assert_eq!(app.top_record, 1, "应跳到下一条命中的记录");
        assert_eq!(app.scroll_top, 1, "滚动位置应落在那条记录的行偏移上");
        let text = render(&mut app, 120, 30);
        let first = text.lines().find(|l| l.contains("HIT-")).expect("应有命中记录");
        assert!(first.contains("HIT-001"), "顶上应是跳过去的那条：{first:?}");

        // 再来一次：继续往下。
        app.nav_dir = Some(1);
        render(&mut app, 120, 30);
        assert_eq!(app.top_record, 2, "应继续往下跳");

        // 反向：回到上一条。
        app.nav_dir = Some(-1);
        render(&mut app, 120, 30);
        assert_eq!(app.top_record, 1, "应能往回跳");
    }

    /// 导航到没有下一条时：只清掉请求，不乱滚。
    #[test]
    fn nav_at_the_end_keeps_the_position() {
        let mut app = App::new();
        for i in 0..5 {
            app.feed_rx(format!("HIT-{i}\r\n").as_bytes());
        }
        app.apply_filter("HIT".into());
        app.top_record = 4;
        app.nav_dir = Some(1);
        let before = app.scroll_top;
        render(&mut app, 120, 30);
        assert_eq!(app.nav_dir, None, "导航请求应被消费掉");
        assert_eq!(app.scroll_top, before, "没有下一条时不该乱滚");
    }

    /// 非终端页（日志 / 统计 / 帮助）按 `n` / `N`：
    /// 不登记导航请求，也不留一个「等切回终端页才生效」的悬着请求。
    #[test]
    fn nav_on_a_non_terminal_tab_does_not_leave_a_pending_request() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new();
        app.feed_rx(b"HIT-0\r\n");
        app.apply_filter("HIT".into());
        // 切到日志页
        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.tab, 1, "应已切到日志页");

        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.nav_dir, None, "非终端页不该留下悬着的导航请求");
        let text = render(&mut app, 120, 30);
        assert!(
            text.contains("终端页"),
            "应当告诉用户要先切到终端页：\n{text}"
        );
        // 切回终端页也不该突然跳一条命中（请求已经作废）。
        app.on_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.nav_dir, None, "换页签时导航请求应复位");
        render(&mut app, 120, 30);
        assert_eq!(app.top_record, 0, "不该因为之前那次按键跳走");
    }

    /// 滚到底时，最后一条记录要**从头**出现（不是只剩尾巴）。
    ///
    /// 定位记录用的行偏移必须逐条推进：画完一条不推进的话，
    /// 后面的记录会拿同一条记录的偏移定位，每条记录的开头被切掉、
    /// 行数少的记录甚至整条看不见（滚到底也看不到最后一条）。
    #[test]
    fn scrolling_to_the_bottom_shows_the_last_record_from_its_first_row() {
        let mut app = App::new();
        for i in 0..10 {
            app.feed_rx(format!("R{i:02}-0\nR{i:02}-1\nR{i:02}-2\n").as_bytes());
        }
        app.follow_tail();
        let text = render(&mut app, 100, 30);
        assert!(
            text.contains("R09-0"),
            "滚到底应当看到最后一条记录的开头：\n{text}"
        );
        // 中间的记录也不能少开头（每条记录都是三行，可见范围内的都要从头画起）。
        assert!(text.contains("R05-0"), "中间记录的开头也不该被切掉：\n{text}");
    }

    /// HEX 视图里同理：可视范围内新开始的记录，偏移必须从 `0000` 起。
    ///
    /// 定位用的行偏移必须逐条推进：不推进的话，后面的记录会拿同一条记录的
    /// 偏移定位，每条记录都从中间画起（偏移变成 0008 / 0010 起步），
    /// 屏幕上再也看不到「0000 开头的新记录」。
    #[test]
    fn hex_view_starts_new_records_at_offset_zero() {
        let mut app = App::new();
        app.view = View::Hex;
        // 每条记录 3 块（每块 8 字节），末尾补一个换行让 feed_rx 封成一条记录。
        for i in 0..10u8 {
            let mut data = Vec::new();
            for j in 0..3u8 {
                data.extend(std::iter::repeat(0xA0 + i * 3 + j).take(8));
            }
            data[23] = b'\n';
            app.feed_rx(&data);
        }
        app.follow_tail();
        let text = render(&mut app, 100, 30);
        // 每条从自己第一块画起的记录都会贡献一行「0000 …」。
        let starts = text.lines().filter(|l| l.contains("0000 ")).count();
        assert!(
            starts >= 2,
            "可视范围内新开始的记录必须从偏移 0000 画起，只找到 {starts} 行：\n{text}"
        );
    }

    /// 关键字正好被折行边界切开时，也要高亮得出来。
    ///
    /// 命中区间必须按**整条逻辑行**算：逐段各算各的，
    /// 关键字被切开时就会「算命中却一个格子都没高亮」。
    #[test]
    fn filter_hit_is_highlighted_across_a_wrap_boundary() {
        let mut app = App::new();
        app.wrap = true;
        let mut data = vec![b'A'; 40];
        data.extend_from_slice(b"NEEDLE");
        data.push(b'B');
        app.feed_rx(&data);
        app.apply_filter("NEEDLE".into());
        assert_eq!(app.stream_filtered().count(), 1, "应当命中这条记录");

        let buf = render_buffer(&mut app, 100, 30);
        let mut cells = 0usize;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].bg == HILITE {
                    cells += 1;
                }
            }
        }
        assert!(cells >= 6, "关键字被折行切开也要高亮，只有 {cells} 格");
    }

    /// 折行时「这条记录占几行」与 `fmt::wrap_row_count` 必须是同一个口径：
    /// 两边算法分叉，统计与渲染就会对不上。
    #[test]
    fn fmt_wrap_row_count_matches_the_rendered_rows() {
        let mut app = App::new();
        app.wrap = true;
        let mut ascii = b"0123456789".repeat(12);
        ascii.extend_from_slice(b"\r\n");
        app.feed_rx(&ascii);
        let mut cjk = "温度".repeat(30).into_bytes();
        cjk.extend_from_slice(b"\r\n");
        app.feed_rx(&cjk);
        // 多行数据：一条记录里有两条逻辑行
        app.feed_rx(b"first\nsecond\n");

        let recs: Vec<_> = app.stream().collect();
        assert_eq!(recs.len(), 3, "三段数据应当各成一条记录");
        for rec in recs {
            for w in [20usize, 43, 60] {
                let layout = StreamLayout { view: View::Text, per_line: 16, content_w: w, wrap: true };
                assert_eq!(
                    record_line_count(rec, &layout),
                    fmt::wrap_row_count(&rec.data, w),
                    "宽度 {w} 下 fmt 与界面的行数统计不一致"
                );
            }
        }
    }

    /// `Record::visual_rows`（界面的行数统计用的就是它）与
    /// `fmt::wrap_display` 折出来的**段数**必须一致。
    ///
    /// 两者分家了就是「统计几行」与「画几行」不一致：滚动会错位，
    /// 滚不到底或者底部多出空行。这里把不一致直接报出来。
    #[test]
    fn visual_rows_agrees_with_the_wrapped_segment_count() {
        let mut app = App::new();
        app.wrap = true;
        // 覆盖各种边界：宽字符、控制符、超长行、多逻辑行、ANSI 转义
        let mut samples: Vec<Vec<u8>> = vec![
            b"plain ascii line\r\n".to_vec(),
            "温度传感器 0xFF 正常\r\n".as_bytes().to_vec(),
            vec![b'x'; 500],
            b"first\nsecond\r\n".to_vec(),
            "\x1b[31mERROR\x1b[0m: 温度过高\r\n".as_bytes().to_vec(),
            "混合中文与 ascii 的长行".repeat(20).into_bytes(),
        ];
        for s in samples.drain(..) {
            app.feed_rx(&s);
        }
        let recs: Vec<_> = app.stream().collect();
        assert!(!recs.is_empty(), "应当有样本");
        for rec in recs {
            for w in [1usize, 2, 5, 20, 43, 60, 200] {
                let counted = rec.visual_rows(w);
                let rendered: usize = rec
                    .text_view()
                    .lines
                    .iter()
                    .map(|l| fmt::wrap_display(l, w).len())
                    .sum();
                assert_eq!(
                    counted,
                    rendered,
                    "宽度 {w} 下 `Record::visual_rows` 与 `fmt::wrap_display` 的口径分家了：{counted} 行 vs {rendered} 段"
                );
            }
        }
    }

    /// 折行时的行数统计必须走**缓存**，不能每帧重算。
    ///
    /// 这条是本轮性能目标的核心：折行数行数若每帧重扫一遍内容，
    /// 开销就随缓冲里的内容线性增长 —— 缓冲越大，界面越卡。
    /// 缓存命中后每条记录只剩一次查表，开销与内容多少无关。
    ///
    /// 样本刻意用**长行**：短行在宽度内放得下，`fmt::wrap_rows` 会直接返回 1，
    /// 根本走不到扫描 —— 拿短行测，测的是早退分支，测不出真实开销。
    #[test]
    fn wrap_row_counting_over_a_full_buffer_stays_fast() {
        let mut app = App::new();
        app.wrap = true;
        // 每条 400 列，在 143 列的宽度下要折成 3 行 —— 真的要走扫描
        let long = "0123456789".repeat(40);
        for _ in 0..2_000 {
            app.feed_rx(format!("{long}\r\n").as_bytes());
        }
        let layout = StreamLayout { view: View::Text, per_line: 16, content_w: 143, wrap: true };
        let recs: Vec<_> = app.stream().collect();
        assert_eq!(recs.len(), 2_000, "样本量要够");

        // 第一遍：填缓存（顺带把显示缓存也建好）
        let start = std::time::Instant::now();
        let mut total = 0usize;
        for rec in &recs {
            total += record_line_count(rec, &layout);
        }
        let first = start.elapsed();
        assert!(total > 2_000, "每条长行应折成多行：{total}");

        // 第二遍：宽度不变，全部命中缓存 —— 这才是每帧的真实开销
        let start = std::time::Instant::now();
        let mut cached_total = 0usize;
        for _ in 0..50 {
            for rec in &recs {
                cached_total += record_line_count(rec, &layout);
            }
        }
        let cached = start.elapsed();
        assert_eq!(cached_total, total * 50, "缓存命中时结果必须一致");
        eprintln!("[折行行数统计计时] 首次 {first:?}，命中缓存 50 遍 {cached:?}");
        assert!(
            cached < std::time::Duration::from_millis(2000),
            "50 遍折行行数统计耗时 {cached:?}，每遍 {} ms —— 缓存可能没生效",
            cached.as_millis() / 50
        );
        // 缓存命中后应远快于首次计算：首次要解码 + 折行，命中只要查表。
        assert!(
            cached * 4 < first,
            "命中缓存 50 遍（{cached:?}）仍不够快，首次计算 {first:?}"
        );
    }

    // ---------- 性能护栏 ----------

    /// 折行模式下的每帧耗时也要有预算：
    /// 折行要数每条记录占几行，绝不能为此把每条记录重新解码一遍。
    ///
    /// 样本用**长行**：短行在宽度内放得下，压根不会触发折行，
    /// 测不出「折行让每帧成本随内容线性增长」这件事。
    #[test]
    fn render_with_wrap_stays_fast_on_a_full_buffer() {
        let mut app = App::new();
        app.wrap = true;
        // 每条 400 列，在 143 列的宽度下要折成 3 行
        let long = "0123456789".repeat(40);
        for _ in 0..2_000 {
            app.feed_rx(format!("{long}\r\n").as_bytes());
        }
        let mut term = Terminal::new(TestBackend::new(200, 50)).expect("TestBackend");
        let start = std::time::Instant::now();
        let frames = 50;
        for _ in 0..frames {
            app.feed_rx(b"tick\r\n");
            term.draw(|f| draw(f, &mut app)).expect("draw");
        }
        let elapsed = start.elapsed();
        eprintln!("[折行渲染计时] {frames} 帧共 {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_millis(1200),
            "{frames} 帧耗时 {elapsed:?}，单帧 {} ms，折行路径疑似退化",
            elapsed.as_millis() * 100 / frames as u128
        );
    }

    /// 开启过滤 + 满缓冲时的每帧耗时也要有预算：
    /// 过滤扫描是每帧都要做的，慢了界面就跟着卡。
    #[test]
    fn render_with_filter_stays_fast_on_a_full_buffer() {
        let mut app = App::new();
        for i in 0..4_000 {
            app.feed_rx(format!("[heartbeat] {i} x 2000 ms (drop=0, err=0)\r\n").as_bytes());
        }
        app.apply_filter("heartbeat".into());

        let mut term = Terminal::new(TestBackend::new(200, 50)).expect("TestBackend");
        let start = std::time::Instant::now();
        let frames = 50;
        for _ in 0..frames {
            app.feed_rx(b"tick\r\n");
            term.draw(|f| draw(f, &mut app)).expect("draw");
        }
        let elapsed = start.elapsed();
        eprintln!("[过滤渲染计时] {frames} 帧共 {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_millis(4000),
            "{frames} 帧耗时 {elapsed:?}，单帧 {} ms，过滤路径疑似退化",
            elapsed.as_millis() * 100 / frames as u128
        );
    }
}
