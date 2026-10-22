//! 配置持久化：把上次用过的串口参数落盘，下次启动自动恢复。
//!
//! 手写 JSON，不引入 serde：本项目的配置结构是扁平的（只有标量值），
//! 一个够用的小解析器就够，不必为此多背一个依赖。
//!
//! 设计原则：**配置文件坏掉不能让程序起不来**。
//! 读不出来、解析失败、类型不对，一律静默退回默认值，
//! 宁可丢一次偏好，也不能让用户被挡在门外。

// 配置项暂时只有本模块使用，等界面层接上后这里会去掉。
#![allow(dead_code)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 配置文件格式的版本号。
///
/// 每次往文件里加字段、或者改变已有字段的含义，都应该把版本号 +1，
/// 这样以后要改格式时能一眼看出「这份文件是哪一版写的」。
///
/// **版本号只描述文件格式，不描述程序行为**：
/// 它记录的是「这份文件里的键名和含义是哪一版的」，
/// 而不是「这份配置来自哪个 stui 版本」。
pub const CONFIG_VERSION: u32 = 1;

/// 用户可持久化的配置项。
///
/// 除 `auto_reconnect` / `show_ms` / `wrap` / `auto_ports` 外都是可选项：
/// `None` 表示“用户没设置过”，调用方应回退到自己的默认值，而不是当成 0。
/// 这四个是开关，没有“未设置”这一说，缺失时就是 false。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub port: Option<String>,
    pub baud: Option<u32>,
    pub bits: Option<u8>,
    pub parity: Option<String>,
    pub stops: Option<u8>,
    pub flow: Option<String>,
    pub view: Option<String>,
    pub interval_ms: Option<u64>,
    pub fps: Option<u32>,
    pub auto_reconnect: bool,
    pub show_ms: bool,
    /// 接收区是否自动换行（超出宽度折行，而不是横向截断）。
    pub wrap: bool,
    /// 启动时是否自动枚举串口并选中第一个可用的。
    pub auto_ports: bool,
}

impl Config {
    /// 读取配置文件，并报告这份配置是不是真的从磁盘上读来的。
    ///
    /// 第二个返回值 `from_file` 只有在「文件存在**且**解析成功」时才为真：
    /// 坏掉的配置会退回默认值，若把它当成「有配置」，界面上的两个开关
    /// 就会被默认值覆盖（时间戳精度从毫秒降到秒，正是要避免的倒退）。
    pub fn load() -> (Config, bool) {
        load_from(&Config::path())
    }

