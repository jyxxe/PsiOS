// Exercise the embedded pages through Chrome's private DevTools pipe.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, readFileSync, readSync, writeSync } from "node:fs";
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

async function keyboardActivate(selector) {
  await evaluate("document.querySelector(" + JSON.stringify(selector) + ").focus()");
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter",
    text: "\r", unmodifiedText: "\r", windowsVirtualKeyCode: 13 });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
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

async function verifyDownload(digest, selector = "#download-artifact") {
  await click(selector);
  const saved = join(input.downloads, "psios-" + digest.slice("sha256:".length) + ".bin");
  for (let attempt = 0; attempt < 160 && !existsSync(saved); attempt += 1) await delay(50);
  const bytes = readFileSync(saved);
  assert.equal("sha256:" + createHash("sha256").update(bytes).digest("hex"), digest);
  return bytes;
}

async function verifyRevokedWindow() {
  await click("#nav-node");
  await waitFor("permitted node inventory", 'document.querySelector("#node-view").textContent.includes(' + JSON.stringify(input.node) + ')');
  await click("#nav-tasks");
  await waitFor("agent's own task", 'document.querySelector("#task-rows button") !== null');
  await selectTask(input.task);
  await waitFor("completed Task", 'document.querySelector("#task-content").textContent.includes("成功")');
  const cover = '#window-tasks [data-action="maximize"]';
  await keyboardActivate(input.covered ? cover : '#window-node [data-action="minimize"]');
  assert.ok(await evaluate('document.querySelector("#node-view").textContent === ""'));
  writeSync(input.control_fd, Buffer.from("R"));
  const reply = Buffer.alloc(1);
  assert.equal(readSync(input.control_fd, reply), 1);
  assert.equal(reply.toString(), "K");
  await keyboardActivate(input.covered ? cover : "#nav-node");
  await waitFor("refused inventory after restoration", 'document.querySelector("#status-node").classList.contains("error")');
  assert.ok(await evaluate('document.querySelector("#node-view").textContent === "" && !document.querySelector("#window-tasks").hidden && ' +
    'document.querySelector("#task-content").textContent.includes(' + JSON.stringify(input.task) + ')'));
  await click("#logout");
  await waitFor("cleared desktop", 'document.querySelector("#workspace").hidden && document.querySelector("#task-content").textContent === ""');
  assert.deepEqual(exceptions, []);
  assert.ok(requests.every(url => new URL(url).origin === input.origin));
}

