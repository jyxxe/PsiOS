// Official Tasks extension lifecycle over a direct STDIO port.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { createInterface } from "node:readline";
import { resolve } from "node:path";

const [sdk, command, ...args] = process.argv.slice(2);
const packagePath = (name) => resolve(sdk, "node_modules", "@modelcontextprotocol", name);
const manifest = async (name) => JSON.parse(await readFile(resolve(packagePath(name), "package.json"), "utf8"));
assert.equal((await manifest("ext-tasks")).version, "0.2.3");
assert.equal((await manifest("client")).version, "2.2.0");
const { withTasks, resultFromTaskOutcome } = await import(pathToFileURL(resolve(packagePath("ext-tasks"), "dist/client/index.js")));

const child = spawn(command, args, { stdio: ["pipe", "pipe", "inherit"], env: { PATH: "/usr/bin:/bin" } });
const stdout = createInterface({ input: child.stdout });
const pending = new Map();
const invalidation = new Set();
const invocation = randomUUID();
let sequence = 0;
let closed = false;
const ended = new Promise((done) => child.once("close", done));
child.on("close", () => {
    closed = true;
    const error = new Error("MCP STDIO subprocess closed");
    for (const { reject } of pending.values()) reject(error);
    for (const listener of invalidation) listener(error);
    pending.clear();
});
stdout.on("line", (line) => {
    const frame = JSON.parse(line);
    const request = pending.get(frame.id);
    assert.ok(request, `Unexpected response ID ${frame.id}`);
    pending.delete(frame.id);
    request.accept("error" in frame ? { kind: "error", error: frame.error } : { kind: "result", result: frame.result });
});

const extension = "io.modelcontextprotocol/tasks";
function dispatch(message) {
    const id = ++sequence;
    const params = { ...message.params };
    params._meta = {
        ...params._meta,
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": { extensions: { [extension]: {} } },
    };
    if (message.method === "tools/call") params._meta["io.psios/mcpInvocationKey"] = invocation;
    return new Promise((accept, reject) => {
        if (closed) return reject(new Error("MCP STDIO is closed"));
        pending.set(id, { accept, reject });
        child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method: message.method, params }) + "\n");
    });
}

try {
    const discovery = await dispatch({ method: "server/discover" });
    assert.equal(discovery.kind, "result");
    const capabilities = discovery.result.capabilities.extensions[extension];
    assert.deepEqual(capabilities, {});
    const session = withTasks({
        endpointId: "sdk-verification",
        taskCapabilities: { generation: "v2", capabilities },
        dispatch,
        onServerRequest: () => () => {},
        onNotification: () => () => {},
        onInvalidated(listener) {
            invalidation.add(listener);
            return () => invalidation.delete(listener);
        },
        get invalidated() { return closed; },
    });
    try {
        const execution = await session.callTool("system.status", {});
        assert.equal(execution.kind, "task");
        assert.match(execution.handle.taskId, /^mcp-task-/);
        const { outcome } = await execution.settle();
        assert.equal(outcome.status, "completed");
        console.log(JSON.stringify(resultFromTaskOutcome(outcome)));
    } finally {
        await session.close();
    }
} finally {
    child.stdin.end();
    await ended;
}