    /// 保存配置。
    ///
    /// 先写同目录下的临时文件再 rename，保证替换是原子的：
    /// 写到一半断电不会把旧配置变成半截 JSON。
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&Config::path())
    }

    /// 配置文件位置。
    ///
    /// Windows 用 `%APPDATA%/stui/config.json`；
    /// 其它平台用 `$XDG_CONFIG_HOME/stui/config.json`，
    /// 没有 `XDG_CONFIG_HOME` 时退回 `~/.config/stui/config.json`。
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    /// 保存到指定路径（测试与内部使用）。
    fn save_to(&self, path: &Path) -> io::Result<()> {
        // 父目录可能还不存在（首次运行时就是）。
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let body = self.to_json_line();
        let tmp = tmp_path(path);
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.flush()?;
            // 先落盘再改名：掉电时旧文件仍然是完整的。
            let _ = f.sync_all();
        }
        // 临时文件与目标在同一目录，因此 rename 在同一文件系统内完成，是原子的。
        // Windows 上 std::fs::rename 会替换已存在的文件。
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // 改名失败时不要留下半截临时文件。
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// 序列化成单行 JSON；键名与字段名一致，缺失的可选项写 null。
    ///
    /// 第一个键固定是 `version`：它是整份文件的格式说明，
    /// 放在最前面是为了让人（和以后自己的解析器）一打开就看到。
    fn to_json_line(&self) -> String {
        let mut s = String::with_capacity(192);
        s.push('{');
        s.push_str("\"version\":");
        s.push_str(&CONFIG_VERSION.to_string());
        s.push(',');
        s.push_str("\"port\":");
        push_opt_str(&mut s, self.port.as_deref());
        s.push_str(",\"baud\":");
        push_opt_num(&mut s, self.baud.map(u64::from));
        s.push_str(",\"bits\":");
        push_opt_num(&mut s, self.bits.map(u64::from));
        s.push_str(",\"parity\":");
        push_opt_str(&mut s, self.parity.as_deref());
        s.push_str(",\"stops\":");
        push_opt_num(&mut s, self.stops.map(u64::from));
        s.push_str(",\"flow\":");
        push_opt_str(&mut s, self.flow.as_deref());
        s.push_str(",\"view\":");
        push_opt_str(&mut s, self.view.as_deref());
        s.push_str(",\"interval_ms\":");
        push_opt_num(&mut s, self.interval_ms);
        s.push_str(",\"fps\":");
        push_opt_num(&mut s, self.fps.map(u64::from));
        s.push_str(",\"auto_reconnect\":");
        s.push_str(if self.auto_reconnect { "true" } else { "false" });
        s.push_str(",\"show_ms\":");
        s.push_str(if self.show_ms { "true" } else { "false" });
        s.push_str(",\"wrap\":");
        s.push_str(if self.wrap { "true" } else { "false" });
        s.push_str(",\"auto_ports\":");
        s.push_str(if self.auto_ports { "true" } else { "false" });
        s.push('}');
        s
    }
}

// ------------------------------------------------------------------
// 路径
// ------------------------------------------------------------------

#[cfg(windows)]
fn config_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        if !appdata.is_empty() {
            return PathBuf::from(appdata).join("stui");
        }
    }
    // APPDATA 缺失（非交互式环境里并不罕见）时退回用户目录。
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("AppData").join("Roaming").join("stui");
    }
    PathBuf::from(".").join(".stui")
}

#[cfg(not(windows))]
fn config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("stui");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("stui");
    }
    PathBuf::from(".").join(".stui")
}

// ------------------------------------------------------------------
// 读
// ------------------------------------------------------------------

/// 从指定路径读取配置；读不出来就返回默认值。
///
/// 第二个返回值表示「这份配置确实来自一个能解析的文件」。
fn load_from(path: &Path) -> (Config, bool) {
    let (cfg, from_file, _version) = read_config(path);
    (cfg, from_file)
}

/// 读出配置文件的版本号。
///
/// 用途：让调用方能判断这份配置是不是来自更新的 stui。
/// 读不出来（文件不存在 / 坏掉）时返回当前版本 ——
/// 拿不准时按「当前版本」处理，因为当前版本读旧文件一定是安全的。
pub fn config_version_at(path: &Path) -> u32 {
    read_config(path).2
}

