// Exercise the embedded pages through Chrome's private DevTools pipe.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const input = JSON.parse(readFileSync(0, "utf8"));
const chrome = spawn(process.argv[2], [
  "--headless", "--disable-gpu", "--disable-dev-shm-usage", "--no-first-run",
  "--disable-background-networking", "--disable-sync", "--disable-extensions",
  "--disable-component-update", "--no-default-browser-check", "--password-store=basic",
  "--remote-debugging-pipe", "--user-data-dir=" + input.profile, "about:blank",
], { stdio: ["ignore", "ignore", "inherit", "pipe", "pipe"] });

const [, , , commands, replies] = chrome.stdio;
const pending = new Map();
const requests = [];
const exceptions = [];
let nextId = 0;
let sessionId;
let buffer = "";

function close(error) {
  for (const { reject } of pending.values()) reject(error);
  pending.clear();
}
chrome.on("error", close);
chrome.on("exit", (code) => close(new Error("Chrome stopped with exit code " + code)));
commands.on("error", close);
replies.on("error", close);

replies.setEncoding("utf8").on("data", (chunk) => {
  const frames = (buffer + chunk).split("\0");
  buffer = frames.pop();
  for (const frame of frames) {
    if (!frame) continue;
    const message = JSON.parse(frame);
    const waiter = pending.get(message.id);
    if (waiter) {
      pending.delete(message.id);
      if (message.error) waiter.reject(new Error(message.error.message));
      else waiter.resolve(message.result);
    }
    if (message.method === "Network.requestWillBeSent") {
      requests.push(message.params.request.url);
    } else if (message.method === "Runtime.exceptionThrown") {
      exceptions.push(message.params.exceptionDetails.text);
    }
  }
});

function send(method, params = {}, timeout = 8_000) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    const deadline = setTimeout(() => {
      pending.delete(id);
      reject(new Error("DevTools command timed out: " + method));
    }, timeout);
    pending.set(id, {
      resolve: (value) => { clearTimeout(deadline); resolve(value); },
      reject: (error) => { clearTimeout(deadline); reject(error); },
    });
    commands.write(JSON.stringify({ id, sessionId, method, params }) + "\0");
  });
}

async function evaluate(expression) {
  const result = await send("Runtime.evaluate", { expression, returnByValue: true });
  assert.ok(!result.exceptionDetails, "Chrome could not evaluate the test expression");
  return result.result.value;
}

async function waitFor(description, expression) {
  for (let attempt = 0; attempt < 160; attempt += 1) {
    if (await evaluate(expression)) return;
    await delay(50);
  }
  throw new Error("Timed out waiting for " + description);
}

function click(selector) {
  return evaluate("document.querySelector(" + JSON.stringify(selector) + ").click()");
}

async function selectTask(id) {
  const selector = '#task-rows button[data-task-id="' + id + '"]';
  const visible = 'document.querySelector(' + JSON.stringify(selector) + ') !== null';
  if (!await evaluate(visible)) {
    await click(await evaluate('document.getElementById("next").disabled') ? "#previous" : "#next");
    await waitFor("page containing the requested task", visible);
  }
  await click(selector);
}

async function verifyDownload(digest) {
  await click("#download-artifact");
  const saved = join(input.downloads, "psios-" + digest.slice("sha256:".length) + ".bin");
  for (let attempt = 0; attempt < 160 && !existsSync(saved); attempt += 1) await delay(50);
  const bytes = readFileSync(saved);
  assert.equal("sha256:" + createHash("sha256").update(bytes).digest("hex"), digest);
  return bytes;
}

