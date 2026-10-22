//! stui 非交互 CLI 的第三批补充契约集成测试。
//!
//! 本文件覆盖**多段 `--expect`**、`listen --save`、`record --stdout`
//! 这几项能力的外层契约：
//!
//! - `send --expect "41 42|FF"`：多段期望必须**同时**出现在回包里才算通过，
//!   缺哪一段要能从 `expect_missing` 里看出来（退出码 5）。
//! - `listen --save <路径>`：收到的字节原样落盘，JSON 里回显 `out`。
//! - `record --stdout`：stdout 归原始字节所有，因此与 `--json` 互斥。
//!
//! 重要约束：
//! - 只依赖内置虚拟设备 `--port sim`（进程内回环），不打开任何真实串口。
//! - 项目没有 serde_json，这里手写一个极简 JSON 取值器（`field` /
//!   `scan_string` / `unescape` / `top_level_members` / `array_items`）。
//! - 读子进程输出时用**独立线程按行读**、主线程用 `recv_timeout` 轮询，
//!   避免阻塞式读在管道没关干净时把测试卡死。
//! - 临时文件落在 `target/tmp` 下，文件名带进程号与自增序号，
//!   同一轮里的多个测试并行跑也不会互相覆盖。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- 极简 JSON 取值

/// 从 `i`（必须是起始引号）处读一个 JSON 字符串，返回 `(内部原文, 引号之后的位置)`。
fn scan_string(json: &str, i: usize) -> Option<(&str, usize)> {
    let b = json.as_bytes();
    let start = i + 1;
    let mut j = start;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2, // 跳过 `\"` `\\` `\uXXXX` 这类转义
            b'"' => return Some((&json[start..j], j + 1)),
            _ => j += 1,
        }
    }
    None
}

/// 反转义 JSON 字符串内容（够本测试用即可）：`\r \n \t \b \f \uXXXX`，其余转义原样还原。
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{08}'),
            Some('f') => out.push('\u{0C}'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => out.push('\u{fffd}'),
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// 从 `json` 中取出顶层键 `key` 对应的值文本，返回内部原文（字符串不含两侧引号）。
///
/// 只认「字符串 token + 冒号」的组合，不会把字符串**值**内部出现的
/// `"key":` 文本误当成键（`scan_string` 会整体跳过字符串）。
fn field<'a>(json: &'a str, key: &str) -> &'a str {
    let b = json.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        let Some((name, after)) = scan_string(json, i) else { return "" };
        i = after;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        // 冒号前不是键，说明这是数组里的字符串元素，跳过即可。
        if i >= b.len() || b[i] != b':' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return "";
        }
        if b[i] == b'"' {
            let Some((val, end)) = scan_string(json, i) else { return "" };
            i = end;
            if name == key {
                return val;
            }
        } else {
            // 数字 / true / false / null：读到分隔符为止。
            let start = i;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']') && !b[i].is_ascii_whitespace()
            {
                i += 1;
            }
            if name == key {
                return &json[start..i];
            }
        }
    }
    ""
}

/// 解析一个 JSON 值，返回 `(原始值文本, 值之后的位置)`。
///
/// 字符串返回**带引号**的原文；数组 / 对象返回整体原文；字面量读到分隔符为止。
fn scan_value(json: &str, i: usize) -> (String, usize) {
    let b = json.as_bytes();
    if i >= b.len() {
        return (String::new(), i);
    }
    match b[i] {
        b'"' => match scan_string(json, i) {
            Some((_val, end)) => (json[i..end].to_string(), end),
            None => (String::new(), i),
        },
        b'[' | b'{' => {
            let mut depth = 0i32;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => match scan_string(json, j) {
                        Some((_, after)) => j = after,
                        None => break,
                    },
                    b'[' | b'{' => {
                        depth += 1;
                        j += 1;
                    }
                    b']' | b'}' => {
                        depth -= 1;
                        j += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => j += 1,
                }
            }
            (json[i..j].to_string(), j)
        }
        _ => {
            let start = i;
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !b[j].is_ascii_whitespace() {
                j += 1;
            }
            (json[start..j].to_string(), j)
        }
    }
}