/// 读配置；额外把文件里的版本号带出来。
///
/// 版本号单独用一个返回值，而不是塞进 `Config`：
/// 它描述的是「文件格式」，不是「用户的设置」，
/// 混进配置结构里只会让人误以为它也是一个可选项。
fn read_config(path: &Path) -> (Config, bool, u32) {
    let mut cfg = Config::default();
    // 先看文件大小：超长的配置连读进内存都不应该，
    // 读进来再判断就晚了，内存已经被吃掉了。
    // 直接当损坏处理，退回默认值。
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_INPUT_LEN as u64 {
            return (Config::default(), false, CONFIG_VERSION);
        }
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        // 文件不存在、无权限、是目录……一律当作“没有配置”。
        Err(_) => return (cfg, false, CONFIG_VERSION),
    };
    let root = match parse_json(&text) {
        Ok(v) => v,
        // 坏掉的 JSON：整份退回默认值。
        Err(_) => return (Config::default(), false, CONFIG_VERSION),
    };
    let Json::Obj(entries) = root else {
        return (Config::default(), false, CONFIG_VERSION);
    };
    // 没写 version 的老配置按版本 1 处理 —— 1 就是「第一个版本」，
    // 等价于「这个字段是后来才加的，之前的人根本不知道它」。
    let mut version = CONFIG_VERSION;
    for (k, v) in entries {
        match (k.as_str(), v) {
            ("version", Json::Num(n)) => version = normalize_version(n),
            ("port", Json::Str(s)) => cfg.port = Some(s),
            ("baud", Json::Num(n)) => cfg.baud = as_u32(n),
            ("bits", Json::Num(n)) => cfg.bits = as_u8(n),
            ("parity", Json::Str(s)) => cfg.parity = Some(s),
            ("stops", Json::Num(n)) => cfg.stops = as_u8(n),
            ("flow", Json::Str(s)) => cfg.flow = Some(s),
            ("view", Json::Str(s)) => cfg.view = Some(s),
            ("interval_ms", Json::Num(n)) => cfg.interval_ms = as_u64(n),
            ("fps", Json::Num(n)) => cfg.fps = as_u32(n),
            ("auto_reconnect", Json::Bool(b)) => cfg.auto_reconnect = b,
            ("show_ms", Json::Bool(b)) => cfg.show_ms = b,
            ("wrap", Json::Bool(b)) => cfg.wrap = b,
            ("auto_ports", Json::Bool(b)) => cfg.auto_ports = b,
            // 未知键或类型不符：只丢掉这一项，其余照常恢复。
            _ => {}
        }
    }
    (cfg, true, version)
}

/// 把文件里写的版本号归一化到「当前程序能处理的版本」。
///
/// 比当前版本新的（文件来自更新的 stui）也按当前版本处理：
/// 只认识自己看得懂的字段，看不懂的键忽略掉。
/// 这样「用旧版读新版的文件」最多丢掉几个新设置，
/// 而不是整份作废 —— 整份作废等于把用户的偏好重置一遍。
fn normalize_version(n: f64) -> u32 {
    if !n.is_finite() || n < 1.0 {
        return CONFIG_VERSION;
    }
    let v = n as u64;
    if v > CONFIG_VERSION as u64 {
        CONFIG_VERSION
    } else {
        v as u32
    }
}

/// 数字转 u32：非整数、负数、超出范围都返回 None（当作没写这一项）。
fn as_u32(n: f64) -> Option<u32> {
    if n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64 {
        Some(n as u32)
    } else {
        None
    }
}

/// 数字转 u64：规则同 `as_u32`。
fn as_u64(n: f64) -> Option<u64> {
    if n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u64::MAX as f64 {
        Some(n as u64)
    } else {
        None
    }
}

/// 数字转 u8：规则同 `as_u32`。
fn as_u8(n: f64) -> Option<u8> {
    if n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u8::MAX as f64 {
        Some(n as u8)
    } else {
        None
    }
}

// ------------------------------------------------------------------
// 写
// ------------------------------------------------------------------

/// 临时文件名：与目标同目录，带进程号避免多实例互相覆盖。
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "config.json".into());
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// 写字符串值或 null。
fn push_opt_str(s: &mut String, v: Option<&str>) {
    match v {
        Some(v) => {
            s.push('"');
            escape_into(s, v);
            s.push('"');
        }
        None => s.push_str("null"),
    }
}

/// 写数字值或 null。
fn push_opt_num(s: &mut String, v: Option<u64>) {
    match v {
        Some(v) => s.push_str(&v.to_string()),
        None => s.push_str("null"),
    }
}

/// JSON 字符串转义：引号、反斜杠与控制字符都必须处理，否则文件就不是合法 JSON。
fn escape_into(s: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            '\u{08}' => s.push_str("\\b"),
            '\u{0c}' => s.push_str("\\f"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
}

// ------------------------------------------------------------------
// 迷你 JSON 解析器
// ------------------------------------------------------------------

/// 解析出来的 JSON 值。
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Obj(Vec<(String, Json)>),
    Arr(Vec<Json>),
}

