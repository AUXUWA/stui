//! stui 非交互 CLI 的补充契约集成测试。
//!
//! 本文件与 `tests/cli_contract.rs` 互补，验证**新增**的那批面向脚本 / AI 调用的能力：
//! `watch`（NDJSON 流式输出）、`send --dry-run`（只解析不发送）、
//! `selftest`（内置自检）、`ports`（`list` 的别名）、`list --kind`（按类型过滤）。
//!
//! 重要约束：
//! - 测试**只依赖内置虚拟设备 `--port sim`**（进程内回环），不打开任何真实
//!   COM / tty 串口，因此在没有串口硬件的机器上也能稳定运行。
//! - 项目没有 serde_json，这里同样手写一个极简 JSON 取值器（`field` /
//!   `scan_string` / `unescape`），另加两个结构辅助函数
//!   （`top_level_members` / `array_items`）用来比对两个子命令的 JSON 结构。
//! - 读子进程输出时用**独立线程按行读**、主线程用 `recv_timeout` 轮询，
//!   避免阻塞式读在管道没关干净时把测试卡死。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
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
/// 与 `cli_contract.rs` 里的同名函数同一套语义：只认「字符串 token + 冒号」的
/// 组合，不会把字符串**值**内部出现的 `"key":` 文本误当成键。
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
        },
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
///
/// 嵌套对象 / 数组里的同名键不会被收进来：这正是用来比对「两个子命令的 JSON
/// 结构是否一致」所需要的——键集合相同就说明结构相同。
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

/// 粗粒度校验：文本是否像一个**合法**的 JSON 对象。
///
/// 判据：首尾是 `{` / `}`、括号配对、字符串闭合、至少有一处 `"键": 值`。
/// 用来断言「输出是合法 JSON 对象」而不必引入 serde_json。
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

/// 取 stdout 第一行里的字段值，**允许空字符串**（但字段必须存在）。
///
/// `f` 判据是「非空」，所以像 `rx_hex: ""` 这种合法的空值只能用这个取。
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

// ---------------------------------------------------------------- 1. watch（NDJSON）

/// `watch --port sim --timeout 300 --json` 必须正常结束（退出码 0），
/// stdout 是 NDJSON：每行都是一个 JSON 对象，最后一行是带 `done` 的汇总行。
///
/// 关于「行数」的实际行为（跑过一遍确认）：`--port sim` 是**进程内**回环虚拟设备
/// （`src/cli.rs` 的 `SimLink` 用两条进程内 mpsc 通道实现），只有本进程自己写入的
/// 字节才会被回送；而 `watch` 从不写入，所以拿不到任何数据块行，stdout 只有
/// 那一行汇总。因此这里断言「行数 == 1 且该行是汇总行」——这是当前实现的真实
/// 契约；多行（数据块行）只有在接真实串口时才会出现。
#[test]
fn watch_sim_emits_ndjson_summary_and_exits_ok() {
    let r = run_capture(&["watch", "--port", "sim", "--timeout", "300", "--json"], 10_000);

    // 退出码 0：watch 没有「收满」条件，超时只是到点收工，不算错误。
    assert_code(&r, 0);
    // watch 的正常输出全部走 stdout，stderr 不该有内容。
    assert!(r.stderr.is_empty(), "watch 不应往 stderr 写东西\n{}", r.report());

    // stdout 是 NDJSON：至少一行，且每行都是合法 JSON 对象。
    assert!(!r.stdout.is_empty(), "watch 应至少输出汇总行\n{}", r.report());
    for (n, line) in r.stdout.iter().enumerate() {
        assert!(
            looks_like_json_object(line),
            "watch 第 {} 行不是合法 JSON 对象：{line}\n{}",
            n + 1,
            r.report()
        );
    }
    // 除最后一行（汇总行）之外，每一行都应是「来了一块数据」的行：
    // 带 len 与 rx_hex 字段。
    for (n, line) in r.stdout.iter().take(r.stdout.len().saturating_sub(1)).enumerate() {
        assert!(
            !field(line, "len").is_empty() && !field(line, "rx_hex").is_empty(),
            "watch 第 {} 行应是数据块行（带 len / rx_hex）：{line}\n{}",
            n + 1,
            r.report()
        );
    }

    // 最后一行是汇总行：带 done 字段。
    let last = r.stdout.last().expect("stdout 不应为空");
    assert_eq!(field(last, "done"), "true", "最后一行应是带 done 的汇总行\n{}", r.report());
    // 汇总行统计的是「本次 watch 的总量」：没有数据源时为 0。
    assert_eq!(field(last, "total"), "0", "没有数据源时 total 应为 0\n{}", r.report());
    assert_eq!(field(last, "chunks"), "0", "没有数据源时 chunks 应为 0\n{}", r.report());

    // 当前实现下（sim 是进程内回环、watch 不写入）只有汇总行。
    assert_eq!(
        r.stdout.len(),
        1,
        "sim 回环下 watch 只会输出汇总行\n{}",
        r.report()
    );
}