/// 解析**顶层** JSON 对象，返回 `(键, 原始值文本)` 列表（保持出现顺序）。
fn top_level_members(json: &str) -> Vec<(String, String)> {
    let b = json.as_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < b.len() && b[i] != b'{' {
        i += 1;
    }
    if i >= b.len() {
        return out;
    }
    i += 1;
    let mut depth = 1i32;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let Some((name, after)) = scan_string(json, i) else { break };
                i = after;
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && b[j] == b':' && depth == 1 {
                    // 这是一个键：取它的值。
                    let mut v = j + 1;
                    while v < b.len() && b[v].is_ascii_whitespace() {
                        v += 1;
                    }
                    let (val, next) = scan_value(json, v);
                    out.push((unescape(name), val));
                    i = next;
                }
            }
            b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// 取顶层键 `key` 对应数组的元素（每个元素是原始 JSON 文本）。
///
/// 找不到该键、或该键的值不是数组时返回 `None`。
fn array_items(json: &str, key: &str) -> Option<Vec<String>> {
    let raw = top_level_members(json)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)?;
    let raw = raw.trim();
    if !raw.starts_with('[') || !raw.ends_with(']') {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    let b = inner.as_bytes();
    let mut items: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => match scan_string(inner, i) {
                Some((_, after)) => i = after,
                None => break,
            },
            b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                depth -= 1;
                i += 1;
            }
            b',' if depth == 0 => {
                let piece = inner[start..i].trim();
                if !piece.is_empty() {
                    items.push(piece.to_string());
                }
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let piece = inner[start..].trim();
    if !piece.is_empty() {
        items.push(piece.to_string());
    }
    Some(items)
}

/// 去掉 JSON 字符串两侧的引号并反转义，得到字符串的**内容**。
///
/// `array_items` 返回的是元素的**原始** JSON 文本（字符串元素带着引号），
/// 拿它跟人读的期望值比较之前要先过这一层。
fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        unescape(&raw[1..raw.len() - 1])
    } else {
        raw.to_string()
    }
}

/// 粗粒度校验：文本是否像一个**合法**的 JSON 对象。
fn looks_like_json_object(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'{' || b[b.len() - 1] != b'}' {
        return false;
    }
    let mut depth = 0i32;
    let mut keys = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let Some((_, after)) = scan_string(s, i) else { return false };
                let mut j = after;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && b[j] == b':' {
                    keys += 1;
                }
                i = after;
            }
            b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    depth == 0 && keys > 0
}

// ------------------------------------------------------------------ 进程调用封装

/// 一次子进程调用的结果：退出码 + stdout 行 + stderr 行。
struct Run {
    code: i32,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl Run {
    /// 失败信息：退出码 + 双管道内容。
    fn report(&self) -> String {
        format!(
            "exit={}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code,
            self.stdout.join("\n"),
            self.stderr.join("\n")
        )
    }

    /// stdout 拼回一段（NDJSON 场景下是多行）。
    fn stdout_text(&self) -> String {
        self.stdout.join("\n")
    }

    /// stderr 拼回一段。
    fn stderr_text(&self) -> String {
        self.stderr.join("\n")
    }
}

/// 运行 `stui <args...>`，把 stdout / stderr 都按行收齐。
///
/// - stdout / stderr 各自交给一个独立线程按行读（`BufReader::lines`），
///   避免在管道写满时互相堵塞。
/// - 主线程用 `recv_timeout` 轮询 stderr 管道：既收到了行，又给「子进程是否已
///   结束」的检查留出了节拍（与 `cli_contract.rs` 里
///   `sim_loopback_delivers_to_concurrent_client` 同一套路）。
/// - 超过 `budget_ms` 仍未结束就杀掉子进程并 panic，绝不留下游离进程。
fn run_capture(args: &[&str], budget_ms: u64) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_stui"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("无法启动 stui（{args:?}）：{e}"));

