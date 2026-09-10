// A tiny stdio MCP server: answers initialize, tools/list and tools/call with one tool named
// after argv[2], so the server's identity is visible in the tool it offers. Notifications
// (no id) are ignored.
const name = process.argv[2] || "stub";
let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buf += chunk;
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.id === undefined) continue;
    let result;
    if (msg.method === "initialize") {
      result = {
        protocolVersion: (msg.params && msg.params.protocolVersion) || "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name, version: "0.0.1" },
      };
    } else if (msg.method === "tools/list") {
      result = {
        tools: [{
          name: `${name}_ping`,
          description: `ping from ${name}`,
          inputSchema: { type: "object", properties: {} },
        }],
      };
    } else if (msg.method === "tools/call") {
      result = { content: [{ type: "text", text: `pong from ${name}` }] };
    } else {
      result = {};
    }
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }) + "\n");
  }
});
