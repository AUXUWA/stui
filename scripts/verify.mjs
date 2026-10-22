#!/usr/bin/env node
// stui 的独立契约自检脚本。
//
// 用途：不依赖 cargo test，直接拿构建好的二进制跑一遍面向脚本/AI 的调用契约，
// 检查「退出码 + stdout 的 JSON」是不是还成立。CI 或发版前手动跑一遍即可。
//
//   node scripts/verify.mjs [二进制路径]
//   node scripts/verify.mjs target/release/stui.exe
//
// 退出码：0 全部通过，1 有用例失败。

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const bin = resolve(process.argv[2] ?? "target/release/stui.exe");
if (!existsSync(bin)) {
  console.error(`找不到二进制：${bin}\n先执行 cargo build --release`);
  process.exit(1);
}

let pass = 0;
let fail = 0;
const failures = [];

/** 跑一次 stui，返回 { code, stdout, stderr }。 */
function run(args, timeoutMs = 15000) {
  const r = spawnSync(bin, args, { encoding: "utf8", timeout: timeoutMs });
  return {
    code: r.status ?? -1,
    stdout: r.stdout ?? "",
    stderr: r.stderr ?? "",
  };
}

/** 解析 stdout 里的单行 JSON；失败返回 null。 */
function jsonOf(out) {
  const line = out.trim().split("\n").filter((l) => l.trim().length > 0).pop();
  if (!line) return null;
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}

function check(name, cond, detail) {
  if (cond) {
    pass += 1;
    console.log(`  ok   ${name}`);
  } else {
    fail += 1;
    failures.push(`${name}${detail ? " —— " + detail : ""}`);
    console.log(`  FAIL ${name}${detail ? " —— " + detail : ""}`);
  }
}

console.log(`stui 契约自检：${bin}\n`);

// ---------------------------------------------------------------- 既有契约（硬性）