    let out_pipe = child.stdout.take().expect("子进程缺少 stdout 管道");
    let err_pipe = child.stderr.take().expect("子进程缺少 stderr 管道");
    let (tx_out, rx_out) = mpsc::channel::<String>();
    let (tx_err, rx_err) = mpsc::channel::<String>();
    let reader_out = std::thread::spawn(move || {
        for line in BufReader::new(out_pipe).lines().map_while(Result::ok) {
            if tx_out.send(line).is_err() {
                break;
            }
        }
    });
    let reader_err = std::thread::spawn(move || {
        for line in BufReader::new(err_pipe).lines().map_while(Result::ok) {
            if tx_err.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let mut stdout: Vec<String> = Vec::new();
    let mut stderr: Vec<String> = Vec::new();
    let mut err_open = true;
    let mut status = None;
    while Instant::now() < deadline {
        // 主线程用 recv_timeout 轮询 stderr：管道关闭后 recv_timeout 会立刻返回
        // Disconnected，这时改成睡一小会儿再查退出状态。
        if err_open {
            match rx_err.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => stderr.push(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => err_open = false,
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
        match child.try_wait() {
            Ok(Some(st)) => {
                status = Some(st);
                break;
            }
            Ok(None) => {}
            Err(e) => panic!("等待 stui（{args:?}）失败：{e}"),
        }
    }
    let status = match status {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stui（{args:?}）在 {budget_ms} ms 内没有结束");
        }
    };

    // 进程已退出，把两个管道里剩下的行收干净。
    let drain_deadline = Instant::now() + Duration::from_millis(2000);
    for (rx, sink) in [(&rx_out, &mut stdout), (&rx_err, &mut stderr)] {
        loop {
            let left = drain_deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(line) => sink.push(line),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    let _ = reader_out.join();
    let _ = reader_err.join();

    Run {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    }
}

/// 断言退出码，失败时带上完整现场。
fn assert_code(r: &Run, want: i32) {
    assert_eq!(r.code, want, "退出码不符，期望 {want}\n{}", r.report());
}

/// 从 stdout 的第一行里取字段，缺失时给出可读报错。
fn f(r: &Run, key: &str) -> String {
    let line = r.stdout.first().cloned().unwrap_or_default();
    let v = field(&line, key).to_string();
    assert!(!v.is_empty(), "stdout JSON 中缺少字段 \"{key}\"\n{}", r.report());
    v
}

/// 取 stdout 第一行里的字段值，**允许空字符串**（但字段必须存在）。
///
/// `f` 的判据是「非空」，所以像 `rx_hex: ""` 这种合法的空值只能用这个取。
fn fv(r: &Run, key: &str) -> String {
    let line = r.stdout.first().cloned().unwrap_or_default();
    let raw = top_level_members(&line)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("stdout JSON 中缺少字段 \"{key}\"\n{}", r.report()));
    // 原始值带引号的是字符串，去掉引号并反转义。
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        unescape(&raw[1..raw.len() - 1])
    } else {
        raw
    }
}

// ------------------------------------------------------------------ 临时文件路径

/// 生成本次运行独有的临时文件路径（放在 target/tmp 下）。
///
/// 名字里带进程号与自增序号：同一个二进制里的多个测试并行跑也不会互相覆盖，
/// 跨次运行残留的文件也不会被上一轮的结果污染。
fn temp_path(tag: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("stui-ext3-{tag}-{}-{n}.bin", std::process::id()))
}

// ------------------------------------------------- 1. send --expect 单段命中

/// `send --port sim --text hi --expect "6869" --json`：
/// 回环把 "hi"（十六进制 6869）原样送回，期望命中 → 退出码 0、expect_ok=true。
///
/// 断言意图：
/// - 退出码 0：期望命中就是成功，不能混进「期望不符」那一档（5）。
/// - `expect_ok` / `expect_checked` 都是固定 schema 里的一列：脚本按固定字段取值，
///   不用先判断这一轮有没有用 `--expect`。
/// - `expect_missing` 恒定存在：全命中时是空数组，脚本按同一个 schema 解析。
#[test]
fn send_expect_single_match_exits_zero_with_expect_ok_true() {
    let r = run_capture(
        &["send", "--port", "sim", "--text", "hi", "--expect", "6869", "--json"],
        20_000,
    );

    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "期望命中时整体应成功\n{}", r.report());
    assert_eq!(r.stdout.len(), 1, "send 的 JSON 应是单行\n{}", r.report());
    assert_eq!(f(&r, "expect_ok"), "true", "期望命中时 expect_ok 应为 true\n{}", r.report());
    // 真的校验过（不是 dry-run 那种「没校验但也没失败」）。
    assert_eq!(
        f(&r, "expect_checked"),
        "true",
        "真的发了也真的校验了，expect_checked 应为 true\n{}",
        r.report()
    );
    // 校验目标是哪串字节，要原样回显。
    assert_eq!(f(&r, "expect_hex"), "6869", "expect_hex 应为 6869\n{}", r.report());
    // 全命中时没有缺失段：字段恒定存在，但内容是空数组。
    let missing = array_items(&r.stdout.first().cloned().unwrap_or_default(), "expect_missing")
        .expect("expect_missing 应是数组");
    assert!(missing.is_empty(), "全命中时 expect_missing 应为空数组\n{}", r.report());
    // 数据确实发出去了、也收到了：期望校验的前提是「有回包可校」。
    assert_eq!(f(&r, "sent"), "2", "\"hi\" 应已发出 2 字节\n{}", r.report());
    assert_eq!(f(&r, "received"), "2", "回环应收到 2 字节\n{}", r.report());
    assert_eq!(f(&r, "rx_hex"), "6869", "rx_hex 应为 6869\n{}", r.report());
}

// ------------------------------------------- 2. send --expect 多段：有一段没命中

/// `send --port sim --text hi --expect "4142|6869" --json`：
/// 回包里只有 6869（"hi"），没有 4142 → 退出码 **5**、expect_ok=false，
/// 并且 `expect_missing` 里要**点名**没命中的那一段。
///
/// 断言意图：
/// - 多期望是「与」的关系：只要有一段没出现，整轮就是「期望不符」（退出码 5），
///   不能因为「有一段命中了」就报成功。
/// - `expect_missing` 是多期望场景的关键字段：`expect_ok=false` 只说「没齐」，
///   这个字段才说清「缺哪一段」，脚本才能把失败原因写给用户看。
/// - 缺失的是 4142（**第一段**）：载荷 "hi" 的回环只有 6869。
///   （本轮需求里写的是「第二个不匹配」，但对 `--text hi` 来说，
///   实际没命中的是第一段 4142 —— 这里按实现的真实行为断言。）
#[test]
fn send_expect_multi_with_one_missing_exits_five_and_names_it() {
    let r = run_capture(
        &[
            "send",
            "--port",
            "sim",
            "--text",
            "hi",
            "--expect",
            "4142|6869",
            "--json",
        ],
        20_000,
    );

    assert_code(&r, 5);
    assert_eq!(f(&r, "ok"), "false", "期望不符时整体应 ok:false\n{}", r.report());
    assert_eq!(
        f(&r, "expect_ok"),
        "false",
        "有一段没命中时 expect_ok 应为 false\n{}",
        r.report()
    );
    // 校验确实做了（不然「不符」就没有依据）。
    assert_eq!(
        f(&r, "expect_checked"),
        "true",
        "expect_checked 应为 true\n{}",
        r.report()
    );
    // 两段期望都要回显出来，逗号分隔。
    assert_eq!(
        f(&r, "expect_hex"),
        "4142,6869",
        "多段期望应原样回显（逗号分隔）\n{}",
        r.report()
    );
    // 没命中的那一段要出现在 expect_missing 里：这里只有 4142。
    let line = r.stdout.first().cloned().unwrap_or_default();
    let missing = array_items(&line, "expect_missing").expect("expect_missing 应是数组");
    assert_eq!(missing.len(), 1, "应当只点名没命中的那一段\n{}", r.report());
    assert_eq!(
        unquote(&missing[0]),
        "4142",
        "没命中的应当是 4142\n{}",
        r.report()
    );
    // 数据本身是发出去了、也收到了：失败原因是「内容不符」，不是「没发出去」。
    assert_eq!(f(&r, "sent"), "2", "\"hi\" 应已发出 2 字节\n{}", r.report());
    assert_eq!(f(&r, "rx_hex"), "6869", "实际收到的是 6869\n{}", r.report());
    // 失败原因也要给出来。
    let err = unescape(&f(&r, "error"));
    assert!(!err.is_empty() && err != "null", "失败时应给出 error\n{}", r.report());
}

/// `send --port sim --text hi --expect "6869|4142" --json`：
/// 第一段命中、第二段没命中 → 同样退出码 5，且 `expect_missing` 里点名 4142。
///
/// 与上一条的区别只在于「哪一段没命中」：这一条把没命中的放在**第二位**，
/// 证明 `expect_missing` 报的是真正缺的那段，而不是固定报第一段或最后一段。
#[test]
fn send_expect_multi_missing_second_segment_is_reported() {
    let r = run_capture(
        &[
            "send",
            "--port",
            "sim",
            "--text",
            "hi",
            "--expect",
            "6869|4142",
            "--json",
        ],
        20_000,
    );

    assert_code(&r, 5);
    assert_eq!(
        f(&r, "expect_ok"),
        "false",
        "有一段没命中时 expect_ok 应为 false\n{}",
        r.report()
    );
    assert_eq!(
        f(&r, "expect_hex"),
        "6869,4142",
        "多段期望应原样回显\n{}",
        r.report()
    );
    let line = r.stdout.first().cloned().unwrap_or_default();
    let missing = array_items(&line, "expect_missing").expect("expect_missing 应是数组");
    assert_eq!(missing.len(), 1, "应当只点名没命中的那一段\n{}", r.report());
    assert_eq!(
        unquote(&missing[0]),
        "4142",
        "没命中的应当是第二段 4142\n{}",
        r.report()
    );
}

// ------------------------------------------ 3. send --expect 多段：全部命中

/// 多段期望**全部**命中时整轮成功：退出码 0、expect_ok=true、expect_missing 为空。
///
/// 载荷用 "ABhi"（回环回来是 41 42 68 69）：只有它才同时含有 4142 与 6869 两段。
/// 载荷用 "hi" 时 4142 永远不可能出现，两个期望不可能同时命中
/// （这一点由上面两条用例覆盖）。
///
/// 断言意图：
/// - 「与」的关系成立时不能误报失败：都齐了就该是 0。
/// - `expect_missing` 恒定存在且此时为空数组：脚本可以按固定 schema 解析。
#[test]
fn send_expect_multi_all_matching_exits_zero() {
    let r = run_capture(
        &[
            "send",
            "--port",
            "sim",
            "--text",
            "ABhi",
            "--expect",
            "4142|6869",
            "--json",
        ],
        20_000,
    );

    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "两段都命中时整体应成功\n{}", r.report());
    assert_eq!(
        f(&r, "expect_ok"),
        "true",
        "两段都命中时 expect_ok 应为 true\n{}",
        r.report()
    );
    assert_eq!(
        f(&r, "expect_checked"),
        "true",
        "expect_checked 应为 true\n{}",
        r.report()
    );
    assert_eq!(
        f(&r, "expect_hex"),
        "4142,6869",
        "多段期望应原样回显\n{}",
        r.report()
    );
    let line = r.stdout.first().cloned().unwrap_or_default();
    let missing = array_items(&line, "expect_missing").expect("expect_missing 应是数组");
    assert!(missing.is_empty(), "全命中时 expect_missing 应为空数组\n{}", r.report());
    // 回包 4 字节，两段期望都从这里命中。
    assert_eq!(f(&r, "received"), "4", "回环应收到 4 字节\n{}", r.report());
    assert_eq!(f(&r, "rx_hex"), "41426869", "rx_hex 应为 41426869\n{}", r.report());
}

// ------------------------------------------------------- 4. listen --save 落盘

/// `listen --port sim --timeout 300 --save <文件> --json`：
/// 退出码 0（收满）或 4（超时），JSON 里有 `out` 字段，且保存文件**真的被创建**。
///
/// 断言意图：
/// - `out` 必须回显成调用方给的那个路径：脚本要知道去哪儿取文件。
/// - 文件必须存在 —— 否则「`--save` 把收到的字节原样落盘」这句话就是空的。
/// - 文件长度必须与 JSON 报告的 `received` 一致：
///   「报告收了多少」和「文件里有多少」必须对得上，否则落盘就是残的。
#[test]
fn listen_with_save_creates_file_and_reports_out_path() {
    let save = temp_path("listen-save");
    // 清掉可能存在的残留，保证「文件被创建」测的是本次运行。
    let _ = std::fs::remove_file(&save);
    let save_str = save.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "listen",
            "--port",
            "sim",
            "--timeout",
            "300",
            "--save",
            &save_str,
            "--json",
        ],
        20_000,
    );

    // 收满是 0、没收满是 4：这两种都属于「正常跑完」，不能是 1/2/3。
    assert!(
        r.code == 0 || r.code == 4,
        "listen 的退出码应为 0（收满）或 4（超时），实际 {}\n{}",
        r.code,
        r.report()
    );
    // stdout 是单行 JSON 对象。
    assert_eq!(r.stdout.len(), 1, "listen 的 JSON 应是单行\n{}", r.report());
    let line = r.stdout.first().cloned().unwrap_or_default();
    assert!(
        looks_like_json_object(&line),
        "listen 输出应是合法 JSON 对象\n{}",
        r.report()
    );
    // out 字段必须存在，且与 --save 给的路径一致（路径里的反斜杠是 JSON 转义，要反转义）。
    assert!(
        top_level_members(&line).iter().any(|(k, _)| k == "out"),
        "listen --save 的结果里应有 out 字段\n{}",
        r.report()
    );
    assert_eq!(
        unescape(&f(&r, "out")),
        save_str,
        "out 应回显 --save 给的路径\n{}",
        r.report()
    );

    // 文件必须被创建出来。
    assert!(save.exists(), "listen --save 应创建文件 {}\n{}", save_str, r.report());
    // 文件长度与报告的 received 一致。
    let received: u64 = f(&r, "received")
        .parse()
        .unwrap_or_else(|e| panic!("received 应是数字\n{}\n{e}", r.report()));
    let len = std::fs::metadata(&save).expect("应能读取保存文件元数据").len();
    assert_eq!(
        len, received,
        "保存文件长度应与报告的 received 一致\n{}",
        r.report()
    );

    let _ = std::fs::remove_file(&save);
}