async function test() {
  await send("Browser.getVersion", {}, 35_000);
  await send("Browser.setDownloadBehavior", { behavior: "allow", downloadPath: input.downloads });
  const { targetId } = await send("Target.createTarget", { url: "about:blank" }, 30_000);
  ({ sessionId } = await send("Target.attachToTarget", { targetId, flatten: true }));
  await send("Page.enable");
  await send("Network.enable");
  await send("Runtime.enable");
  await send("Page.navigate", { url: input.origin }, 25_000);
  await waitFor("ready pairing page",
    'document.title === "PsiOS Shell" && document.readyState === "complete" && !document.getElementById("pairing").hidden');
  await evaluate('document.getElementById("code").focus()');
  await send("Input.insertText", { text: input.code });
  await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
  assert.ok(await evaluate('document.activeElement.id === "connect"'));
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter",
    text: "\r", unmodifiedText: "\r", windowsVirtualKeyCode: 13 });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
  await waitFor("authorized node", 'document.getElementById("node-view").textContent.includes(' +
    JSON.stringify(input.node) + ')');
  assert.ok(await evaluate('document.activeElement.id === "view-title" && ' +
    'document.getElementById("code").value === "" && document.cookie === "" && ' +
    'localStorage.length === 0 && sessionStorage.length === 0'));

  const reserves = await evaluate(`Array.from(document.querySelectorAll("#node-view .card"))
    .filter(card => ["状态存储", "见证存储"].includes(card.querySelector("h2").textContent))
    .map(card => ({ fields: Object.fromEntries(Array.from(card.querySelectorAll("dt"),
      label => [label.textContent, label.nextElementSibling.textContent])),
      warnings: Array.from(card.querySelectorAll(".reserve-low"), item => item.textContent) }))`);
  const [state, witness] = reserves;
  assert.deepEqual(state.warnings, [input.inode_observed ?
    "可用 inode 已低于预留数量。" : "无法核对本文件系统配置的 inode 预留数量。"]);
  assert.deepEqual(witness.warnings, ["可用字节已低于预留空间。"]);
  assert.equal(state.fields["预留 inode"], 9007199254740991n.toLocaleString("zh-CN"));
  assert.equal(witness.fields["预留 inode"], "0");
  assert.ok(input.inode_observed ? /^[0-9,]+$/.test(state.fields["可用 inode"]) :
    state.fields["可用 inode"] === "文件系统未提供");

  await click("#nav-tasks");
  await waitFor("first page", 'document.querySelectorAll("#task-rows button").length === 20');
  await click("#next");
  await waitFor("second page", 'document.querySelectorAll("#task-rows button").length === 2');
  await click("#previous");
  await waitFor("return to first page", 'document.querySelectorAll("#task-rows button").length === 20');

  await send("Emulation.setDeviceMetricsOverride", {
    width: 360, height: 800, screenWidth: 360, screenHeight: 800, deviceScaleFactor: 1, mobile: true,
  });
  await waitFor("narrow viewport", "window.innerWidth === 360");
  await selectTask(input.task);
  await waitFor("visible task result", 'document.getElementById("task-content").textContent.includes("成功") && ' +
    'document.getElementById("task-content").textContent.includes(' + JSON.stringify(input.task) + ')');
  assert.ok(await evaluate('document.activeElement.id === "task-detail" && ' +
    'document.getElementById("task-detail").getBoundingClientRect().bottom > 0 && ' +
    'document.getElementById("task-detail").getBoundingClientRect().top < window.innerHeight'));
  assert.equal(await evaluate("document.documentElement.scrollWidth"), 360);
  assert.ok(input.history.length > 3);
  await click(".task-history summary");
  await waitFor("first history page", 'document.querySelectorAll(".task-history li").length === 3');
  assert.deepEqual(await evaluate('Array.from(document.querySelectorAll(".task-history time"), t => t.dateTime)'),
    input.history.slice(0, 3).map(item => item.at));
  await click(".history-controls button:last-child");
  const last = input.history.at(-1);
  await waitFor("settled history revision", 'document.querySelector(".task-history li:last-child strong")' +
    '?.textContent.includes(' + JSON.stringify(last.reference.version + " · 成功") + ')');
  const historyText = await evaluate('document.querySelector(".task-history ol").textContent');
  assert.ok(historyText.includes(last.run_id) && historyText.includes(last.operation_refs[0].resource_id));
  assert.ok(await evaluate('document.activeElement === document.querySelector(".task-history ol")'));
  assert.equal(await evaluate("document.documentElement.scrollWidth"), 360);
  await click(".history-controls button:first-child");
  await waitFor("previous history page", 'document.querySelector(".task-history strong")?.textContent.startsWith("r1 ·")');
  await click(".task-history summary");
  await waitFor("closed history", 'document.querySelectorAll(".task-history li").length === 0');
  await click("#preview-artifact");
  await waitFor("verified artifact preview", 'document.getElementById("artifact-json")?.textContent.includes(' +
    JSON.stringify(input.node) + ')');
  assert.ok(await evaluate('document.activeElement.id === "artifact-json"'));
  await verifyDownload(input.digest);
  await click("#close-task");
  assert.ok(await evaluate('document.getElementById("task-detail").hidden && ' +
    'document.activeElement.dataset.taskId === ' + JSON.stringify(input.task)));

  await selectTask(input.large_task);
  await waitFor("large artifact reference", 'document.querySelector(".artifact-reference")?.textContent.includes(' +
    JSON.stringify(input.large_digest) + ')');
  await click("#preview-artifact");
  await waitFor("size limit", 'document.querySelector(".artifacts .message")?.textContent.includes("64 KiB")');
  assert.ok((await verifyDownload(input.large_digest)).length > 2 * 128 * 1024);
  await selectTask(input.pending);
  await waitFor("pending Task action", 'document.querySelector(".task-cancel button") !== null');
  await click(".task-cancel button");
  await waitFor("authoritative cancellation", 'document.getElementById("task-content").textContent.includes("已取消")');
  assert.ok(await evaluate('!document.querySelector(".task-cancel button") && document.activeElement.id === "task-detail"'));
  await click("#logout");
  await waitFor("cleared session", 'document.getElementById("workspace").hidden && ' +
    'document.getElementById("task-content").textContent === ""');
  assert.deepEqual(exceptions, []);
  assert.ok(requests.length > 6 && requests.every((url) => new URL(url).origin === input.origin));
}

try {
  await test();
} finally {
  chrome.kill("SIGTERM");
}
