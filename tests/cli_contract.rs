//! stui 非交互 CLI 的契约集成测试。
//!
//! 这里验证的是 `stui list | send | listen | sim` 这套面向脚本 / AI 调用的
//! **命令行契约**：子命令分派、退出码语义、单行 JSON 输出结构与字段取值。
//!
//! 重要约束：
//! - 测试**只依赖内置虚拟设备 `--port sim`**（进程内回环），不打开任何真实
//!   COM / tty 串口，因此在没有串口硬件的机器上也能稳定运行。
//! - `list` 只校验 JSON 结构（`ok` / `ports`），**不假设列表里一定有内容**，
//!   避免不同机器端口枚举结果不同导致的假失败。
//! - 项目没有 serde_json，这里手写了一个极简 JSON 取值器；测试断言刻意避免
//!   让字符串**值**内部出现与被查字段同名的文本，以免取值器误判。

use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- 极简 JSON 取值

/// 从 `json` 中取出顶层键 `key` 对应的值文本，返回内部原文（字符串不含两侧引号）。
///
/// 实现方式是从头扫描、识别「字符串 token + 冒号」的组合，因此不会把字符串
/// **值**内部出现的 `"key":` 文本误当成键（`scan_string` 会整体跳过字符串）。
/// 只处理顶层平坦对象——CLI 输出正是这种形状，嵌套对象的同名键按出现顺序取先到者。
///
/// 返回值不做反转义，需要可读文本时再调用 [`unescape`]。
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

// ------------------------------------------------------------------ 进程调用封装

/// 一次子进程调用的结果，便于断言时把退出码 / stdout / stderr 一起打印出来。
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// 失败信息：退出码 + 双管道输出。
    fn report(&self) -> String {
        format!("exit={}\n--- stdout ---\n{}\n--- stderr ---\n{}", self.code, self.stdout, self.stderr)
    }
}

/// 运行 `stui <args...>` 并收齐输出。
fn run(args: &[&str]) -> Run {
    let out: Output = Command::new(env!("CARGO_BIN_EXE_stui"))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("无法启动 stui（{args:?}）：{e}"));
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// 断言退出码，失败时带上完整现场。
fn assert_code(r: &Run, want: i32) {
    assert_eq!(r.code, want, "退出码不符，期望 {want}\n{}", r.report());
}

/// 取字段并在缺失时给出可读报错。
fn f<'a>(r: &'a Run, key: &str) -> &'a str {
    let v = field(&r.stdout, key);
    assert!(!v.is_empty(), "stdout JSON 中缺少字段 \"{key}\"\n{}", r.report());
    v
}

// ------------------------------------------------------------ 取值器自身的自检

/// 这些用例只测上面手写的取值器本身：取值器有 bug 时，不能把它误判成 CLI 违约。
mod json_helper {
    use super::{field, unescape};

    /// 必须跳过前面无关的字段，且能正确解析数字与字符串两类取值。
    #[test]
    fn picks_fields_regardless_of_preceding_keys() {
        let j = r#"{"ok":true,"port":"sim","sent":4,"received":4,"rx_hex":"4142"}"#;
        assert_eq!(field(j, "ok"), "true");
        assert_eq!(field(j, "port"), "sim");
        assert_eq!(field(j, "sent"), "4");
        assert_eq!(field(j, "rx_hex"), "4142");
        assert_eq!(field(j, "missing"), "");
    }