/// `watch` 与 `listen` 的超时语义不同：watch 到点即正常结束（0），
/// listen 收不满就是超时（4）。这个区别必须稳定，否则脚本没法区分
/// 「正常收工」和「没收到东西」。
#[test]
fn watch_and_listen_differ_on_timeout_exit_code() {
    let w = run_capture(&["watch", "--port", "sim", "--timeout", "300", "--json"], 10_000);
    let l = run_capture(&["listen", "--port", "sim", "--timeout", "300", "--json"], 10_000);

    assert_code(&w, 0);
    assert_code(&l, 4);
    // listen 收不满时报 ok:false + error:"timeout"；watch 始终 ok:true。
    assert_eq!(f(&l, "ok"), "false", "listen 超时应 ok:false\n{}", l.report());
    assert_eq!(f(&l, "error"), "timeout", "listen 超时 error 应为 timeout\n{}", l.report());
    assert_eq!(f(&w, "ok"), "true", "watch 正常收工应 ok:true\n{}", w.report());
}

// ------------------------------------------------------------ 2. send --dry-run

/// `send --dry-run` 只解析不发送：ok=true、planned 给出将要发送的字节数，
/// 而 sent 恒为 0（且 received 也是 0，因为压根没打开串口）。
#[test]
fn send_dry_run_reports_planned_payload_without_sending() {
    let r = run_capture(&["send", "--port", "sim", "--text", "hi", "--dry-run", "--json"], 10_000);

    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "dry-run 应 ok:true\n{}", r.report());
    // "hi" 是 2 个字节。
    assert_eq!(f(&r, "planned"), "2", "\"hi\" 应规划为 2 字节\n{}", r.report());
    // 没有真的发送：sent / received 都是 0，rx_hex 为空。
    assert_eq!(f(&r, "sent"), "0", "dry-run 不应真的发送\n{}", r.report());
    assert_eq!(f(&r, "received"), "0", "dry-run 不应收到任何东西\n{}", r.report());
    assert_eq!(fv(&r, "rx_hex"), "", "dry-run 的 rx_hex 应为空\n{}", r.report());
    // planned_hex 描述将要发出的内容，便于脚本预览。
    assert_eq!(f(&r, "planned_hex"), "6869", "planned_hex 应为 6869\n{}", r.report());
    // 期望字段恒定出现：dry-run 没真的发，也就谈不上「期望不符」。
    assert_eq!(f(&r, "expect_ok"), "true", "dry-run 时 expect_ok 应为 true\n{}", r.report());
}

/// `--dry-run` 连端口都不打开：给一个根本不存在的端口名也必须成功。
///
/// 这条断言是「没有真的发送」最强的证据——如果实现偷偷去 open 端口，
/// 这里会变成退出码 3（打开失败）。
#[test]
fn send_dry_run_never_opens_the_port() {
    let r = run_capture(
        &[
            "send",
            "--port",
            "COM_NO_SUCH_PORT_9999",
            "--text",
            "hi",
            "--dry-run",
            "--json",
        ],
        10_000,
    );
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "dry-run 应 ok:true\n{}", r.report());
    assert_eq!(f(&r, "planned"), "2", "\"hi\" 应规划为 2 字节\n{}", r.report());
    assert_eq!(f(&r, "sent"), "0", "dry-run 不应真的发送\n{}", r.report());
}