/// `listen --port sim --timeout 300 --save <文件> --json`：
/// sim 是进程内回环且没人往里写，300 ms 内收不到数据 → 退出码 4、
/// `error` 是 "timeout"，但**文件照样被创建**。
///
/// 断言意图：超时不等于「什么都没做」——`--save` 的文件应当在收工前就已经建好，
/// 脚本据此判断「这一轮落盘了，只是没收到东西」。
#[test]
fn listen_save_file_exists_even_when_receive_times_out() {
    let save = temp_path("listen-save-timeout");
    let _ = std::fs::remove_file(&save);
    let save_str = save.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "listen",
            "--port",
            "sim",
            "--timeout",
            "300",
            "--save",
            &save_str,
            "--json",
        ],
        20_000,
    );

    // sim 下没人往回环里写数据，所以确定性地是超时。
    assert_code(&r, 4);
    assert_eq!(f(&r, "ok"), "false", "超时时 ok 应为 false\n{}", r.report());
    assert_eq!(f(&r, "error"), "timeout", "error 应为 \"timeout\"\n{}", r.report());
    assert_eq!(f(&r, "received"), "0", "超时时 received 应为 0\n{}", r.report());
    // 一个字节都没收到：rx_hex 是空字符串（字段存在，值为空）。
    assert_eq!(fv(&r, "rx_hex"), "", "超时时 rx_hex 应为空\n{}", r.report());
    // 超时也要有 out：文件建了，路径就得给出去。
    assert_eq!(
        unescape(&f(&r, "out")),
        save_str,
        "超时结果里也应回显 out\n{}",
        r.report()
    );
    assert!(save.exists(), "超时时也应创建保存文件\n{}", r.report());

    let _ = std::fs::remove_file(&save);
}

