//! stui 非交互 CLI 的第三批契约集成测试。
//!
//! 本文件覆盖本轮新增 / 新固化的 CLI 能力：
//! - `record`：把收到的数据原样落盘到 `--out` 指定的文件，并给出汇总 JSON；
//! - `send --expect`：校验设备回包里是否出现期望的字节序列（含专门的退出码 5）；
//! - `listen --format ndjson`：流式逐块输出，最后一行是带 `done` 的汇总行；
//! - `list --json`：每个端口对象都带 `manufacturer` / `product` 两列；
//! - `--help`：新子命令与新参数必须写出来，脚本 / AI 才知道怎么调。
//!
//! 重要约束：
//! - 测试**只依赖内置虚拟设备 `--port sim`**（进程内回环），不打开任何真实
//!   COM / tty 串口，因此在没有串口硬件的机器上也能稳定运行。
//! - 项目没有 serde_json，这里沿用既有文件里的极简 JSON 取值器
//!   （`field` / `scan_string` / `unescape` / `top_level_members` /
//!   `array_items` / `looks_like_json_object`）。
//! - 读子进程输出时用**独立线程按行读**、主线程用 `recv_timeout` 轮询，
//!   避免阻塞式读在管道没关干净时把测试卡死。
//! - 临时文件一律放在 Cargo 为集成测试准备的 `CARGO_TARGET_TMPDIR`（target/tmp）
//!   下，文件名带进程号与自增序号：既不污染仓库根目录，测试之间也不会互相覆盖。

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
/// 与 `cli_contract.rs` / `cli_contract_ext.rs` 里的同名函数同一套语义：
/// 只认「字符串 token + 冒号」的组合，不会把字符串**值**内部出现的
/// `"key":` 文本误当成键。
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

// ------------------------------------------------------------------ 临时文件路径

/// 生成本次运行独有的临时文件路径（放在 target/tmp 下）。
///
/// 名字里带进程号与自增序号：同一个二进制里的多个测试并行跑也不会互相覆盖，
/// 跨次运行残留的文件也不会被上一轮的结果污染。
fn temp_path(tag: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("stui-{tag}-{}-{n}.bin", std::process::id()))
}

// ------------------------------------------------------------------ 1. record

/// `record --port sim --duration 300 --out <文件> --json`：
/// 退出码 0，汇总 JSON 里有 ok / bytes / chunks / out 字段，并且文件真的被创建。
///
/// 断言意图：
/// - `ok=true`：录完这一轮是「成功收工」，不是中途失败；
/// - `bytes` / `chunks`：告诉脚本这一轮到底录到多少（没有数据源时是 0）；
/// - `out`：回显落盘路径，脚本才知道去哪儿取文件；
/// - 文件必须**真的存在**——否则「record 把数据原样落盘」这句话就是空的。
#[test]
fn record_creates_out_file_and_reports_summary() {
    let out = temp_path("record-summary");
    // 清掉可能存在的残留，保证「文件被创建」这条断言测的是本次运行。
    let _ = std::fs::remove_file(&out);
    let out_str = out.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "record",
            "--port",
            "sim",
            "--duration",
            "300",
            "--out",
            &out_str,
            "--json",
        ],
        20_000,
    );

    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "record 收工应 ok:true\n{}", r.report());
    // bytes / chunks 是数字型字段，必须能原样取到。
    let bytes: u64 = f(&r, "bytes")
        .parse()
        .unwrap_or_else(|e| panic!("bytes 应是数字\n{}\n{e}", r.report()));
    let chunks: u64 = f(&r, "chunks")
        .parse()
        .unwrap_or_else(|e| panic!("chunks 应是数字\n{}\n{e}", r.report()));
    // out 必须回显成调用方给的那个路径（路径里的反斜杠是 JSON 转义，要反转义）。
    assert_eq!(
        unescape(&f(&r, "out")),
        out_str,
        "out 应回显 --out 给的路径\n{}",
        r.report()
    );

    // 文件必须被创建出来，且长度与报告的字节数一致：
    // 「报告录了多少」和「文件里有多少」必须对得上。
    assert!(out.exists(), "record 应创建输出文件 {}\n{}", out_str, r.report());
    let len = std::fs::metadata(&out).expect("应能读取输出文件元数据").len();
    assert_eq!(len, bytes, "文件长度应与报告的 bytes 一致\n{}", r.report());
    // sim 是进程内回环、record 自己不写入，因此没有数据块，块数为 0。
    assert_eq!(chunks, 0, "没有数据源时 chunks 应为 0\n{}", r.report());

    let _ = std::fs::remove_file(&out);
}