// ------------------------------------------------------------------ 3. selftest

/// `selftest --json`：退出码 0、ok=true、checks 数组非空，
/// 且每一项都带 name / ok / detail。
#[test]
fn selftest_json_reports_non_empty_checks() {
    let r = run_capture(&["selftest", "--json"], 20_000);

    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "selftest 应 ok:true\n{}", r.report());

    let line = r.stdout.first().expect("selftest 应输出一行");
    assert!(
        looks_like_json_object(line),
        "selftest 输出应是合法 JSON 对象\n{}",
        r.report()
    );
    let checks = array_items(line, "checks").expect("selftest 输出应含 checks 数组");
    assert!(!checks.is_empty(), "checks 数组不应为空\n{}", r.report());
    for c in &checks {
        assert!(
            looks_like_json_object(c),
            "checks 元素应是 JSON 对象：{c}\n{}",
            r.report()
        );
        assert!(!field(c, "name").is_empty(), "检查项应有 name：{c}\n{}", r.report());
        // selftest 全部通过时退出码才是 0，所以每项都应为 true。
        assert_eq!(field(c, "ok"), "true", "检查项应通过：{c}\n{}", r.report());
        assert!(!field(c, "detail").is_empty(), "检查项应有 detail：{c}\n{}", r.report());
    }
}

// --------------------------------------------------------------- 4. list --kind

/// `list --kind usb --json`：退出码 0，输出是合法 JSON 对象且含 ports 数组；
/// 数组里每一项的 kind 都必须是 usb（过滤真的生效了）。
///
/// 不假设列表一定非空：不同机器枚举到的串口不一样，空数组同样合法。
#[test]
fn list_kind_usb_filters_ports_to_usb() {
    let r = run_capture(&["list", "--kind", "usb", "--json"], 10_000);

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
        assert_eq!(
            field(p, "kind"),
            "usb",
            "--kind usb 下每一项都应是 usb：{p}\n{}",
            r.report()
        );
    }
    // count 必须和数组长度一致，否则脚本按 count 取数会取错。
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

/// 非法 `--kind` 是用法错误（退出码 2），ok=false + error 非空。
#[test]
fn list_rejects_unknown_kind() {
    let r = run_capture(&["list", "--kind", "nope", "--json"], 10_000);
    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "非法 kind 应 ok:false\n{}", r.report());
    let err = f(&r, "error");
    assert!(err != "null" && !err.is_empty(), "error 应为具体错误说明\n{}", r.report());
}

// -------------------------------------------------------------- 5. ports 别名

/// `ports` 是 `list` 的别名：两者退出码相同、JSON 的**顶层键集合**相同，
/// 并且都含 ok 与 ports。
#[test]
fn ports_alias_has_the_same_json_structure_as_list() {
    let l = run_capture(&["list", "--json"], 10_000);
    let p = run_capture(&["ports", "--json"], 10_000);

    assert_code(&l, 0);
    assert_code(&p, 0);

    let lline = l.stdout.first().expect("list 应输出一行");
    let pline = p.stdout.first().expect("ports 应输出一行");
    assert!(
        looks_like_json_object(pline),
        "ports 输出应是合法 JSON 对象\n{}",
        p.report()
    );

    let lkeys: Vec<String> = top_level_members(lline).into_iter().map(|(k, _)| k).collect();
    let pkeys: Vec<String> = top_level_members(pline).into_iter().map(|(k, _)| k).collect();
    assert_eq!(lkeys, pkeys, "ports 与 list 的顶层键应完全一致\n{}", p.report());

    // 两个子命令都必须给出 ok 与 ports 这两个调用方赖以取值的字段。
    for key in ["ok", "ports"] {
        assert!(lkeys.iter().any(|k| k == key), "list 应含 {key} 字段\n{}", l.report());
        assert!(pkeys.iter().any(|k| k == key), "ports 应含 {key} 字段\n{}", p.report());
    }
    assert_eq!(field(pline, "ok"), "true", "ports 的 ok 应为 true\n{}", p.report());
    assert!(array_items(pline, "ports").is_some(), "ports 的 ports 应是数组\n{}", p.report());
}