// ------------------------------------------------------------ 5. record --stdout

/// `record --port sim --duration 300 --stdout --json`：
/// `--stdout` 与 `--json` 互斥 → 用法错误（退出码 2）、ok=false，
/// error 要说清「两者不能同时用」。
///
/// 断言意图：stdout 在 `--stdout` 模式下归**原始字节**所有，
/// JSON 汇总再写进去就混在一起了。实现不猜调用方想要哪个，直接报用法错误，
/// 脚本因此能靠退出码 2 发现自己拼错了参数。
#[test]
fn record_rejects_stdout_together_with_json() {
    let out = temp_path("record-stdout-json");
    let _ = std::fs::remove_file(&out);
    let out_str = out.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "record",
            "--port",
            "sim",
            "--duration",
            "300",
            "--stdout",
            "--out",
            &out_str,
            "--json",
        ],
        20_000,
    );

    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "用法错误时 ok 应为 false\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(!err.is_empty() && err != "null", "error 应为具体错误说明\n{}", r.report());
    // 错误信息要点名冲突的两个参数：脚本要能据此改命令行。
    assert!(err.contains("--stdout"), "error 应点名 --stdout：{err}\n{}", r.report());
    assert!(err.contains("--json"), "error 应点名 --json：{err}\n{}", r.report());

    let _ = std::fs::remove_file(&out);
}