console.log("list");
{
  const r = run(["list", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("输出是单行 JSON 对象", j !== null && typeof j === "object");
  check("含 ok=true", j?.ok === true);
  check("含 ports 数组", Array.isArray(j?.ports));
  check("输出只有一行", r.stdout.trim().split("\n").length === 1);
}

console.log("send（文本回环）");
{
  const r = run(["send", "--port", "sim", "--text", "hi\\r\\n", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("ok=true", j?.ok === true);
  check("sent=4", j?.sent === 4, `实际 ${j?.sent}`);
  check("received=4", j?.received === 4, `实际 ${j?.received}`);
  check("rx_hex=68690D0A", j?.rx_hex === "68690D0A", `实际 ${j?.rx_hex}`);
}

console.log("send（十六进制回环）");
{
  const r = run(["send", "--port", "sim", "--hex", "41 42", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("sent=2", j?.sent === 2, `实际 ${j?.sent}`);
  check("rx_hex=4142", j?.rx_hex === "4142", `实际 ${j?.rx_hex}`);
}

console.log("send（奇数位十六进制应被拒绝）");
{
  const r = run(["send", "--port", "sim", "--text", "hi", "--hex", "ABC", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 2", r.code === 2, `实际 ${r.code}`);
  check("ok=false", j?.ok === false);
  check("error 非空", typeof j?.error === "string" && j.error.length > 0);
}

console.log("send（缺少载荷来源应被拒绝）");
{
  const r = run(["send", "--port", "sim", "--json"]);
  check("退出码为 2", r.code === 2, `实际 ${r.code}`);
}

console.log("listen（静默端口应超时）");
{
  const r = run(["listen", "--port", "sim", "--timeout", "300", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 4", r.code === 4, `实际 ${r.code}`);
  check("ok=false", j?.ok === false);
  check("error=timeout", j?.error === "timeout", `实际 ${j?.error}`);
}

console.log("sim（就绪信号走 stderr）");
{
  const r = run(["sim", "--duration", "300", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("ok=true", j?.ok === true);
  check("stderr 含 stui-sim ready", r.stderr.includes("stui-sim ready"));
}

console.log("未知子命令");
{
  const r = run(["bogus", "--json"]);
  check("退出码为 2", r.code === 2, `实际 ${r.code}`);
}

// ---------------------------------------------------------------- 新增能力（非硬性）

console.log("watch（NDJSON 流式）");
{
  const r = run(["watch", "--port", "sim", "--timeout", "400", "--json"], 20000);
  const lines = r.stdout.trim().split("\n").filter((l) => l.trim().length > 0);
  let allJson = lines.length > 0;
  for (const l of lines) {
    try {
      JSON.parse(l);
    } catch {
      allJson = false;
    }
  }
  check("退出码为 0 或 4", r.code === 0 || r.code === 4, `实际 ${r.code}`);
  check("每一行都是合法 JSON", allJson, `共 ${lines.length} 行`);
  check("最后一行是汇总（带 done 字段）", jsonOf(r.stdout)?.done === true);
}

console.log("selftest");
{
  const r = run(["selftest", "--json"], 20000);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("ok=true", j?.ok === true);
  check("checks 数组非空", Array.isArray(j?.checks) && j.checks.length > 0);
}

console.log("ports（list 的别名）");
{
  const r = run(["ports", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("结构与 list 一致", j?.ok === true && Array.isArray(j?.ports));
}

console.log("send --dry-run");
{
  const r = run(["send", "--port", "sim", "--text", "hi", "--dry-run", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("planned=2", j?.planned === 2, `实际 ${j?.planned}`);
  check("sent=0", j?.sent === 0, `实际 ${j?.sent}`);
}

console.log("list --kind");
{
  const r = run(["list", "--kind", "usb", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("输出是合法 JSON", j !== null);
}

console.log("--help");
{
  const r = run(["--help"]);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  for (const token of ["list", "send", "listen", "sim", "watch", "selftest", "--json"]) {
    check(`--help 含 ${token}`, r.stdout.includes(token));
  }
}

// ---------------------------------------------------------------- 第二轮：新增能力

console.log("record（接收数据落盘）");
{
  const out = "target/verify-record.log";
  const r = run(["record", "--port", "sim", "--duration", "300", "--out", out, "--json"], 20000);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("ok=true", j?.ok === true);
  check("含 bytes 字段", typeof j?.bytes === "number");
  check("含 chunks 字段", typeof j?.chunks === "number");
  check("含 out 字段", typeof j?.out === "string" && j.out.length > 0);
}

console.log("send --expect（回显匹配）");
{
  const r = run(["send", "--port", "sim", "--text", "hi", "--expect", "6869", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("expect_ok=true", j?.expect_ok === true, `实际 ${j?.expect_ok}`);
}

console.log("send --expect（回显不匹配应为退出码 5）");
{
  const r = run(["send", "--port", "sim", "--text", "hi", "--expect", "414243", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 5", r.code === 5, `实际 ${r.code}`);
  check("expect_ok=false", j?.expect_ok === false, `实际 ${j?.expect_ok}`);
}

console.log("send --expect（奇数位十六进制仍是参数错误）");
{
  const r = run(["send", "--port", "sim", "--hex", "ABC", "--expect", "41", "--json"]);
  check("退出码为 2", r.code === 2, `实际 ${r.code}`);
}

console.log("listen --format ndjson");
{
  const r = run(["listen", "--port", "sim", "--timeout", "300", "--format", "ndjson", "--json"], 20000);
  const lines = r.stdout.trim().split("\n").filter((l) => l.trim().length > 0);
  let allJson = lines.length > 0;
  for (const l of lines) {
    try {
      JSON.parse(l);
    } catch {
      allJson = false;
    }
  }
  check("退出码为 0 或 4", r.code === 0 || r.code === 4, `实际 ${r.code}`);
  check("每一行都是合法 JSON", allJson, `共 ${lines.length} 行`);
  check("最后一行是汇总（带 done 字段）", jsonOf(r.stdout)?.done === true);
}

console.log("list --json 的端口字段");
{
  const r = run(["list", "--json"]);
  const j = jsonOf(r.stdout);
  const ports = Array.isArray(j?.ports) ? j.ports : [];
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("ports 是数组", Array.isArray(j?.ports));
  check(
    "每个端口都有 manufacturer 与 product 字段",
    ports.every((p) => "manufacturer" in p && "product" in p),
    `${ports.length} 个端口`,
  );
}

console.log("--help（第二轮新增项）");
{
  const r = run(["--help"]);
  for (const token of ["record", "--expect", "--format"]) {
    check(`--help 含 ${token}`, r.stdout.includes(token));
  }
}

// ---------------------------------------------------------------- 第三轮：新增能力

console.log("send --expect（多个期望：全部命中）");
{
  // "ABhi" 回环回来是 41426869，两个期望都应当命中
  const r = run(["send", "--port", "sim", "--text", "ABhi", "--expect", "4142,6869", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 0", r.code === 0, `实际 ${r.code}`);
  check("expect_ok=true", j?.expect_ok === true, `实际 ${j?.expect_ok}`);
  check("expect_missing 是空数组", Array.isArray(j?.expect_missing) && j.expect_missing.length === 0, `实际 ${JSON.stringify(j?.expect_missing)}`);
}

console.log("send --expect（多个期望：有一个不命中）");
{
  const r = run(["send", "--port", "sim", "--text", "hi", "--expect", "4142,6869", "--json"]);
  const j = jsonOf(r.stdout);
  check("退出码为 5", r.code === 5, `实际 ${r.code}`);
  check("expect_ok=false", j?.expect_ok === false, `实际 ${j?.expect_ok}`);
  check(
    "expect_missing 列出了没命中的那个",
    Array.isArray(j?.expect_missing) && j.expect_missing.length === 1 && /4142/i.test(j.expect_missing[0]),
    `实际 ${JSON.stringify(j?.expect_missing)}`,
  );
}

console.log("send --expect（多个期望里有非法十六进制）");
{
  const r = run(["send", "--port", "sim", "--text", "hi", "--expect", "41,ZZ", "--json"]);
  check("退出码为 2", r.code === 2, `实际 ${r.code}`);
}

console.log("listen --save（落盘）");
{
  const out = "target/verify-listen-save.bin";
  const r = run(["listen", "--port", "sim", "--timeout", "300", "--save", out, "--json"], 20000);
  const j = jsonOf(r.stdout);
  check("退出码为 0 或 4", r.code === 0 || r.code === 4, `实际 ${r.code}`);
  check("JSON 里有 out 字段", typeof j?.out === "string" && j.out.length > 0, `实际 ${j?.out}`);
  check("stdout 仍是纯 JSON 行", (() => {
    for (const l of r.stdout.trim().split("\n").filter((x) => x.trim())) {
      try {
        JSON.parse(l);
      } catch {
        return false;
      }
    }
    return true;
  })());
}

console.log("record --stdout（原始字节输出）");
{
  const r = run(["record", "--port", "sim", "--duration", "300", "--stdout", "--json"], 20000);
  check("退出码为 0 或 2", r.code === 0 || r.code === 2, `实际 ${r.code}`);
  if (r.code === 0) {
    check("stdout 里出现了原始字节（不是 JSON 行）", !r.stdout.trim().startsWith("{"), `实际 ${JSON.stringify(r.stdout.slice(0, 40))}`);
  }
}

console.log("record --stdout 与 --json 冲突");
{
  const r = run(["record", "--port", "sim", "--duration", "200", "--stdout", "--json"], 20000);
  check("退出码为 2", r.code === 2, `实际 ${r.code}`);
}

// ---------------------------------------------------------------- 汇总

console.log(`\n通过 ${pass}，失败 ${fail}`);
if (fail > 0) {
  console.log("\n失败用例：");
  for (const f of failures) console.log(`  - ${f}`);
  process.exit(1);
}
process.exit(0);
