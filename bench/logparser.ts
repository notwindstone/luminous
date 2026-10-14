// A log parser: string splitting and slicing, key=value fields, a record
// object per line, aggregation in Maps and Sets, a report.
type Entry = {
  time: string;
  level: string;
  service: string;
  user: string;
  path: string;
  status: number;
  latency: number;
  bytes: number;
};

type Stats = { count: number; errors: number; totalLatency: number; maxLatency: number; bytes: number };

const LEVELS = ["INFO", "INFO", "INFO", "WARN", "ERROR", "DEBUG"];
const SERVICES = ["auth", "billing", "search", "gateway", "profile", "orders", "inventory"];
const PATHS = ["/api/login", "/api/logout", "/api/items", "/api/items/42", "/api/cart", "/api/checkout", "/health"];
const USERS = ["alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy"];

function generate(lines: number): string {
  const out: string[] = [];
  let seed = 7;
  for (let i = 0; i < lines; i++) {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    const level = LEVELS[seed % LEVELS.length];
    const service = SERVICES[(seed >> 3) % SERVICES.length];
    const path = PATHS[(seed >> 6) % PATHS.length];
    const user = USERS[(seed >> 9) % USERS.length];
    const status = level === "ERROR" ? 500 : (seed >> 12) % 10 === 0 ? 404 : 200;
    const latency = (seed >> 4) % 900 + 3;
    const sec = String(i % 60).padStart(2, "0");
    out.push(
      `2024-03-15T10:${String((i / 60) % 60 | 0).padStart(2, "0")}:${sec}Z ${level} [${service}] ` +
        `user=${user} path=${path} status=${status} latency=${latency}ms bytes=${(seed >> 2) % 50000}`,
    );
  }
  return out.join("\n");
}

function parseLine(line: string): Entry | null {
  const sp1 = line.indexOf(" ");
  const sp2 = line.indexOf(" ", sp1 + 1);
  const close = line.indexOf("]", sp2);
  if (sp1 < 0 || sp2 < 0 || close < 0) return null;
  const entry: Entry = {
    time: line.slice(0, sp1),
    level: line.slice(sp1 + 1, sp2),
    service: line.slice(sp2 + 2, close),
    user: "",
    path: "",
    status: 0,
    latency: 0,
    bytes: 0,
  };
  const fields = line.slice(close + 2).split(" ");
  for (const f of fields) {
    const eq = f.indexOf("=");
    if (eq < 0) continue;
    const key = f.slice(0, eq);
    const value = f.slice(eq + 1);
    switch (key) {
      case "user":
        entry.user = value;
        break;
      case "path":
        entry.path = value;
        break;
      case "status":
        entry.status = parseInt(value, 10);
        break;
      case "latency":
        entry.latency = parseInt(value.endsWith("ms") ? value.slice(0, -2) : value, 10);
        break;
      case "bytes":
        entry.bytes = Number(value);
        break;
    }
  }
  return entry;
}

function analyze(text: string): string {
  const byService = new Map<string, Stats>();
  const byPath = new Map<string, number>();
  const users = new Set<string>();
  const errorsByUser = new Map<string, number>();
  let parsed = 0;
  for (const line of text.split("\n")) {
    const e = parseLine(line);
    if (e === null) continue;
    parsed++;
    let s = byService.get(e.service);
    if (s === undefined) {
      s = { count: 0, errors: 0, totalLatency: 0, maxLatency: 0, bytes: 0 };
      byService.set(e.service, s);
    }
    s.count++;
    s.totalLatency += e.latency;
    if (e.latency > s.maxLatency) s.maxLatency = e.latency;
    s.bytes += e.bytes;
    if (e.status >= 500) {
      s.errors++;
      errorsByUser.set(e.user, (errorsByUser.get(e.user) ?? 0) + 1);
    }
    const key = e.path.startsWith("/api/items/") ? "/api/items/:id" : e.path;
    byPath.set(key, (byPath.get(key) ?? 0) + 1);
    users.add(e.user);
  }
  const lines: string[] = [`parsed ${parsed} lines, ${users.size} users`];
  const services = [...byService.keys()].sort();
  for (const name of services) {
    const s = byService.get(name)!;
    const avg = (s.totalLatency / s.count).toFixed(1);
    lines.push(`${name.toUpperCase()}: ${s.count} req, ${s.errors} err, avg ${avg}ms, max ${s.maxLatency}ms, ${s.bytes} bytes`);
  }
  const paths = [...byPath.entries()].sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1));
  lines.push(paths.map(([p, n]) => `${p}=${n}`).join(" "));
  const worst = [...errorsByUser.entries()].sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1))[0];
  lines.push(`most errors: ${worst[0]} (${worst[1]})`);
  return lines.join("\n");
}

const start = Date.now();
const text = generate(200000);
let report = "";
for (let round = 0; round < 3; round++) report = analyze(text);
console.log(report);
console.log(`elapsed: ${Date.now() - start} ms`);