/// `record --port sim --duration 300 --stdout --out <文件>`（不带 --json）：
/// 退出码 0；**stdout 里没有 JSON 汇总**（stdout 归原始字节所有），
/// 汇总行改走 stderr；记录文件照样被创建。
///
/// 断言意图：
/// - 「stdout 是原始字节流」这条承诺的实质是「stdout 里不会混进 JSON」：
///   脚本可以直接 `stui record ... --stdout > raw.bin` 拿到原始报文。
/// - 汇总挪到 stderr，是为了让 stdout 保持字节级干净。
/// - sim 是进程内回环且 record 自己不写入，没有数据源，所以原始字节流是空的；
///   这正是「stdout 里不该有别的东西」的证据。
#[test]
fn record_stdout_keeps_stdout_free_of_json_and_summarizes_on_stderr() {
    let out = temp_path("record-stdout");
    let _ = std::fs::remove_file(&out);
    let out_str = out.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "record",
            "--port",
            "sim",
            "--duration",
            "300",
            "--stdout",
            "--out",
            &out_str,
        ],
        20_000,
    );

    assert_code(&r, 0);
    // stdout 不能是 JSON：它要么是原始字节，要么是空的，但绝不能是汇总对象。
    let stdout_text = r.stdout_text();
    assert!(
        !stdout_text.contains('{'),
        "--stdout 时 stdout 里不该有 JSON 汇总：{stdout_text}\n{}",
        r.report()
    );
    for line in &r.stdout {
        assert!(
            !looks_like_json_object(line),
            "--stdout 时 stdout 不该出现 JSON 行：{line}\n{}",
            r.report()
        );
    }
    // 汇总行改走 stderr。
    let stderr_text = r.stderr_text();
    assert!(
        stderr_text.contains("已记录"),
        "汇总应改走 stderr\n{}",
        r.report()
    );
    assert!(
        !stderr_text.contains("\"ok\":"),
        "stderr 里也不该出现 JSON 汇总\n{}",
        r.report()
    );
    // sim 没有数据源，所以原始字节流是空的（这正是 stdout 里没有别的东西的证据）。
    assert!(
        stdout_text.trim().is_empty(),
        "没有数据源时 stdout 应为空，实际：{stdout_text:?}\n{}",
        r.report()
    );
    // 记录文件照样被创建。
    assert!(out.exists(), "record 应创建记录文件 {}\n{}", out_str, r.report());

    let _ = std::fs::remove_file(&out);
}