    /// 值内部的同名文本 / 引号 / 逗号都不能干扰取值。
    #[test]
    fn ignores_same_key_text_inside_values() {
        let j = r#"{"error":"bad, stuff \"ok\": not a key","ok":false}"#;
        assert_eq!(field(j, "ok"), "false");
        assert_eq!(unescape(field(j, "error")), "bad, stuff \"ok\": not a key");
        assert_eq!(field(r#"{"error":null}"#, "error"), "null");
    }

    /// 反转义：JSON 里的 `\r\n` 是 4 个字符，解码后应还原成 2 个字节。
    #[test]
    fn unescapes_control_sequences() {
        assert_eq!(unescape(r"hi\r\n"), "hi\r\n");
        assert_eq!(unescape(r"A\tB"), "A\tB");
        assert_eq!(unescape(r"C"), "C");
    }
}

// ------------------------------------------------------------------- 1. 帮助输出

#[test]
fn help_lists_cli_subcommands() {
    // `--help` 必须列出全部 CLI 子命令与 JSON 开关，且退出码为 0。
    let r = run(&["--help"]);
    assert_code(&r, 0);
    for token in ["list", "send", "listen", "sim", "--json"] {
        assert!(r.stdout.contains(token), "--help 输出缺少 {token}\n{}", r.report());
    }
}

// ---------------------------------------------------------------------- 2. list

#[test]
fn list_json_is_single_line_object() {
    // `list --json` 输出必须是**单行** JSON，且带 ok/ports 字段；不假设列表非空。
    let r = run(&["list", "--json"]);
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "list 的 ok 应为 true\n{}", r.report());
    assert!(r.stdout.contains("\"ports\":["), "list 缺少 ports 数组\n{}", r.report());
    let one_line = r.stdout.trim();
    assert!(!one_line.contains('\n'), "list 的 JSON 应为单行\n{}", r.report());
    assert!(one_line.starts_with('{') && one_line.ends_with('}'), "list 输出应是一个 JSON 对象\n{}", r.report());
}

// ---------------------------------------------------------------------- 3. send 文本

#[test]
fn send_text_roundtrip_via_sim() {
    // 虚拟设备回环：`hi\r\n` 共 4 字节，发出 4 收到 4，rx_text 解码后逐字节一致。
    let r = run(&["send", "--port", "sim", "--text", "hi\r\n", "--json"]);
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "send 的 ok 应为 true\n{}", r.report());
    assert_eq!(f(&r, "port"), "sim", "port 应回显为 sim\n{}", r.report());
    assert_eq!(f(&r, "sent"), "4", "\"hi\\r\\n\" 应为 4 字节\n{}", r.report());
    assert_eq!(f(&r, "received"), "4", "回环应收到 4 字节\n{}", r.report());
    let rx_text = unescape(f(&r, "rx_text"));
    assert_eq!(rx_text, "hi\r\n", "rx_text 解码后应等于 \"hi\\r\\n\"\n{}", r.report());
    // expect_ok 是固定 schema 里的一列：即使这一轮没用 --expect，它也必须在，
    // 且恒为 true（没有期望可比 = 没有期望不符）。脚本按固定字段取值。
    assert_eq!(f(&r, "expect_ok"), "true", "没用 --expect 时 expect_ok 应为 true\n{}", r.report());
}

// ---------------------------------------------------------------------- 4. send HEX

#[test]
fn send_hex_roundtrip_via_sim() {
    // `--hex "41 42"` → 2 字节，回环 rx_hex 应为大写连续十六进制 4142。
    let r = run(&["send", "--port", "sim", "--hex", "41 42", "--json"]);
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "send 的 ok 应为 true\n{}", r.report());
    assert_eq!(f(&r, "sent"), "2", "\"41 42\" 应为 2 字节\n{}", r.report());
    assert_eq!(f(&r, "received"), "2", "回环应收到 2 字节\n{}", r.report());
    assert_eq!(f(&r, "rx_hex"), "4142", "rx_hex 应为 4142\n{}", r.report());
    // 期望字段恒定出现：脚本按固定 schema 取值，不用先判断这轮有没有用 --expect。
    assert_eq!(f(&r, "expect_ok"), "true", "没用 --expect 时 expect_ok 应为 true\n{}", r.report());
}

// ------------------------------------------------------------------ 5. 奇数位十六进制

