// marion's MCP declaration for pi (`marion_harness::pi`). pi has no MCP client of its own; this
// extension is one, for exactly one stdio server: it starts the server named below, offers each of
// its tools to pi's model under the given prefix, and forwards every call. marion writes one per
// node, with that node's server filled in, and loads it with `pi -e <this file>`.
//
// An MCP result with `isError: true` is thrown, which pi records as `isError: true` on
// `tool_execution_end`. A server that cannot start fails the extension load, which pi reports on
// stderr and exits 1 on, before any request is made.
import { spawn } from "node:child_process";

const SERVER = __MARION_SERVER__;
const PREFIX = __MARION_PREFIX__;

export default async function (pi) {
  const child = spawn(SERVER.command, SERVER.args, {
    env: { ...process.env, ...SERVER.env },
    stdio: ["pipe", "pipe", "inherit"],
  });
  const pending = new Map();
  let next = 1;
  let buffer = "";
  const fail = (why) => {
    for (const { reject } of pending.values()) reject(new Error(why));
    pending.clear();
  };
  child.on("error", (e) => fail(`marion's MCP server did not start: ${e.message}`));
  child.on("exit", (code, signal) => fail(`marion's MCP server exited (${signal ?? code})`));
  child.stdin.on("error", (e) => fail(`marion's MCP server closed its input: ${e.message}`));
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    buffer += chunk;
    let nl;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, nl).trim();
      buffer = buffer.slice(nl + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      const waiter = msg.id !== undefined && pending.get(msg.id);
      if (!waiter) continue;
      pending.delete(msg.id);
      if (msg.error) waiter.reject(new Error(msg.error.message ?? JSON.stringify(msg.error)));
      else waiter.resolve(msg.result);
    }
  });
  const send = (frame) => child.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...frame }) + "\n");
  const request = (method, params) =>
    new Promise((resolve, reject) => {
      const id = next++;
      pending.set(id, { resolve, reject });
      send({ id, method, params });
    });

  await request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "pi-marion-extension", version: "1" },
  });
  send({ method: "notifications/initialized", params: {} });
  const { tools = [] } = await request("tools/list", {});

  for (const tool of tools) {
    pi.registerTool({
      name: PREFIX + tool.name,
      label: PREFIX + tool.name,
      description: tool.description ?? tool.name,
      parameters: tool.inputSchema ?? { type: "object", properties: {} },
      async execute(_id, params) {
        const result = await request("tools/call", { name: tool.name, arguments: params ?? {} });
        const content = (result?.content ?? []).map((c) =>
          c.type === "image"
            ? { type: "image", data: c.data, mimeType: c.mimeType }
            : { type: "text", text: c.text ?? JSON.stringify(c) },
        );
        if (result?.isError) {
          throw new Error(content.map((c) => c.text ?? "").join("\n") || "the call was refused");
        }
        return { content, details: result?.structuredContent ?? {} };
      },
    });
  }

  const stop = () => {
    if (child.exitCode === null && child.signalCode === null) child.kill();
  };
  pi.on("session_shutdown", async () => stop());
  process.on("exit", stop);
}