async function verifyWindowTiling() {
  const viewport = { width: 1280, height: 860, screenWidth: 1280, screenHeight: 860, deviceScaleFactor: 1, mobile: false };
  await send("Emulation.setDeviceMetricsOverride", viewport);
  await click("#nav-node");
  await waitFor("node observation", 'document.querySelector("#node-view").textContent.includes(' + JSON.stringify(input.node) + ')');
  await click("#nav-tasks");
  await waitFor("task list", 'document.querySelector("#task-rows button") !== null');
  await selectTask(input.task);
  const detail = 'document.querySelector("#task-content").textContent.includes(' + JSON.stringify(input.task) + ')';
  await waitFor("task details", detail);
  await click("#nav-applications");
  await waitFor("application list", '!document.querySelector("#applications-empty").hidden');
  const initialRequests = requests.length;
  const nodeRight = '#window-node [data-tile-side="right"]';
  const taskLeft = '#window-tasks [data-tile-side="left"]';
  const taskRight = '#window-tasks [data-tile-side="right"]';
  const positions = `(() => {
    const bounds = id => document.getElementById("window-" + id).getBoundingClientRect();
    const tasks = bounds("tasks"), node = bounds("node"), applications = bounds("applications");
    return { tasksFirst: tasks.right <= node.left, nodeFirst: node.right <= tasks.left,
      appBelow: applications.top >= Math.max(tasks.bottom, node.bottom),
      equalHeight: tasks.height === node.height, stacked: node.bottom <= tasks.top,
      equalWidth: tasks.width === node.width };
  })()`;

  await keyboardActivate(nodeRight);
  await keyboardActivate(taskLeft);
  assert.ok(await evaluate('document.activeElement.matches(' + JSON.stringify(taskLeft) + ')'));
  assert.ok(await evaluate('document.activeElement.getAttribute("aria-pressed") === "true"'));
  assert.ok(await evaluate(detail));
  assert.equal(requests.length, initialRequests);
  let layout = await evaluate(positions);
  assert.ok(layout.tasksFirst && layout.appBelow && layout.equalHeight && layout.equalWidth, JSON.stringify(layout));
  assert.deepEqual(await evaluate(`[...document.querySelectorAll("#desktop > section:not([hidden])")].map(window => window.id)`),
    ["window-tasks", "window-node", "window-applications"]);

  await keyboardActivate(taskRight);
  layout = await evaluate(positions);
  assert.ok(layout.nodeFirst && layout.appBelow, JSON.stringify(layout));
  assert.ok(await evaluate('document.activeElement.matches(' + JSON.stringify(taskRight) + ')'));

  const maximize = '#window-tasks [data-action="maximize"]';
  await keyboardActivate(maximize);
  assert.ok(await evaluate(`document.getElementById("window-node").hidden &&
    document.getElementById("node-view").textContent === "" && !document.getElementById("desktop").classList.contains("has-tiles")`));
  await keyboardActivate(maximize);
  await waitFor("restored tiles", 'document.getElementById("node-view").textContent.includes(' + JSON.stringify(input.node) + ')');
  assert.ok((await evaluate(positions)).nodeFirst);
  await keyboardActivate('#window-tasks [data-action="minimize"]');
  assert.ok(await evaluate('document.getElementById("task-content").textContent === ""'));
  await keyboardActivate("#nav-tasks");
  await waitFor("reopened Task details", detail);
  assert.ok((await evaluate(positions)).nodeFirst);

  await send("Emulation.setDeviceMetricsOverride", {
    width: 360, height: 800, screenWidth: 360, screenHeight: 800, deviceScaleFactor: 1, mobile: true,
  });
  await waitFor("narrow width", "window.innerWidth === 360");
  assert.ok((await evaluate(positions)).stacked);
  assert.equal(await evaluate("document.documentElement.scrollWidth"), 360);
  assert.ok(await evaluate('getComputedStyle(document.querySelector(' + JSON.stringify(taskRight) + ')).display === "none"'));
  await send("Emulation.setDeviceMetricsOverride", viewport);
  await waitFor("wide width", "window.innerWidth === 1280");
  assert.ok((await evaluate(positions)).nodeFirst);
  await keyboardActivate(taskRight);
  assert.ok(await evaluate('document.activeElement.getAttribute("aria-pressed") === "false" && !document.getElementById("window-tasks").hasAttribute("data-tile")'));
  await keyboardActivate('#window-node [data-action="close"]');
  await keyboardActivate("#nav-node");
  assert.ok(await evaluate('document.querySelector("#desktop > [data-tile]") === null'));
  await keyboardActivate(nodeRight);
  await click("#logout");
  await waitFor("cleared layout and Task data", `document.getElementById("workspace").hidden &&
    !document.querySelector("#desktop > [data-tile]") && document.getElementById("task-content").textContent === ""`);
  assert.deepEqual(exceptions, []);
  assert.ok(requests.every(url => new URL(url).origin === input.origin));
}