/// `record --port sim --duration 300 --out <文件>`（不带 --stdout、不带 --json）：
/// 汇总行走 **stdout**（stdout 没被原始字节占住时就是默认行为），
/// 且 stdout 里同样没有 JSON 汇总。
///
/// 断言意图：与上一条形成对照 —— 汇总落在 stdout 还是 stderr，
/// 取决于 stdout 归谁所有；两种模式下 stdout 都不会混进 JSON 对象。
#[test]
fn record_without_stdout_summarizes_on_stdout() {
    let out = temp_path("record-plain");
    let _ = std::fs::remove_file(&out);
    let out_str = out.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &["record", "--port", "sim", "--duration", "300", "--out", &out_str],
        20_000,
    );

    assert_code(&r, 0);
    let stdout_text = r.stdout_text();
    assert!(
        stdout_text.contains("已记录"),
        "不用 --stdout 时汇总应走 stdout\n{}",
        r.report()
    );
    assert!(
        !stdout_text.contains('{'),
        "文本模式下 stdout 里不该有 JSON：{stdout_text}\n{}",
        r.report()
    );
    assert!(out.exists(), "record 应创建记录文件 {}\n{}", out_str, r.report());

    let _ = std::fs::remove_file(&out);
}

// ------------------------------------------------------------------- 6. --help