// ------------------------------------------------------------------- 6. --help

/// `--help` 必须把新增的子命令与参数都写出来，脚本 / AI 才能照着调用。
#[test]
fn help_lists_watch_selftest_ports_and_new_flags() {
    let r = run_capture(&["--help"], 10_000);
    assert_code(&r, 0);
    let text = r.stdout_text();
    for token in ["watch", "selftest", "ports", "--dry-run", "--kind"] {
        assert!(text.contains(token), "--help 输出缺少 {token}\n{}", r.report());
    }
}

// -------------------------------------------------------- 7. 奇数位十六进制

/// 奇数位十六进制必须被拒绝：退出码 2、ok=false、error 非空，
/// 而且错误信息要**指明出错的内容**（"ABC"）和原因（位数是奇数）。
#[test]
fn send_rejects_odd_length_hex_with_precise_error() {
    let r = run_capture(&["send", "--port", "sim", "--hex", "ABC", "--json"], 10_000);

    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "参数错误时 ok 应为 false\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(!err.is_empty() && err != "null", "error 应为具体错误说明\n{}", r.report());
    // 错误信息要带上出错的标记本身。
    assert!(err.contains("ABC"), "error 应指出出错的标记 ABC：{err}\n{}", r.report());
    // 还要说清原因：位数是奇数。
    assert!(err.contains("奇数"), "error 应说明位数是奇数：{err}\n{}", r.report());
}

// ------------------------------------------------- 8. selftest 的文本输出（补强）

/// `selftest`（不给 --json）：退出码 0，stdout 是给人看的逐行结论，
/// 并且**不含** JSON 对象。
///
/// 断言意图：文本模式与 JSON 模式是两套输出，不能互相污染 ——
/// 人看的那份里混进 JSON 行就没法读了。
#[test]
fn selftest_without_json_prints_human_readable_lines() {
    let r = run_capture(&["selftest"], 20_000);

    assert_code(&r, 0);
    let text = r.stdout_text();
    assert!(!text.trim().is_empty(), "selftest 文本模式应输出内容\n{}", r.report());
    // 三个检查项的名字都要出现：少一项就意味着有一项没跑。
    for name in ["sim-open", "echo-match", "no-crosstalk"] {
        assert!(text.contains(name), "selftest 输出应提到 {name}\n{}", r.report());
    }
    // 全通过时要有明确的结论行。
    assert!(text.contains("自检通过"), "selftest 应给出总体结论\n{}", r.report());
    // 文本模式里不该出现 JSON 对象。
    assert!(
        !text.contains('{'),
        "文本模式下不该出现 JSON：{text}\n{}",
        r.report()
    );
}

// ------------------------------------------------- 9. list --verbose 的两列（补强）

/// `list --verbose`（文本模式）：每个端口行都要多出「厂商」与「产品」两列。
///
/// 断言意图：`--verbose` 只影响人看的输出（JSON 里本来就给全了这两列），
/// 所以不假设机器上一定有串口：没端口时打印「（未发现串口）」同样合法；
/// 有端口时每一行都必须带这两列。
#[test]
fn list_verbose_prints_manufacturer_and_product_columns() {
    let r = run_capture(&["list", "--verbose"], 20_000);

    assert_code(&r, 0);
    let text = r.stdout_text();
    if text.trim().is_empty() || text.contains("（未发现串口）") || text.contains("（没有匹配的串口）") {
        // 这台机器上一个串口都没有：没有行可校验，断言到此为止。
        return;
    }
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.contains("厂商:") && line.contains("产品:"),
            "--verbose 的每一行都要有厂商与产品两列：{line}\n{}",
            r.report()
        );
    }
}