#[test]
fn send_rejects_odd_length_hex() {
    // "ABC" 是奇数位：属于参数错误（退出码 2），且必须回 ok:false + error。
    let r = run(&["send", "--port", "sim", "--text", "hi", "--hex", "ABC", "--json"]);
    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "参数错误时 ok 应为 false\n{}", r.report());
    let err = f(&r, "error");
    assert!(err != "null" && !err.is_empty(), "error 应为具体错误说明\n{}", r.report());
}

// ------------------------------------------------------------------- 6. 缺少数据来源

#[test]
fn send_requires_a_payload_source() {
    // 既无 --text 也无 --hex/--file：用法错误（退出码 2）。
    let r = run(&["send", "--port", "sim", "--json"]);
    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "缺少数据来源时 ok 应为 false\n{}", r.report());
    let err = f(&r, "error");
    assert!(err != "null" && !err.is_empty(), "error 应为具体错误说明\n{}", r.report());
}

// ------------------------------------------------------------------- 7. 端口不存在

#[test]
fn send_reports_unavailable_port() {
    // 打开失败属于运行期错误（退出码 3），与用法错误 2 区分开。
    let r = run(&["send", "--port", "COM_NO_SUCH_PORT_9999", "--text", "hi", "--json"]);
    assert_code(&r, 3);
    assert_eq!(f(&r, "ok"), "false", "打开端口失败时 ok 应为 false\n{}", r.report());
    let err = f(&r, "error");
    assert!(err != "null" && !err.is_empty(), "error 应为具体错误说明\n{}", r.report());
}

// --------------------------------------------------------------------- 8. listen

#[test]
fn listen_timeout_on_silent_sim() {
    // sim 是纯回环、没有外部数据源，300ms 内收不到东西 → 超时（退出码 4）。
    let r = run(&["listen", "--port", "sim", "--timeout", "300", "--json"]);
    assert_code(&r, 4);
    assert_eq!(f(&r, "ok"), "false", "超时时 ok 应为 false\n{}", r.report());
    assert_eq!(f(&r, "error"), "timeout", "error 应为 \"timeout\"\n{}", r.report());
    assert_eq!(f(&r, "received"), "0", "超时时 received 应为 0\n{}", r.report());
}

// ------------------------------------------------------------------ 9. 未知子命令

#[test]
fn unknown_subcommand_fails_with_usage() {
    // `bogus` 既不是 CLI 子命令也不是合法 TUI 参数 → 退出码 2。
    let r = run(&["bogus", "--json"]);
    assert_code(&r, 2);
}

// ------------------------------------------------------------------ 10. repeat

#[test]
fn send_repeat_multiplies_payload() {
    // 3 次 × 2 字节 = 6，且要求全部回环收到。
    let r = run(&["send", "--port", "sim", "--text", "hi", "--repeat", "3", "--interval", "10", "--json"]);
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "send 的 ok 应为 true\n{}", r.report());
    assert_eq!(f(&r, "sent"), "6", "3 × 2 字节应为 6\n{}", r.report());
    assert_eq!(f(&r, "received"), "6", "回环应收到 6 字节\n{}", r.report());
}

// ---------------------------------------------------------------------- 11. sim

#[test]
fn sim_reports_ready_on_stderr() {
    // 就绪标记走 stderr（stdout 留给 JSON 结果），两者不能混在一起。
    let r = run(&["sim", "--duration", "300", "--json"]);
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "sim 的 ok 应为 true\n{}", r.report());
    assert_eq!(f(&r, "mode"), "sim", "mode 应为 sim\n{}", r.report());
    assert!(r.stderr.contains("stui-sim ready"), "stderr 应包含就绪标记 stui-sim ready\n{}", r.report());
}

// ------------------------------------------------------ 12. 回环端到端（spawn）