/// 解析一段 JSON 文本；失败返回 Err（调用方据此回退默认值）。
fn parse_json(text: &str) -> Result<Json, String> {
    // 长度上限先判：超长的输入直接判为损坏，绝不进入解析。
    if text.len() > MAX_INPUT_LEN {
        return Err(format!("内容过长（超过 {MAX_INPUT_LEN} 字节）"));
    }
    let mut p = Parser { chars: text.chars().collect(), pos: 0, depth: 0 };
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err("内容末尾有多余字符".to_string());
    }
    Ok(v)
}

/// 嵌套层数上限。
///
/// 解析器是递归下降的，没有上限的话一份几十兆层嵌套的配置就能把栈撑爆
/// （「配置文件坏掉不能让程序起不来」这条约定就守不住了）。
const MAX_DEPTH: usize = 64;

/// 配置文件的输入长度上限（字节）。
///
/// 一份正常配置只有几百字节，64 KiB 已经宽松得离谱。
/// 再长就说明文件要么是坏的、要么根本不是人写的，
/// 这种文件连解析都不该开始：光把字符读进内存就够呛。
const MAX_INPUT_LEN: usize = 64 * 1024;

/// 单个 JSON 字符串的长度上限（字节）。
///
/// 防止一份几百字节的配置里塞一个几十兆的字符串，
/// 把内存吃光还顺带拖慢其它部分。
const MAX_STRING_LEN: usize = 8 * 1024;

/// 递归下降解析器。
struct Parser {
    chars: Vec<char>,
    pos: usize,
    /// 当前嵌套深度（对象 / 数组各算一层）。
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        self.skip_ws();
        match self.bump() {
            Some(got) if got == c => Ok(()),
            Some(got) => Err(format!("期望 {c:?}，实际是 {got:?}")),
            None => Err(format!("期望 {c:?}，但内容已结束")),
        }
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            None => Err("内容为空".to_string()),
            Some('{') => self.parse_obj(),
            Some('[') => self.parse_arr(),
            Some('"') => Ok(Json::Str(self.parse_string()?)),
            Some('t') => self.parse_literal("true", Json::Bool(true)),
            Some('f') => self.parse_literal("false", Json::Bool(false)),
            Some('n') => self.parse_literal("null", Json::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!("无法识别的字符 {c:?}")),
        }
    }

    fn parse_literal(&mut self, word: &str, v: Json) -> Result<Json, String> {
        for want in word.chars() {
            match self.bump() {
                Some(got) if got == want => {}
                _ => return Err(format!("期望字面量 {word}")),
            }
        }
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("无效的数字 {text:?}"))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.skip_ws();
        self.expect('"')?;
        let mut out = String::new();
        loop {
            // 长度检查放在循环开头：转义分支里有 continue，
            // 放在循环末尾会被跳过。
            if out.len() > MAX_STRING_LEN {
                return Err(format!("字符串过长（超过 {MAX_STRING_LEN} 字节）"));
            }
            match self.bump() {
                None => return Err("字符串没有结束".to_string()),
                Some('"') => {
                    if out.len() > MAX_STRING_LEN {
                        return Err(format!("字符串过长（超过 {MAX_STRING_LEN} 字节）"));
                    }
                    return Ok(out);
                }
                Some('\\') => {
                    let esc = self.bump().ok_or("转义符后内容已结束")?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{08}'),
                        'f' => out.push('\u{0c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let cp = self.parse_hex4()?;
                            // 代理对：高字节在前
                            if (0xD800..0xDC00).contains(&cp) {
                                let save = self.pos;
                                if self.peek() == Some('\\') {
                                    self.pos += 1;
                                    if self.peek() == Some('u') {
                                        self.pos += 1;
                                        let lo = self.parse_hex4()?;
                                        if (0xDC00..0xE000).contains(&lo) {
                                            let c = 0x10000
                                                + ((cp - 0xD800) << 10)
                                                + (lo - 0xDC00);
                                            out.push(
                                                char::from_u32(c).ok_or("无效的代理对")?,
                                            );
                                            continue;
                                        }
                                    }
                                }
                                self.pos = save;
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            }
                        }
                        other => return Err(format!("不支持的转义 \\{other}")),
                    }
                }
                Some(c) => out.push(c),
            }
        }
    }

    /// 读 4 位十六进制数字（`\uXXXX`）。
    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self.bump().ok_or("\\u 转义不完整")?;
            let d = c.to_digit(16).ok_or_else(|| format!("无效的十六进制数字 {c:?}"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_obj(&mut self) -> Result<Json, String> {
        self.enter()?;
        let r = self.parse_obj_inner();
        self.depth = self.depth.saturating_sub(1);
        r
    }

    fn parse_arr(&mut self) -> Result<Json, String> {
        self.enter()?;
        let r = self.parse_arr_inner();
        self.depth = self.depth.saturating_sub(1);
        r
    }

    /// 进入一层嵌套：超过上限就报错，避免递归把栈撑爆。
    fn enter(&mut self) -> Result<(), String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!("嵌套过深（超过 {MAX_DEPTH} 层）"));
        }
        self.depth += 1;
        Ok(())
    }

    fn parse_obj_inner(&mut self) -> Result<Json, String> {
        self.expect('{')?;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            let key = self.parse_string()?;
            self.expect(':')?;
            let val = self.parse_value()?;
            entries.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => return Ok(Json::Obj(entries)),
                Some(c) => return Err(format!("对象里出现了意外的字符 {c:?}")),
                None => return Err("对象没有闭合".to_string()),
            }
        }
    }

    fn parse_arr_inner(&mut self) -> Result<Json, String> {
        self.expect('[')?;
        let mut items: Vec<Json> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => return Ok(Json::Arr(items)),
                Some(c) => return Err(format!("数组里出现了意外的字符 {c:?}")),
                None => return Err("数组没有闭合".to_string()),
            }
        }
    }
}