async function test() {
  await send("Browser.getVersion", {}, 35_000);
  if (input.downloads) await send("Browser.setDownloadBehavior", { behavior: "allow", downloadPath: input.downloads });
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
  await waitFor("paired desktop", '!document.querySelector("#workspace").hidden && !document.querySelector("#empty-desktop").hidden');
  if (input.windowTiling) return verifyWindowTiling();
  assert.ok(await evaluate('document.activeElement.id === "nav-node" && document.querySelectorAll(".app-window:not([hidden])").length === 0'));
  assert.ok(!requests.some(url => ["/api/node", "/api/tasks"].includes(new URL(url).pathname)));
  if (input.projectReview) {
    const project = '#project-rows button[data-project-id="' + input.project + '"]';
    const opener = '#project-content [data-work-item-id="' + input.work_item.resource_id + '"] button';
    const present = selector => 'document.querySelector(' + JSON.stringify(selector) + ') !== null';
    const submit = '.work-item-review button[type="submit"]';
    const reviews = () => requests.filter(url => new URL(url).pathname === "/api/review-work-item").length;
    await keyboardActivate("#nav-projects");
    await waitFor("reviewer's project", present(project));
    await keyboardActivate(project);
    await waitFor("pending review", present(opener));
    await keyboardActivate(opener);
    await waitFor("review controls", present(submit));
    assert.ok(await evaluate('document.querySelector(' + JSON.stringify(submit) + ').disabled'));
    await evaluate('(() => { const select = document.querySelector("#work-item-decision"); select.value = "accept"; ' +
      'select.dispatchEvent(new Event("change")); })()');
    assert.equal(reviews(), 0);
    await keyboardActivate(submit);
    await waitFor("unknown review reply", 'document.querySelector(".work-item-review p").textContent.includes("审核回执未收到")');
    assert.ok(await evaluate('document.querySelector(' + JSON.stringify(submit) + ').disabled'));
    await keyboardActivate('.work-item-review button[type="button"]');
    const details = 'document.querySelector("#work-item-content").textContent';
    await waitFor("durable review from Core", details + '.includes("已验证") && ' +
      details + '.includes(' + JSON.stringify(input.reviewer) + ')');
    assert.ok(!await evaluate(present(submit)));
    assert.equal(reviews(), 1);
    await click("#logout");
    await waitFor("closed review session", 'document.querySelector("#workspace").hidden');
    assert.deepEqual(exceptions, []);
    assert.ok(requests.every(url => new URL(url).origin === input.origin));
    return;
  }
  if (input.projectEvidence) {
    const project = '#project-rows button[data-project-id="' + input.project + '"]';
    const opener = '#project-content [data-work-item-id="' + input.work_item.resource_id + '"] button';
    const present = selector => 'document.querySelector(' + JSON.stringify(selector) + ') !== null';
    function checkpoint(mark) {
      writeSync(input.control_fd, Buffer.from(mark));
      const response = Buffer.alloc(1);
      assert.equal(readSync(input.control_fd, response), 1);
      assert.equal(response.toString(), "K");
    }
    await keyboardActivate("#nav-projects");
    await waitFor("evidence project", present(project));
    await keyboardActivate(project);
    await waitFor("work item with evidence", present(opener));
    checkpoint("R");
    await keyboardActivate(opener);
    const details = 'document.querySelector("#work-item-content")';
    const contains = value => details + '.textContent.includes(' + JSON.stringify(value) + ')';
    await waitFor("fixed work item and its task evidence", [input.evidence_task.resource_id,
      input.evidence_artifact.digest, input.work_item.version, "审核中"].map(contains).join(" && "));
    await verifyDownload(input.evidence_artifact.digest, "#download-work-item-evidence");
    assert.ok(await evaluate(contains("交付说明 <em>原样保留</em>") + ' && !' + details + '.querySelector("em")'));
    assert.ok(!await evaluate(contains("已终止")));
    await send("Emulation.setDeviceMetricsOverride", { width: 360, height: 750, deviceScaleFactor: 1, mobile: true });
    assert.ok(await evaluate('document.documentElement.scrollWidth <= 360 && !document.querySelector("#work-item-detail").hidden'));
    await click("#close-work-item");
    assert.ok(await evaluate('document.activeElement === document.querySelector(' + JSON.stringify(opener) + ') && ' + details + '.textContent === ""'));
    await click(opener);
    await waitFor("reopened task evidence", contains(input.evidence_artifact.digest));
    await click('#window-projects [data-action="minimize"]');
    assert.ok(await evaluate(details + '.textContent === "" && document.querySelector("#work-item-detail").hidden'));
    checkpoint("V");
    await click("#nav-projects");
    await waitFor("revoked project directory", 'document.querySelector("#status-projects").classList.contains("error")');
    assert.ok(await evaluate('!document.querySelector("#project-rows button") && ' + details + '.textContent === ""'));
    await click("#logout");
    await waitFor("closed project session", 'document.querySelector("#workspace").hidden');
    assert.deepEqual(exceptions, []);
    assert.ok(requests.every(url => new URL(url).origin === input.origin));
    return;
  }
  if (input.agentControl) {
    const opener = '#agent-session-rows [data-session-id="' + input.session + '"]';
    const pause = '[data-session-action="pause"]';
    const resume = '[data-session-action="resume"]';
    const exists = selector => 'document.querySelector(' + JSON.stringify(selector) + ') !== null';
    const content = 'document.querySelector("#agent-session-content").textContent';
    await keyboardActivate("#nav-agent-runtime");
    await waitFor("controlled session", exists(opener));
    await keyboardActivate(opener);
    await waitFor("pause control", exists(pause));
    await keyboardActivate(pause);
    await waitFor("original pause after the missing acknowledgement", exists(resume) + ' && ' +
      content + '.includes("已暂停") && document.querySelector("#status-agent-runtime").textContent.includes("已重新读取会话")');
    await keyboardActivate(resume);
    await waitFor("confirmed resume", exists(pause) + ' && ' + content + '.includes("等待模型") && ' +
      'document.querySelector("#status-agent-runtime").textContent.includes("已提交恢复")');
    assert.equal(requests.filter(url => new URL(url).pathname === "/api/control-agent-session").length, 2);
    await click("#logout");
    await waitFor("clear paused session controls", 'document.querySelector("#workspace").hidden && ' + content + ' === ""');
    assert.deepEqual(exceptions, []);
    assert.ok(requests.every(url => new URL(url).origin === input.origin));
    return;
  }
  if (input.agentRuntime) {
    const selector = '#agent-session-rows [data-session-id="' + input.session + '"]';
    const opener = 'document.querySelector(' + JSON.stringify(selector) + ')';
    const rows = 'document.querySelector("#agent-session-rows")';
    const detail = 'document.querySelector("#agent-session-detail")';
    const content = 'document.querySelector("#agent-session-content")';
    const count = rows + '.querySelectorAll("button").length';
    const empty = rows + '.textContent === "" && ' + detail + '.hidden && ' + content + '.textContent === ""';
    await keyboardActivate("#nav-agent-runtime");
    await waitFor("first session page", count + " === 8");
    assert.ok(!await evaluate(opener));
    await click("#agent-sessions-next");
    await waitFor("second session page", opener + " && " + count + " === 1");
    assert.ok(!await evaluate(rows + '.textContent.includes(' + JSON.stringify(input.hidden) + ')'));
    await keyboardActivate(selector);
    const values = 'Object.fromEntries(Array.from(' + content + '.querySelectorAll("dt"), ' +
      'key => [key.textContent, key.nextElementSibling.textContent]))';
    await waitFor("aggregated model usage", values + '["报告输出 Token"] === "3"');
    const usage = await evaluate(values);
    assert.equal(usage["报告输入 Token"], "12");
    assert.equal(usage["已签发调用"], "1");
    assert.equal(usage["状态"], "成功");
    assert.ok(!await evaluate('document.querySelector("[data-session-action]")'));
    assert.ok(!await evaluate(content + '.textContent.includes("private")'));
    await send("Emulation.setDeviceMetricsOverride", { width: 360, height: 750, deviceScaleFactor: 1, mobile: true });
    assert.ok(await evaluate('document.documentElement.scrollWidth <= 360 && !' + detail + '.hidden'));
    await click("#close-agent-session");
    assert.ok(await evaluate('document.activeElement === ' + opener + ' && ' + content + '.textContent === ""'));
    await click("#agent-sessions-previous");
    await waitFor("return to the first session page", count + " === 8");
    await click("#agent-sessions-next");
    await waitFor("reopen the settled session", opener);
    await click(selector);
    await waitFor("reopened usage", values + '["报告输出 Token"] === "3"');
    await click('#window-agent-runtime [data-action="minimize"]');
    assert.ok(await evaluate(empty));
    writeSync(input.control_fd, Buffer.from("V"));
    const response = Buffer.alloc(1);
    assert.equal(readSync(input.control_fd, response), 1);
    assert.equal(response.toString(), "K");
    await click("#nav-agent-runtime");
    await waitFor("revoked session reader", 'document.querySelector("#status-agent-runtime").classList.contains("error")');
    assert.ok(await evaluate(empty));
    await click("#logout");
    await waitFor("paired desktop cleared", 'document.querySelector("#workspace").hidden && ' + empty);
    assert.deepEqual(exceptions, []);
    assert.ok(requests.every(url => new URL(url).origin === input.origin));
    return;
  }
  if (input.projects) {
    const project = '#project-rows button[data-project-id="' + input.project + '"]';
    const item = '#project-content [data-work-item-id="' + input.work_item + '"]';
    const present = selector => 'document.querySelector(' + JSON.stringify(selector) + ') !== null';
    await keyboardActivate("#nav-projects");
    await waitFor("first project page", 'document.querySelectorAll("#project-rows button").length === 8');
    assert.ok(!await evaluate(present(project)));
    await click("#projects-next");
    await waitFor("second project page", present(project) + ' && document.querySelectorAll("#project-rows button").length === 1');
    assert.ok(!await evaluate('document.querySelector("#project-rows").textContent.includes("无成员资格的项目")'));
    await keyboardActivate(project);
    await waitFor("first work item page", 'document.querySelectorAll("#project-content li").length === 12');
    await click("#work-items-next");
    await waitFor("second work item page", 'document.querySelectorAll("#project-content li").length === 1');
    if (!await evaluate(present(item))) {
      await click("#work-items-previous");
      await waitFor("blocked item", present(item));
    }
    const selector = 'document.querySelector(' + JSON.stringify(item) + ')';
    assert.deepEqual(await evaluate(`[${selector}.querySelector("strong").textContent, ${selector}.querySelector("b")]`),
      ["<b>核对交付条件</b>", null]);
    assert.ok(await evaluate(`${selector}.textContent.includes("有未完成的依赖") && ${selector}.textContent.includes("优先级 高")`));
    await send("Emulation.setDeviceMetricsOverride", { width: 360, height: 750, deviceScaleFactor: 1, mobile: true });
    assert.ok(await evaluate('document.documentElement.scrollWidth <= 360 && !document.querySelector("#project-detail").hidden'));
    await click('#window-projects [data-action="minimize"]');
    assert.ok(await evaluate('document.querySelector("#project-content").textContent === "" && ' +
      'document.querySelector("#project-rows").textContent === "" && document.querySelector("#project-title").textContent === "工作项"'));
    writeSync(input.control_fd, Buffer.from("R"));
    const resumed = Buffer.alloc(1);
    assert.equal(readSync(input.control_fd, resumed), 1);
    assert.equal(resumed.toString(), "K");
    await click("#nav-projects");
    await waitFor("removed project after reopening", '!document.querySelector("#projects-previous").disabled && ' +
      '!document.querySelector("#projects-empty").hidden');
    assert.ok(await evaluate('!(' + present(project) + ') && document.querySelector("#project-detail").hidden'));
    await click("#projects-previous");
    await waitFor("remaining project directory", 'document.querySelectorAll("#project-rows button").length === 8');
    await click("#logout");
    await waitFor("cleared project view", 'document.querySelector("#workspace").hidden && !document.querySelector("#project-rows button")');
    assert.deepEqual(exceptions, []);
    assert.ok(requests.every(url => new URL(url).origin === input.origin));
    return;
  }
  if (input.chatSend) {
    await click("#nav-chat");
    await waitFor("writable conversation", 'document.querySelector("#chat-rows button") !== null');
    await click('#chat-rows button[data-conversation-id="' + input.conversation + '"]');
    await waitFor("enabled composer", '!document.querySelector("#chat-composer").hidden');
    await evaluate('document.querySelector("#chat-draft").value = "唯一一条交接消息"');
    await click("#chat-send");
    await waitFor("lost send receipt", 'document.querySelector("#chat-send-status").textContent.includes("待核实")');
    assert.ok(await evaluate('document.querySelector("#chat-draft").readOnly && ' +
      'document.querySelector("#chat-send").textContent === "核对并重试"'));
    writeSync(input.control_fd, Buffer.from("R"));
    const signal = Buffer.alloc(1);
    assert.equal(readSync(input.control_fd, signal), 1);
    assert.equal(signal.toString(), "K");
    const count = path => requests.filter(url => new URL(url).pathname === path).length;
    assert.equal(count("/api/chat-send"), 1);
    await click("#chat-send");
    await waitFor("authoritative send after retry", 'document.querySelector("#chat-send-status").textContent === "已记录发送回执。" && ' +
      'document.querySelector(".chat-entry p")?.textContent === "唯一一条交接消息"');
    assert.equal(count("/api/chat-prepare"), 1);
    assert.equal(count("/api/chat-send"), 2);
    assert.ok(await evaluate('document.querySelectorAll(".chat-entry").length === 1 && !document.querySelector("#chat-draft").value'));
    await click("#logout");
    await waitFor("closed Chat session", 'document.querySelector("#workspace").hidden');
    assert.deepEqual(exceptions, []);
    return;
  }
  if (input.chat) {
    await click("#nav-chat");
    await waitFor("first conversation directory page", 'document.querySelectorAll("#chat-rows button").length === 12');
    assert.equal(await evaluate('document.querySelector("#chat-rows td:nth-child(2)").textContent'), "私聊");
    await click("#chat-next");
    await waitFor("room on second directory page", 'document.querySelector("#chat-rows button")?.textContent === "现场排查"');
    await click("#chat-previous");
    await waitFor("earlier conversations", 'document.querySelectorAll("#chat-rows button").length === 12');
    await click("#chat-next");
    await waitFor("room on second directory page again", 'document.querySelector("#chat-rows button")?.textContent === "现场排查"');
    await click('#chat-rows button[data-conversation-id="' + input.conversation + '"]');
    await waitFor("first permitted message page", 'document.querySelectorAll(".chat-entry").length === 16');
    assert.ok(await evaluate('document.querySelector("#chat-composer").hidden'));
    assert.ok(await evaluate('document.activeElement.id === "chat-detail" && ' +
      'document.querySelector(".chat-entry p").textContent === "现场记录 1\\n<em>以文字显示</em>" && ' +
      '!document.querySelector(".chat-entry em")'));
    await click("#chat-history-next");
    await waitFor("later messages", 'document.querySelectorAll(".chat-entry").length === 2 && ' +
      'document.querySelector(".chat-entry p")?.textContent.startsWith("现场记录 17")');
    await send("Emulation.setDeviceMetricsOverride", {
      width: 360, height: 800, screenWidth: 360, screenHeight: 800, deviceScaleFactor: 1, mobile: true,
    });
    await waitFor("narrow Chat viewport", "window.innerWidth === 360");
    assert.equal(await evaluate("document.documentElement.scrollWidth"), 360);
    await keyboardActivate('#window-chat [data-action="minimize"]');
    assert.ok(await evaluate('document.querySelector("#chat-content").textContent === "" && ' +
      'document.querySelector("#chat-rows").textContent === "" && document.querySelector("#chat-title").textContent === "会话历史"'));
    writeSync(input.control_fd, Buffer.from("R"));
    const signal = Buffer.alloc(1);
    assert.equal(readSync(input.control_fd, signal), 1);
    assert.equal(signal.toString(), "K");
    await keyboardActivate("#nav-chat");
    await waitFor("removed room no longer visible", '!document.querySelector("#chat-empty").hidden');
    assert.ok(await evaluate('document.querySelector("#chat-detail").hidden && ' +
      'document.querySelector("#chat-content").textContent === ""'));
    const readOnly = ["/api/session", "/api/session-end", "/api/conversations", "/api/conversation-messages"];
    assert.ok(requests.map(url => new URL(url).pathname).filter(path => path.startsWith("/api/"))
      .every(path => readOnly.includes(path)));
    await click("#logout");
    await waitFor("closed Chat", 'document.querySelector("#workspace").hidden');
    assert.deepEqual(exceptions, []);
    return;
  }
  if (input.applicationCatalog) {
    await click("#nav-applications");
    await waitFor("first application page", 'document.querySelectorAll("#application-rows button").length === 8');
    await click("#applications-next");
    await waitFor("the last application page", 'document.querySelector("#application-rows button")?.textContent === "catalog-8"');
    await click("#applications-previous");
    await waitFor("return to the first application page", 'document.querySelectorAll("#application-rows button").length === 8');
    await click('#application-rows button[data-application-id="' + input.application + '"]');
    await waitFor("first release page", 'document.querySelectorAll(".release-list li").length === 8');
    assert.ok(await evaluate('document.activeElement.id === "application-detail" && document.querySelector(".release-list").textContent.includes("已选用")'));
    await click("#releases-next");
    await waitFor("second release page", 'document.querySelectorAll(".release-list li").length === 1 && document.querySelector(".release-list li strong").textContent === "9.0.0"');
    await send("Emulation.setDeviceMetricsOverride", {
      width: 360, height: 800, screenWidth: 360, screenHeight: 800, deviceScaleFactor: 1, mobile: true,
    });
    await waitFor("narrow catalog viewport", "window.innerWidth === 360");
    assert.equal(await evaluate("document.documentElement.scrollWidth"), 360);
    await click("#nav-tasks");
    await waitFor("another app stays open", 'document.querySelectorAll("#task-rows button").length > 0');
    await keyboardActivate('#window-applications [data-action="minimize"]');
    assert.ok(await evaluate('document.querySelector("#application-content").textContent === "" && document.querySelector("#application-rows").textContent === ""'));
    writeSync(input.control_fd, Buffer.from("R"));
    const signal = Buffer.alloc(1);
    assert.equal(readSync(input.control_fd, signal), 1);
    assert.equal(signal.toString(), "K");
    await keyboardActivate("#nav-applications");
    await waitFor("release scope checked again", 'document.querySelector("#application-content .error") !== null');
    assert.ok(await evaluate('document.querySelector(".release-list") === null && document.querySelectorAll("#task-rows button").length > 0'));
    await click("#logout");
    await waitFor("cleared catalog on disconnect", 'document.querySelector("#workspace").hidden && document.querySelector("#application-rows").textContent === ""');
    assert.deepEqual(exceptions, []);
    return;
  }
  if (input.navigateBack) {
    await click("#nav-node");
    await waitFor("populated inventory", 'document.querySelector("#node-view").textContent.includes(' + JSON.stringify(input.node) + ')');
    const history = await send("Page.getNavigationHistory");
    const prior = history.entries[history.currentIndex];
    await send("Page.navigate", { url: "about:blank" });
    await waitFor("navigation away", 'location.href === "about:blank"');
    await send("Page.navigateToHistoryEntry", { entryId: prior.id });
    await waitFor("unpaired page after Back", 'document.querySelector("#workspace")?.hidden && !document.querySelector("#pairing").hidden');
    assert.ok(await evaluate('document.querySelector("#principal").textContent === "" && document.querySelector("#node-view").textContent === ""'));
    assert.equal(await evaluate("location.origin"), input.origin);
    assert.deepEqual(exceptions, []);
    return;
  }
  if (input.endSession) {
    await click("#nav-node");
    await waitFor("populated inventory", 'document.querySelector("#node-view").textContent.includes(' + JSON.stringify(input.node) + ')');
    await click("#nav-tasks");
    await waitFor("visible task", 'document.querySelector("#task-rows button") !== null');
    await selectTask(input.task);
    await waitFor("authorized result", 'document.querySelector("#task-content").textContent.includes("成功")');
    const reads = requests.filter(url => new URL(url).pathname.startsWith("/api/")).length;
    writeSync(input.control_fd, Buffer.from("R"));
    if (input.realIdle) {
      await delay(270_000);
      assert.ok(await evaluate('!document.querySelector("#workspace").hidden && document.querySelector("#task-content").textContent.includes("成功")'));
      assert.equal(requests.filter(url => new URL(url).pathname.startsWith("/api/")).length, reads);
    }
    const signal = Buffer.alloc(1);
    assert.equal(readSync(input.control_fd, signal), 1);
    assert.equal(signal.toString(), "K");
    await waitFor("redaction after the native Shell session ends", 'document.querySelector("#workspace").hidden');
    assert.ok(await evaluate('document.querySelector("#node-view").textContent === "" && ' +
      'document.querySelector("#task-content").textContent === "" && document.querySelector("#task-rows").textContent === "" && ' +
      'document.querySelector("#principal").textContent === "" && !document.querySelector("#pairing").hidden'));
    assert.deepEqual(exceptions, []);
    return;
  }
  if (input.control_fd !== undefined) return verifyRevokedWindow();
  await send("Emulation.setDeviceMetricsOverride", {
    width: 1280, height: 860, screenWidth: 1280, screenHeight: 860, deviceScaleFactor: 1, mobile: false,
  });
  await click("#nav-node");
  await waitFor("authorized node", 'document.getElementById("node-view").textContent.includes(' +
    JSON.stringify(input.node) + ')');
  assert.ok(await evaluate('document.activeElement.id === "title-node" && ' +
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
  assert.ok(await evaluate(`document.querySelector("#window-node").getBoundingClientRect().right <=
    document.querySelector("#window-tasks").getBoundingClientRect().left &&
    document.querySelector("#active-app").textContent === "任务中心"`));
  assert.ok(await evaluate(`(() => {
    const tasks = document.querySelector("#window-tasks .window-body");
    tasks.scrollTop = tasks.scrollHeight;
    const independent = tasks.scrollTop > 0 && document.scrollingElement.scrollTop === 0 &&
      document.querySelector("#window-node .window-body").scrollTop === 0;
    tasks.scrollTop = 0;
    return independent;
  })()`));
  await click('#window-tasks [data-action="maximize"]');
  assert.ok(await evaluate('document.querySelector("#window-node").hidden && document.querySelector("#window-tasks").classList.contains("maximized")'));
  await click('#window-tasks [data-action="maximize"]');
  assert.ok(await evaluate('!document.querySelector("#window-node").hidden'));
  await click("#nav-node");
  assert.ok(await evaluate('document.activeElement.id === "title-node" && !document.querySelector("#window-tasks").hidden'));
  await click("#nav-tasks");
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
  await click('#window-tasks [data-action="minimize"]');
  assert.ok(await evaluate(`document.querySelector("#nav-tasks").dataset.state === "minimized" &&
    document.querySelector("#task-content").textContent === "" && document.activeElement.id === "title-node"`));
  await click("#nav-tasks");
  await waitFor("restored pending Task", 'document.querySelector(".task-cancel button") !== null');
  await click('#window-tasks [data-action="close"]');
  assert.ok(await evaluate('document.querySelector("#nav-tasks").dataset.state === "closed" && document.querySelector("#task-rows").textContent === ""'));
  await click("#nav-tasks");
  await waitFor("reopened task list", 'document.querySelectorAll("#task-rows button").length === 20');
  assert.ok(await evaluate('document.querySelector("#task-detail").hidden'));
  await selectTask(input.pending);
  await waitFor("same pending Task", 'document.querySelector(".task-cancel button") !== null');
  const cancelRequests = () => requests.filter(url => new URL(url).pathname === "/api/cancel-pending-task").length;
  assert.equal(cancelRequests(), 0);
  await click(".task-cancel button");
  await waitFor("authoritative cancellation", 'document.getElementById("task-content").textContent.includes("已取消")');
  assert.ok(await evaluate('!document.querySelector(".task-cancel button") && document.activeElement.id === "task-detail"'));
  assert.equal(cancelRequests(), 1);
  await click("#logout");
  await waitFor("cleared session", 'document.getElementById("workspace").hidden && ' +
    'document.getElementById("task-content").textContent === ""');
  assert.ok(await evaluate('document.querySelector("#node-view").textContent === "" && document.querySelectorAll(".app-window:not([hidden])").length === 0'));
  assert.deepEqual(exceptions, []);
  assert.ok(requests.length > 6 && requests.every((url) => new URL(url).origin === input.origin));
}

try {
  await test();
} finally {
  chrome.kill("SIGTERM");
}