// ------------------------------------------------------- 2. send --expect 命中

/// `send --port sim --text hi --expect 6869 --json`：
/// 回环把 "hi" 原样送回，期望的 6869 出现在收到的字节里 → 退出码 0，expect_ok=true。
///
/// `expect_ok` 是固定 schema 里的一列：脚本按固定字段取值，
/// 不用先判断「这一轮到底有没有用 --expect」。
#[test]
fn send_expect_matching_echo_reports_expect_ok_true() {
    let r = run_capture(
        &["send", "--port", "sim", "--text", "hi", "--expect", "6869", "--json"],
        20_000,
    );

    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "期望命中时整体应成功\n{}", r.report());
    assert_eq!(f(&r, "expect_ok"), "true", "期望命中时 expect_ok 应为 true\n{}", r.report());
    // 期望目标被原样回显，便于脚本确认「校验的是哪串字节」。
    assert_eq!(f(&r, "expect_hex"), "6869", "expect_hex 应为 6869\n{}", r.report());
    // 收到的确实就是 "hi"（2 字节）。
    assert_eq!(f(&r, "received"), "2", "回环应收到 2 字节\n{}", r.report());
    assert_eq!(f(&r, "rx_hex"), "6869", "rx_hex 应为 6869\n{}", r.report());
}

// ------------------------------------------------------ 3. send --expect 不命中

/// `send --port sim --text hi --expect 414243 --json`：
/// 期望的字节序列（"ABC"）在回环里根本没有 → 退出码 **5**，expect_ok=false。
///
/// 退出码 5 是专门留给「操作成功但结果不符合预期」的：
/// 脚本要能把它和「运行失败（1）」区分开——后者是环境问题，前者是调参问题。
#[test]
fn send_expect_absent_bytes_exits_with_code_five() {
    let r = run_capture(
        &["send", "--port", "sim", "--text", "hi", "--expect", "414243", "--json"],
        20_000,
    );

    assert_code(&r, 5);
    assert_eq!(f(&r, "expect_ok"), "false", "期望未出现时 expect_ok 应为 false\n{}", r.report());
    // 整体不 ok，且带一句失败原因。
    assert_eq!(f(&r, "ok"), "false", "期望未出现时整体应 ok:false\n{}", r.report());
    assert!(!f(&r, "error").is_empty(), "失败时应给出 error\n{}", r.report());
    // 期望目标仍然回显出来：脚本要能看出「等的是哪串字节」。
    assert_eq!(f(&r, "expect_hex"), "414243", "expect_hex 应为 414243\n{}", r.report());
    // 数据其实发出去了、也收到了，只是内容不符——这一点必须能区分出来。
    assert_eq!(f(&r, "sent"), "2", "\"hi\" 应已发出 2 字节\n{}", r.report());
    assert_eq!(f(&r, "rx_hex"), "6869", "实际收到的是 6869\n{}", r.report());
}

// ------------------------------------------- 4. send 奇数位十六进制 + --expect

/// `send --port sim --hex ABC --expect 41 --json`：
/// 奇数位十六进制是**参数错误**（退出码 2），不是运行错误也不是期望不匹配。
///
/// 断言意图：载荷都还没解析成功，校验那一步根本没轮得到，
/// 因此退出码必须是 2（用法错误），不能是 5（期望不符）。
#[test]
fn send_rejects_odd_length_hex_before_expect_checking() {
    let r = run_capture(
        &["send", "--port", "sim", "--hex", "ABC", "--expect", "41", "--json"],
        20_000,
    );

    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "参数错误时 ok 应为 false\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(!err.is_empty() && err != "null", "error 应为具体错误说明\n{}", r.report());
    // 错误信息要指出出错的标记本身与原因。
    assert!(err.contains("ABC"), "error 应指出出错的标记 ABC：{err}\n{}", r.report());
    assert!(err.contains("奇数"), "error 应说明位数是奇数：{err}\n{}", r.report());
}

// ----------------------------------------------- 4b. send --dry-run 的口径