// ------------------------------------------------------------------
// 测试
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 建一个独立的临时目录，避免测试动到用户真实的配置文件。
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "stui-config-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    fn sample() -> Config {
        Config {
            port: Some("COM3".to_string()),
            baud: Some(115_200),
            bits: Some(8),
            parity: Some("none".to_string()),
            stops: Some(1),
            flow: Some("none".to_string()),
            view: Some("hex".to_string()),
            interval_ms: Some(250),
            fps: Some(30),
            auto_reconnect: true,
            show_ms: true,
            wrap: true,
            auto_ports: true,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.json");
        let cfg = sample();
        cfg.save_to(&path).expect("保存失败");

        let (got, from_file) = load_from(&path);
        assert_eq!(cfg, got, "保存后再读回来，字段应当完全一致");
        assert!(from_file, "能解析的配置应当报告为「来自文件」");

        // 内容是单行 JSON
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains('\n'), "配置文件应当只有一行：{text}");
        assert!(text.starts_with('{') && text.ends_with('}'), "应当是一个 JSON 对象：{text}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_json_falls_back_to_default() {
        let dir = temp_dir("corrupt");
        let path = dir.join("config.json");
        fs::write(&path, "{ this is not valid json").unwrap();

        let (got, from_file) = load_from(&path);
        assert_eq!(got, Config::default(), "坏掉的 JSON 应当退回默认值");
        assert!(!from_file, "坏掉的配置不算「来自文件」");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let dir = temp_dir("missing");
        let path = dir.join("not-created-yet").join("config.json");

        let (got, from_file) = load_from(&path);
        assert_eq!(got, Config::default(), "文件不存在时应当退回默认值");
        assert!(!from_file, "文件不存在时不算「来自文件」");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = temp_dir("mkdir");
        let path = dir.join("nested").join("deep").join("config.json");

        sample().save_to(&path).expect("保存失败：父目录应当被自动创建");
        assert!(path.exists(), "配置文件应当已经写出来");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strings_are_escaped_and_restored() {
        let dir = temp_dir("escape");
        let path = dir.join("config.json");
        let mut cfg = sample();
        cfg.port = Some("C:\\dev\\\"x\"".to_string());

        cfg.save_to(&path).expect("保存失败");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\\\\"), "反斜杠应当被转义：{text}");
        assert!(text.contains("\\\""), "引号应当被转义：{text}");

        let (got, _) = load_from(&path);
        assert_eq!(got.port, cfg.port, "转义后的字符串应当能原样读回");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_and_mistyped_keys_are_ignored() {
        let dir = temp_dir("partial");
        let path = dir.join("config.json");
        fs::write(
            &path,
            "{\"port\":\"COM7\",\"future_key\":42,\"baud\":\"not-a-number\",\"show_ms\":true}",
        )
        .unwrap();

        let (got, _) = load_from(&path);
        assert_eq!(got.port.as_deref(), Some("COM7"));
        assert_eq!(got.baud, None, "类型不符的字段应当退回默认值");
        assert!(got.show_ms, "类型正确的字段应当被恢复");
        assert_eq!(got.fps, None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_path_ends_with_config_json() {
        let p = Config::path();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("config.json"));
        assert!(p.parent().is_some());
    }

    /// 嵌套太深的 JSON 必须报错，而不是把栈撑爆。
    ///
    /// 解析器是递归下降的，没有深度上限时一份几十万层嵌套的配置
    /// 会让进程直接 abort（栈溢出），「配置文件坏掉不能让程序起不来」
    /// 这条约定就守不住了。
    ///
    /// 嵌套层数只比 MAX_DEPTH 深一点：再深整份文件就超过输入长度上限了，
    /// 那样先撞上的是长度检查，测不到深度这一层。
    #[test]
    fn deeply_nested_json_is_rejected() {
        let depth = MAX_DEPTH + 10;
        let deep = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let e = parse_json(&deep).unwrap_err();
        assert!(e.contains("嵌套过深"), "应报嵌套过深：{e}");
    }

    /// 太深的配置文件要退回默认值，而不是让程序起不来。
    #[test]
    fn deeply_nested_config_falls_back_to_default() {
        let dir = temp_dir("deep");
        let path = dir.join("config.json");
        let depth = MAX_DEPTH + 10;
        let deep = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        fs::write(&path, &deep).unwrap();

        let (got, from_file) = load_from(&path);
        assert_eq!(got, Config::default(), "太深的配置应当退回默认值");
        assert!(!from_file, "解析失败的配置不算「来自文件」");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parser_handles_escapes_and_unicode() {        let v = parse_json("{\"a\":\"\\u4e2d\\u6587\\n\\t\\\"\",\"b\":[1,2.5,true,null]}").unwrap();
        let Json::Obj(entries) = v else { panic!("应当解析成对象") };
        assert_eq!(entries[0].1, Json::Str("中文\n\t\"".to_string()));
        assert_eq!(
            entries[1].1,
            Json::Arr(vec![
                Json::Num(1.0),
                Json::Num(2.5),
                Json::Bool(true),
                Json::Null
            ])
        );
    }

    /// 两个新开关必须和 auto_reconnect / show_ms 一样能存能取。
    ///
    /// 开关是「有就是有、没有就是没有」的值，
    /// 写盘时漏掉一个字段就等于把这个开关永久关掉了。
    #[test]
    fn wrap_and_auto_ports_roundtrip() {
        let dir = temp_dir("switches");
        let path = dir.join("config.json");

        for (wrap, auto_ports) in [(true, true), (true, false), (false, true), (false, false)] {
            let cfg = Config { wrap, auto_ports, ..Config::default() };
            cfg.save_to(&path).expect("保存失败");

            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains("\"wrap\""), "文件里应当有 wrap 字段：{text}");
            assert!(text.contains("\"auto_ports\""), "文件里应当有 auto_ports 字段：{text}");

            let (got, from_file) = load_from(&path);
            assert!(from_file, "能解析的配置应当报告为「来自文件」");
            assert_eq!(got.wrap, wrap, "wrap 应当原样读回");
            assert_eq!(got.auto_ports, auto_ports, "auto_ports 应当原样读回");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// 超过长度上限的配置文件要当成损坏：退回默认值，绝不解析。
    ///
    /// 就算文件内容其实是一份合法 JSON（只是特别长）也一样拒绝：
    /// 上限的意义就是「不这么大」，而不是「大到什么程度算合法」。
    #[test]
    fn oversized_config_is_rejected_without_panic() {
        let dir = temp_dir("oversized");
        let path = dir.join("config.json");
        // 一份语法完全合法、但超过 64 KiB 的配置。
        let big = format!(
            "{{\"port\":\"COM3\",\"note\":\"{}\"}}",
            "x".repeat(MAX_INPUT_LEN + 1024)
        );
        fs::write(&path, &big).unwrap();

        let (got, from_file) = load_from(&path);
        assert_eq!(got, Config::default(), "超长的配置应当退回默认值");
        assert!(!from_file, "超长的配置不算「来自文件」");

        // 直接调解析器也必须是 Err，而不是解析成功。
        let e = parse_json(&big).unwrap_err();
        assert!(e.contains("过长"), "应报内容过长：{e}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 单个字符串超长时也要拒绝，避免一个巨型字符串把内存吃光。
    #[test]
    fn oversized_string_is_rejected() {
        let long = "y".repeat(MAX_STRING_LEN + 1);
        let text = format!("{{\"port\":\"{long}\"}}");
        let e = parse_json(&text).unwrap_err();
        assert!(e.contains("字符串过长"), "应报字符串过长：{e}");
    }

    /// 配置文件里的未知键必须被忽略，而不是让整份配置作废。
    ///
    /// 向前兼容：新版本写出的键，旧版本读到时只当没看见；
    /// 如果因为一个不认识的键就丢掉全部设置，那升级就会变成「重置」。
    #[test]
    fn unknown_keys_are_ignored_but_known_ones_survive() {
        let dir = temp_dir("unknown");
        let path = dir.join("config.json");
        fs::write(
            &path,
            "{\"port\":\"COM7\",\"wrap\":true,\"auto_ports\":false,\"future_flag\":true,\"nested\":{\"a\":[1,2]}}",
        )
        .unwrap();

        let (got, from_file) = load_from(&path);
        assert!(from_file, "带未知键的配置仍然是「来自文件」");
        assert_eq!(got.port.as_deref(), Some("COM7"));
        assert!(got.wrap, "已知字段应当被恢复");
        assert!(!got.auto_ports, "已知字段应当被恢复");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 写出来的配置必须带版本号，并且带版本号的配置能原样读回。
    ///
    /// 版本号是「这份文件是哪一版格式」的标记：
    /// 以后要改格式（比如改键名、改含义）时，能一眼分清新旧文件，
    /// 不用靠猜某个键存不存在来判断。
    #[test]
    fn config_with_version_roundtrips() {
        let dir = temp_dir("version");
        let path = dir.join("config.json");
        let cfg = sample();
        cfg.save_to(&path).expect("保存失败");

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(&format!("\"version\":{CONFIG_VERSION}")),
            "配置里应当写明版本号：{text}"
        );

        // 读回来仍然是原来那份配置：版本号只描述文件格式，不影响内容。
        let (got, from_file) = load_from(&path);
        assert!(from_file, "能解析的配置应当报告为「来自文件」");
        assert_eq!(cfg, got, "带版本号的配置应当能原样读回");
        assert_eq!(config_version_at(&path), CONFIG_VERSION);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 没有版本号的老配置必须照样能读出已知字段。
    ///
    /// 这是最要紧的一条兼容性：用户升级前存下的配置不能因为
    /// 「少了一个它压根不知道存在的字段」就被丢掉。
    #[test]
    fn legacy_config_without_version_is_still_readable() {
        let dir = temp_dir("legacy");
        let path = dir.join("config.json");
        // 一份没有 version 的老配置：字段名与现在一致，只是没有版本标记。
        fs::write(
            &path,
            "{\"port\":\"COM5\",\"baud\":9600,\"bits\":8,\"parity\":\"none\",\"stops\":1,\
             \"flow\":\"none\",\"view\":\"text\",\"interval_ms\":200,\"fps\":25,\
             \"auto_reconnect\":true,\"show_ms\":true,\"wrap\":true,\"auto_ports\":false}",
        )
        .unwrap();

        let (got, from_file) = load_from(&path);
        assert!(from_file, "没有版本号的老配置仍然是「来自文件」");
        assert_eq!(got.port.as_deref(), Some("COM5"));
        assert_eq!(got.baud, Some(9600));
        assert_eq!(got.bits, Some(8));
        assert_eq!(got.parity.as_deref(), Some("none"));
        assert_eq!(got.stops, Some(1));
        assert_eq!(got.flow.as_deref(), Some("none"));
        assert_eq!(got.view.as_deref(), Some("text"));
        assert_eq!(got.interval_ms, Some(200));
        assert_eq!(got.fps, Some(25));
        assert!(got.auto_reconnect);
        assert!(got.show_ms);
        assert!(got.wrap);
        assert!(!got.auto_ports);
        // 缺版本号的老配置按当前版本对待。
        assert_eq!(config_version_at(&path), CONFIG_VERSION);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 版本号是未来的值时不能 panic，也不能把整份配置丢掉。
    ///
    /// 用户可能拿新版本写的配置喂给旧版本（旧版本写的版本号更大）。
    /// 正确的反应是「尽力而为」：认得出的字段照常恢复，
    /// 认不出的键忽略掉 —— 而不是让程序起不来。
    #[test]
    fn future_version_is_tolerated_without_panic() {
        let dir = temp_dir("future");
        let path = dir.join("config.json");
        fs::write(
            &path,
            "{\"version\":999,\"port\":\"COM9\",\"baud\":115200,\"wrap\":true,\
             \"some_new_switch\":{\"a\":[1,2]}}",
        )
        .unwrap();

        let (got, from_file) = load_from(&path);
        assert!(from_file, "未来版本号的配置仍然是「来自文件」");
        assert_eq!(got.port.as_deref(), Some("COM9"));
        assert_eq!(got.baud, Some(115_200));
        assert!(got.wrap, "已知字段应当照常恢复");
        // 未来版本认识的字段我们不认识，忽略即可，不能因此作废整份配置。
        assert_eq!(config_version_at(&path), CONFIG_VERSION, "未来版本号应当按当前版本处理");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 版本号写成非数字（甚至是非整数）时同样按当前版本处理，不许 panic。
    #[test]
    fn broken_version_field_is_tolerated() {
        let dir = temp_dir("badversion");
        let path = dir.join("config.json");
        fs::write(&path, "{\"version\":\"one\",\"port\":\"COM4\"}").unwrap();
        let (got, from_file) = load_from(&path);
        assert!(from_file);
        assert_eq!(got.port.as_deref(), Some("COM4"));
        assert_eq!(config_version_at(&path), CONFIG_VERSION);

        let path2 = dir.join("config2.json");
        fs::write(&path2, "{\"version\":-3,\"port\":\"COM4\"}").unwrap();
        let (got2, from_file2) = load_from(&path2);
        assert!(from_file2);
        assert_eq!(got2.port.as_deref(), Some("COM4"));

        let _ = fs::remove_dir_all(&dir);
    }
}
