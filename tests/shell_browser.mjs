// Exercise the embedded pages through Chrome's private DevTools pipe.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
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

async function test() {
  await send("Browser.getVersion", {}, 35_000);
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
  await click('#task-rows button[data-task-id="' + input.task + '"]');
  await waitFor("visible task result", 'document.getElementById("task-content").textContent.includes("成功") && ' +
    'document.getElementById("task-content").textContent.includes(' + JSON.stringify(input.task) + ')');
  assert.ok(await evaluate('document.activeElement.id === "task-detail" && ' +
    'document.getElementById("task-detail").getBoundingClientRect().bottom > 0 && ' +
    'document.getElementById("task-detail").getBoundingClientRect().top < window.innerHeight'));
  assert.equal(await evaluate("document.documentElement.scrollWidth"), 360);
  await click("#preview-artifact");
  await waitFor("verified artifact preview", 'document.getElementById("artifact-json")?.textContent.includes(' +
    JSON.stringify(input.node) + ')');
  assert.ok(await evaluate('document.activeElement.id === "artifact-json"'));
  await click("#close-task");
  assert.ok(await evaluate('document.getElementById("task-detail").hidden && ' +
    'document.activeElement.dataset.taskId === ' + JSON.stringify(input.task)));

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