/// `send --dry-run --text hi --expect 6869 --json`：
/// 什么都没发，也就什么都没校验 —— `expect_checked` 必须是 false。
///
/// 断言意图：`expect_ok` 只说「有没有失败」，dry-run 下它恒为 true；
/// 脚本若只看 `expect_ok` 会误判成「设备回包已校验通过」。
/// 所以另给一个 `expect_checked` 说清楚「到底校没校验」。
#[test]
fn send_dry_run_marks_the_expect_as_not_checked() {
    let r = run_capture(
        &["send", "--dry-run", "--text", "hi", "--expect", "6869", "--json"],
        20_000,
    );

    assert_code(&r, 0);
    assert_eq!(
        f(&r, "expect_checked"),
        "false",
        "dry-run 没校验过，expect_checked 应为 false\n{}",
        r.report()
    );
    // 期望目标仍然给出来，脚本才知道「本来要校验的是哪串字节」。
    assert_eq!(
        f(&r, "expect_hex"),
        "6869",
        "expect_hex 应为 6869\n{}",
        r.report()
    );
}

/// `send --dry-run --bytes abc --json`：参数非法仍是退出码 2。
///
/// 断言意图：dry-run 只跳过「发送 / 等待」这个动作，
/// 不该跳过「参数写得对不对」—— 否则同一份脚本在 dry-run 与真跑之间
/// 得到两套退出码语义。
#[test]
fn send_dry_run_still_rejects_invalid_parameters() {
    let r = run_capture(
        &["send", "--dry-run", "--text", "hi", "--bytes", "abc", "--json"],
        20_000,
    );

    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "参数错误时 ok 应为 false\n{}", r.report());
}

// ------------------------------------------------------- 5. listen --format ndjson

/// `listen --port sim --timeout 300 --format ndjson --json`：
/// 退出码 0（收满）或 4（超时），stdout 的**每一行**都是合法 JSON，
/// 且最后一行带 `done` 字段。
///
/// 断言意图：
/// - NDJSON 的前提是「一行一个 JSON」，中间混进非 JSON 行会让流式消费方崩掉；
/// - `done` 标出「这是最后一行」，消费方据此判断流是否结束；
/// - 退出码 0/4 都合法：sim 是进程内回环、没有外部数据源，超时是常态，
///   但两种退出码都属于「正常跑完这一轮」。
#[test]
fn listen_ndjson_emits_only_json_lines_and_ends_with_done() {
    let r = run_capture(
        &["listen", "--port", "sim", "--timeout", "300", "--format", "ndjson", "--json"],
        20_000,
    );

    assert!(
        r.code == 0 || r.code == 4,
        "listen ndjson 的退出码应是 0（收满）或 4（超时），实际 {}\n{}",
        r.code,
        r.report()
    );
    // ndjson 模式下 stdout 只能是 JSON 行，错误信息不许混进来。
    assert!(r.stderr.is_empty(), "listen ndjson 不应往 stderr 写东西\n{}", r.report());
    assert!(!r.stdout.is_empty(), "listen ndjson 应至少输出一行\n{}", r.report());

    for (n, line) in r.stdout.iter().enumerate() {
        assert!(
            looks_like_json_object(line),
            "listen ndjson 第 {} 行不是合法 JSON 对象：{line}\n{}",
            n + 1,
            r.report()
        );
    }
    // 最后一行是带 done 的汇总行。
    let last = r.stdout.last().expect("stdout 不应为空");
    assert_eq!(
        field(last, "done"),
        "true",
        "listen ndjson 的最后一行应是带 done 的汇总行\n{}",
        r.report()
    );
}

// ------------------------------------------------------------ 6. list 的厂商/产品

/// `list --json`：每个端口对象里都必须有 `manufacturer` 与 `product` 两列。
///
/// 断言意图：这两列是脚本识别设备型号的依据（同名设备靠 vendor/product 区分），
/// 所以**字段本身必须存在**——系统没报出来时给空串，而不是把字段省掉，
/// 否则脚本按固定 schema 取值会取到「字段缺失」。
///
/// 不假设列表非空：不同机器枚举到的串口不一样，空数组同样合法。
#[test]
fn list_ports_expose_manufacturer_and_product_fields() {
    let r = run_capture(&["list", "--json"], 20_000);

    assert_code(&r, 0);
    let line = r.stdout.first().expect("list 应输出一行");
    assert!(
        looks_like_json_object(line),
        "list 输出应是合法 JSON 对象\n{}",
        r.report()
    );
    assert_eq!(field(line, "ok"), "true", "list 的 ok 应为 true\n{}", r.report());

    let ports = array_items(line, "ports").expect("list 输出应含 ports 数组");
    for p in &ports {
        assert!(
            looks_like_json_object(p),
            "ports 元素应是 JSON 对象：{p}\n{}",
            r.report()
        );
        // 字段必须存在（值可以是空串，但键不能少）。
        for key in ["manufacturer", "product"] {
            let present = top_level_members(p).iter().any(|(k, _)| k == key);
            assert!(
                present,
                "端口对象缺少 {key} 字段：{p}\n{}",
                r.report()
            );
        }
    }
    // count 必须与数组长度一致，否则脚本按 count 取数会取错。
    if let Some(count) = top_level_members(line)
        .into_iter()
        .find(|(k, _)| k == "count")
        .map(|(_, v)| v)
    {
        assert_eq!(
            count,
            ports.len().to_string(),
            "count 应与 ports 数组长度一致\n{}",
            r.report()
        );
    }
}