#[test]
fn sim_loopback_delivers_to_concurrent_client() {
    // 先起一个后台 sim，等它就绪，再用独立的 send 进程发数据，验证跨进程回环。
    let mut child = Command::new(env!("CARGO_BIN_EXE_stui"))
        .args(["sim", "--duration", "2500", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("启动 sim 子进程失败：{e}"));

    // stderr 交给独立线程按行读，主线程用 recv_timeout 轮询，避免阻塞式读卡死。
    let stderr_pipe = child.stderr.take().expect("sim 子进程缺少 stderr 管道");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = String::new();
    let mut ready = false;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(Duration::from_millis(50))) {
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
                if line.contains("stui-sim ready") {
                    ready = true;
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let r = run(&["send", "--port", "sim", "--text", "ping", "--json"]);

    // 先回收子进程，再断言，确保失败路径也不会留下游离进程。
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    assert!(ready, "5 秒内未在 sim 的 stderr 看到 stui-sim ready，已读到：\n{seen}");
    assert_code(&r, 0);
    assert_eq!(f(&r, "ok"), "true", "send 的 ok 应为 true\n{}", r.report());
    assert_eq!(f(&r, "sent"), "4", "\"ping\" 应为 4 字节\n{}", r.report());
    assert_eq!(f(&r, "received"), "4", "跨进程回环应收到 4 字节\n{}", r.report());
}

// ------------------------------------------------------ 13. ports 别名的输出形状

#[test]
fn ports_alias_json_is_single_line() {
    // `ports` 是 `list` 的别名，JSON 输出形状必须完全一致：单行对象 + ok/ports。
    let r = run(&["ports", "--json"]);
    assert_code(&r, 0);
    let one_line = r.stdout.trim();
    assert!(!one_line.contains('\n'), "ports 的 JSON 应为单行\n{}", r.report());
    assert!(
        one_line.starts_with('{') && one_line.ends_with('}'),
        "ports 输出应是一个 JSON 对象\n{}",
        r.report()
    );
    assert_eq!(f(&r, "ok"), "true", "ports 的 ok 应为 true\n{}", r.report());
    assert!(r.stdout.contains("\"ports\":["), "ports 缺少 ports 数组\n{}", r.report());
}

// ------------------------------------------------------- 14. send 的 rx_hex 格式

#[test]
fn send_rx_hex_is_uppercase_contiguous_hex() {
    // JSON 里的 rx_hex 是脚本直接拿去比对的字节串：大写、连续、不带空格。
    // （面向人眼的 `fmt::hex_bytes` 才带空格，JSON 里必须是紧凑形式。）
    let r = run(&["send", "--port", "sim", "--hex", "00 ff 0a", "--json"]);
    assert_code(&r, 0);
    let rx_hex = f(&r, "rx_hex");
    assert_eq!(rx_hex, "00FF0A", "rx_hex 应为大写连续十六进制\n{}", r.report());
    assert!(
        rx_hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
        "rx_hex 只应含大写十六进制字符：{rx_hex}\n{}",
        r.report()
    );
    assert_eq!(rx_hex.len() % 2, 0, "rx_hex 长度应为偶数（每字节两位）\n{}", r.report());
    assert!(!rx_hex.contains(' '), "rx_hex 不应含空格\n{}", r.report());
}

// --------------------------------------------- 15. 参数取值与边界（复核发现的缺陷）

/// `--baud` 越界必须是用法错误。
///
/// 早先按 u64 解析再 `as u32`，`4294967297` 会被悄悄截断成 1、
/// `4294967296` 变成 0，退出码还是 0。
#[test]
fn send_rejects_out_of_range_baud() {
    for baud in ["4294967296", "4294967297"] {
        let r = run(&["send", "--port", "sim", "--baud", baud, "--text", "hi", "--json"]);
        assert_code(&r, 2);
        assert_eq!(f(&r, "ok"), "false", "波特率越界应是用法错误\n{}", r.report());
        let err = unescape(&f(&r, "error"));
        assert!(err.contains("baud"), "错误信息要指明是 --baud：{err}\n{}", r.report());
    }
}

/// `--repeat 0` 等于一次都没发：不能报成功。
///
/// 空载荷已经是用法错误（退出码 2），`--repeat 0` 口径要一致。
#[test]
fn send_rejects_zero_repeat() {
    let r = run(&["send", "--port", "sim", "--text", "AB", "--repeat", "0", "--json"]);
    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "repeat 0 应是用法错误\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(err.contains("repeat"), "错误信息要指明是 --repeat：{err}\n{}", r.report());
}

/// 漏写取值时不能把下一个选项名当成内容发出去。
///
/// `send --port sim --text --json` 早先会把 "--json" 当正文发给设备。
#[test]
fn send_rejects_option_name_as_value() {
    let r = run(&["send", "--port", "sim", "--text", "--json", "--wait", "100"]);
    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "取值不能是另一个选项\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(err.contains("--text"), "错误信息要指明是哪个参数缺取值：{err}\n{}", r.report());
}

/// 错误输出必须是**合法** JSON：控制字符要转义，不能原样写进字符串。
#[test]
fn error_output_escapes_control_characters() {
    let r = run(&["a\u{0c}b", "--json"]);
    assert_code(&r, 2);
    let out = r.stdout.trim();
    assert_eq!(out.lines().count(), 1, "错误输出应当是单行 JSON\n{}", r.report());
    assert!(
        !out.chars().any(|c| (c as u32) < 0x20),
        "输出里不能有裸控制字符：{out:?}\n{}",
        r.report()
    );
    assert!(out.contains("\\u000C"), "控制字符应当写成 \\u00XX：{out:?}\n{}", r.report());
}

// --------------------------------------------- 16. --expect 的匹配语义（补强）

/// `--expect` 是**子串**匹配，而且期望序列可以横跨字节边界。
///
/// 载荷 "hi\r\n" 回环回来是 68 69 0D 0A；期望 "690D"（0x69 0x0D）
/// 落在第 2、3 个字节上，起点不在字节边界上。
/// 断言意图：设备回包里的目标序列不一定对齐发送的字节，
/// 校验必须按字节流找子串，否则会漏判。
#[test]
fn send_expect_matches_substring_that_spans_bytes() {
    let r = run(&["send", "--port", "sim", "--text", "hi\r\n", "--expect", "690D", "--json"]);
    assert_code(&r, 0);
    assert_eq!(f(&r, "expect_ok"), "true", "跨字节的子串也应命中\n{}", r.report());
    // 期望目标原样回显（紧凑大写十六进制）。
    assert_eq!(f(&r, "expect_hex"), "690D", "expect_hex 应为 690D\n{}", r.report());
    // 回包确实是 4 字节，说明命中不是因为「什么都没收到」。
    assert_eq!(f(&r, "received"), "4", "回环应收到 4 字节\n{}", r.report());
}

/// `--expect-timeout` 没有 `--expect` 一起用就是一句没有主语的指令：
/// 属于用法错误（退出码 2）。
///
/// 断言意图：静默忽略一个无效参数，脚本会以为「超时时限生效了」，
/// 实际上根本没人在等 —— 必须当场报出来。
#[test]
fn send_rejects_expect_timeout_without_expect() {
    let r = run(&["send", "--port", "sim", "--text", "hi", "--expect-timeout", "100", "--json"]);
    assert_code(&r, 2);
    assert_eq!(f(&r, "ok"), "false", "用法错误时 ok 应为 false\n{}", r.report());
    let err = unescape(&f(&r, "error"));
    assert!(!err.is_empty() && err != "null", "error 应为具体错误说明\n{}", r.report());
    // 错误信息要指出缺的是哪一个参数。
    assert!(
        err.contains("--expect-timeout"),
        "错误信息要指明是 --expect-timeout：{err}\n{}",
        r.report()
    );
}