/// `--help` 必须把本轮新增的子命令与参数都写出来，脚本 / AI 才能照着调用。
///
/// 断言意图：文案里没有的能力，调用方根本不会知道它存在；
/// 退出码表也要写明 5（响应与期望不符）这个新码的含义。
#[test]
fn help_lists_new_subcommands_and_flags() {
    let r = run_capture(&["--help"], 10_000);

    assert_code(&r, 0);
    let text = r.stdout_text();
    for token in [
        "record",           // 新子命令
        "--out",            // record 的落盘路径
        "--duration",       // record 的录制时长
        "--stdout",         // record 的原始字节冲一份到 stdout
        "--save",           // listen 的落盘路径
        "--expect",         // send 的期望响应校验
        "--expect-timeout", // 期望响应的时限
        "--format",         // listen / record 的输出形状
        "--idle",           // listen 空闲收工
        "--count",          // sim 重复发送次数
        "退出码",           // 退出码表
    ] {
        assert!(text.contains(token), "--help 输出缺少 {token}\n{}", r.report());
    }
    // 多段期望的写法也要写出来：「竖线分隔」是这一轮新增的用法。
    assert!(
        text.contains("竖线分隔"),
        "--help 应说明 --expect 可用竖线分隔多段\n{}",
        r.report()
    );
    // 退出码 5 的含义必须写清楚。
    assert!(
        text.contains("响应与期望不符"),
        "--help 应写明退出码 5 的含义\n{}",
        r.report()
    );
}