// ------------------------------------------------------------------- 7. --help

/// `--help` 里必须出现 `record`、`--expect`、`--format`、`watch`、`selftest`。
///
/// 断言意图：面向脚本 / AI 的调用方只能照着 `--help` 学怎么调，
/// 没写进帮助的能力就等于不存在。
#[test]
fn help_lists_record_expect_format_watch_and_selftest() {
    let r = run_capture(&["--help"], 20_000);

    assert_code(&r, 0);
    let text = r.stdout_text();
    for token in ["record", "--expect", "--format", "watch", "selftest"] {
        assert!(text.contains(token), "--help 输出缺少 {token}\n{}", r.report());
    }
}

// ------------------------------------------- 8. record --json 与 --format text（补强）

/// `record --port sim --duration 300 --out <文件> --json --format text`：
/// `--json` 意味着 stdout 全是 JSON 行，而 `--format text` 是给人看的整块文本，
/// 两者混在一起会让按行解析的下游崩掉 → 用法错误（退出码 2）。
///
/// 断言意图：两个参数互相矛盾时，实现不猜调用方想要哪个，
/// 而是当场报出来，脚本靠退出码 2 就能发现自己拼错了参数。
#[test]
fn record_rejects_json_together_with_format_text() {
    let out = temp_path("record-json-text");
    let _ = std::fs::remove_file(&out);
    let out_str = out.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "record",
            "--port",
            "sim",
            "--duration",
            "300",
            "--out",
            &out_str,
            "--json",
            "--format",
            "text",
        ],
        20_000,
    );

    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "用法错误时 ok 应为 false\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(!err.is_empty() && err != "null", "error 应为具体错误说明\n{}", r.report());
    // 错误信息要点名冲突的两个参数。
    assert!(err.contains("--json"), "error 应点名 --json：{err}\n{}", r.report());
    assert!(err.contains("--format"), "error 应点名 --format：{err}\n{}", r.report());
}

// --------------------------------------- 9. listen ndjson + --save 的 out 字段（补强）

/// `listen --port sim --timeout 300 --format ndjson --save <文件> --json`：
/// NDJSON 的最后一行是带 `done` 的汇总行，给了 `--save` 时它还要带 `out`，
/// 并且保存文件**真的被创建**。
///
/// 断言意图：`--save` 的路径在两种输出形状下都要给出去 ——
/// 流式消费方读到最后一行就能知道去哪儿取文件，不必再猜。
#[test]
fn listen_ndjson_with_save_reports_out_path_and_creates_file() {
    let save = temp_path("listen-ndjson-save");
    let _ = std::fs::remove_file(&save);
    let save_str = save.to_str().expect("临时路径应能转成 UTF-8").to_string();

    let r = run_capture(
        &[
            "listen",
            "--port",
            "sim",
            "--timeout",
            "300",
            "--format",
            "ndjson",
            "--save",
            &save_str,
            "--json",
        ],
        20_000,
    );

    // sim 下没有数据源：收不满是常态，但两种退出码都属于「跑完了这一轮」。
    assert!(
        r.code == 0 || r.code == 4,
        "listen ndjson 的退出码应是 0（收满）或 4（超时），实际 {}\n{}",
        r.code,
        r.report()
    );
    let last = r.stdout.last().expect("listen ndjson 应至少输出一行");
    assert!(
        looks_like_json_object(last),
        "最后一行应是合法 JSON 对象：{last}\n{}",
        r.report()
    );
    // 汇总行带 done，也带 out。
    assert_eq!(field(last, "done"), "true", "最后一行应带 done\n{}", r.report());
    let out = top_level_members(last)
        .into_iter()
        .find(|(k, _)| k == "out")
        .map(|(_, v)| v)
        .expect("给了 --save 时汇总行应有 out 字段");
    assert_eq!(
        unescape(&out[1..out.len() - 1]),
        save_str,
        "out 应回显 --save 给的路径\n{}",
        r.report()
    );
    // 文件必须被创建出来。
    assert!(save.exists(), "listen --save 应创建文件 {}\n{}", save_str, r.report());

    let _ = std::fs::remove_file(&save);
}
