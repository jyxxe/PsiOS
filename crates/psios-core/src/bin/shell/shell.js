"use strict";

const byId = (id) => document.getElementById(id);
const pairing = byId("pairing");
const workspace = byId("workspace");
const code = byId("code");
const status = byId("status");
const nodeView = byId("node-view");
const taskRows = byId("task-rows");
const taskDetail = byId("task-detail");
const taskContent = byId("task-content");
const previous = byId("previous");
const next = byId("next");

let bearer = null;
let sessionRequests = new AbortController();
let canCancelPending = false;
const apps = new Map(["node", "tasks"].map(id => [id, {
  window: byId("window-" + id), title: byId("title-" + id),
  dock: byId("nav-" + id), status: byId("status-" + id), state: "closed",
}]));
let active = null;
let maximized = null;
let nodeUpdate = 0;
let taskUpdate = 0;
let detailUpdate = 0;
let cursors = [null];
let nextCursor = null;
let selectedTask = null;

function message(element, text, error = false) {
  element.textContent = text;
  element.classList.toggle("error", error);
}

function leave(text, error = false) {
  bearer = null;
  sessionRequests.abort();
  sessionRequests = new AbortController();
  canCancelPending = false;
  workspace.hidden = true;
  for (const app of apps.values()) app.state = "closed";
  active = maximized = null;
  cursors = [null];
  nextCursor = selectedTask = null;
  updateDesktop();
  byId("principal").textContent = "";
  message(status, "");
  code.value = "";
  pairing.hidden = false;
  message(byId("pair-message"), text, error);
  code.focus();
}

async function observeSession(token) {
  try {
    await post("/api/session-end", {}, token);
  } catch {
    // EOF and connection loss also end the local browser's view of this session.
  }
  if (bearer === token) leave("本机会话已结束。请在终端重新启动 Shell。");
}

async function post(path, body, token = bearer) {
  const headers = { "Content-Type": "application/json" };
  if (token) headers.Authorization = "Bearer " + token;
  let response;
  try {
    response = await fetch(path, {
      method: "POST",
      signal: sessionRequests.signal,
      headers,
      body: JSON.stringify(body),
      cache: "no-store",
      redirect: "error",
      credentials: "omit",
    });
  } catch {
    throw new Error("本机 Shell 已断开。请在终端重新启动。");
  }
  if (response.status === 401 && token && token === bearer) {
    leave("会话已失效。请在终端重新启动 Shell。", true);
  }
  if (!response.ok) {
    const result = await response.json();
    const description = result.error?.message || "请求失败";
    const error = new Error(description);
    error.code = result.error?.code;
    throw error;
  }
  return response;
}

async function request(path, body, token = bearer) {
  return (await post(path, body, token)).json();
}

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function fields(rows) {
  const list = element("dl");
  for (const [name, value, monospace] of rows) {
    list.append(element("dt", name), element("dd", value ?? "—", monospace ? "mono" : ""));
  }
  return list;
}

function card(title, rows) {
  const section = element("section", undefined, "card");
  section.append(element("h2", title), fields(rows));
  return section;
}

function capacity(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value < 1024) return value + " B";
  const units = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
  const power = Math.min(units.length, Math.floor(Math.log2(value) / 10));
  return (value / 1024 ** power).toLocaleString("zh-CN", { maximumFractionDigits: 2 }) +
    " " + units[power - 1];
}

function storageCard(title, storage) {
  const inodeFloor = storage.reserve_inodes == null ? null : BigInt(storage.reserve_inodes);
  const freeInodes = storage.inode_accounting === "Observed" ? BigInt(storage.available_inodes) : null;
  const inodeStatus = storage.inode_accounting === "Unavailable" ? "文件系统未提供" : "未报告";
  const section = card(title, [
    ["可用空间", capacity(storage.available_bytes)],
    ["总容量", capacity(storage.capacity_bytes)],
    ["预留空间", capacity(storage.reserve_bytes)],
    ["可用 inode", freeInodes?.toLocaleString("zh-CN") ?? inodeStatus],
    ["预留 inode", inodeFloor?.toLocaleString("zh-CN") ?? "未报告"],
  ]);
  const size = Number(storage.capacity_bytes);
  if (size > 0) {
    const meter = element("meter", undefined, "reserve");
    meter.max = size;
    meter.value = Number(storage.available_bytes);
    meter.low = Number(storage.reserve_bytes);
    meter.optimum = size;
    meter.setAttribute("aria-label", title + "可用空间");
    section.append(meter);
  }
  const warnings = [];
  if (BigInt(storage.available_bytes) < BigInt(storage.reserve_bytes)) {
    warnings.push("可用字节已低于预留空间。");
  }
  if (inodeFloor !== null && inodeFloor > 0n) {
    if (freeInodes === null) warnings.push("无法核对本文件系统配置的 inode 预留数量。");
    else if (freeInodes < inodeFloor) warnings.push("可用 inode 已低于预留数量。");
  }
  if (storage.read_only) warnings.push("此文件系统以只读方式挂载。");
  for (const warning of warnings) section.append(element("p", warning, "reserve-low"));
  return section;
}

function renderNode(result) {
  const { reference, state, queue, authority_storage: storage } = result;
  const grid = element("div", undefined, "node-grid");
  const lifecycle = { Ready: "就绪" };
  const scheduling = { Enabled: "已启用", Cordoned: "已暂停接收" };
  const node = card("本机节点", [
    ["节点 ID", reference.resource_id, true],
    ["状态", lifecycle[state.lifecycle] ?? state.lifecycle],
    ["调度", scheduling[state.scheduling] ?? state.scheduling],
    ["启动 ID", state.boot_id, true],
    ["工作线程", state.configuration.worker_count],
    ["队列上限", state.configuration.queue_limit],
  ]);
  node.classList.add("content-card");
  const phases = { Queued: "排队", Running: "执行中", Done: "已处理" };
  const counts = Object.entries(queue).map(([name, count]) => [phases[name] ?? name, count]);
  const queueCard = card("执行队列", counts);
  if (counts.length === 0) queueCard.append(element("p", "队列为空。"));
  queueCard.classList.add("content-card");
  grid.append(node, storageCard("状态存储", storage.state),
    storageCard("见证存储", storage.witness), queueCard);
  nodeView.replaceChildren(grid);
}

const lifecycles = {
  WaitingApproval: "等待审批", Queued: "已排队", Running: "执行中",
  Succeeded: "成功", Failed: "失败", Cancelling: "正在取消", Cancelled: "已取消", Paused: "已暂停",
};

function artifactView(taskId, references) {
  const section = element("section", undefined, "artifacts");
  const label = element("label", "任务制品");
  label.htmlFor = "artifact-reference";
  const choices = element("select");
  choices.id = "artifact-reference";
  for (const reference of references) {
    choices.append(element("option", reference.resource_id));
  }
  const preview = element("button", "预览");
  preview.id = "preview-artifact";
  preview.type = "button";
  const download = element("button", "下载");
  download.id = "download-artifact";
  download.type = "button";
  const controls = element("div", undefined, "artifact-controls");
  controls.append(choices, preview, download);
  const referenceBox = element("pre", undefined, "artifact-reference");
  referenceBox.setAttribute("aria-label", "固定制品引用");
  const notice = element("p", "", "message");
  const content = element("pre", undefined, "artifact-json");
  content.id = "artifact-json";
  content.hidden = true;
  content.tabIndex = 0;
  content.setAttribute("aria-label", "已核验的 JSON 内容");
  content.setAttribute("aria-live", "off");
  section.append(label, controls, referenceBox, notice, content);

  function clear() {
    referenceBox.textContent = JSON.stringify(references[choices.selectedIndex], null, 2);
    message(notice, "");
    content.hidden = true;
    content.textContent = "";
  }
  choices.addEventListener("change", clear);
  clear();

  async function readArtifact(save) {
    if (choices.disabled) return;
    clear();
    const reference = references[choices.selectedIndex];
    preview.disabled = download.disabled = choices.disabled = true;
    message(notice, "正在核对制品…");
    try {
      const response = await post("/api/task-artifact", { task_id: taskId, reference, download: save });
      if (save) {
        const bytes = await response.arrayBuffer();
        const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
        const hex = Array.from(hash, (byte) => byte.toString(16).padStart(2, "0")).join("");
        if ("sha256:" + hex !== reference.digest) throw new Error("下载内容与所选制品的 SHA-256 不符。");
        if (!section.isConnected) return;
        const url = URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
        const link = element("a");
        link.href = url;
        link.download = "psios-" + hex + ".bin";
        section.append(link);
        try {
          link.click();
        } finally {
          link.remove();
          setTimeout(() => URL.revokeObjectURL(url), 0);
        }
        message(notice, "摘要已核验，已提交浏览器下载。");
      } else {
        const result = await response.json();
        if (!section.isConnected) return;
        content.textContent = result.content_json;
        content.hidden = false;
        message(notice, "已核对固定版本的 SHA-256 摘要。");
        if (active === "tasks") content.focus();
      }
    } catch (error) {
      if (!section.isConnected) return;
      if (error.code === "QuotaExceeded") {
        message(notice, save ? "制品超过 8 MiB。可用 psiosctl artifact 的 --max-bytes 选项导出。"
          : "制品超过 64 KiB。可以下载后查看。", true);
      } else {
        message(notice, error.message, true);
      }
    } finally {
      preview.disabled = download.disabled = choices.disabled = false;
    }
  }
  preview.addEventListener("click", () => readArtifact(false));
  download.addEventListener("click", () => readArtifact(true));
  return section;
}

function historyView(task) {
  const panel = element("details", undefined, "task-history");
  const summary = element("summary", "执行历史");
  const boundary = element("p", "截至任务修订 " + task.task_revision, "history-boundary");
  const list = element("ol");
  list.tabIndex = -1;
  list.setAttribute("aria-label", "当前页的历史修订");
  const notice = element("p", "", "message");
  notice.setAttribute("role", "status");
  const controls = element("div", undefined, "history-controls");
  const back = element("button", "上一页");
  const forward = element("button", "下一页");
  back.type = forward.type = "button";
  controls.append(back, forward);
  controls.hidden = true;
  panel.append(summary, boundary, list, notice, controls);
  let pages = [null];
  let nextAfter = null;
  let generation = 0;

  async function loadPage(target, focus = false) {
    const generationAtRequest = ++generation;
    list.replaceChildren();
    controls.hidden = true;
    message(notice, "正在读取历史…");
    try {
      const page = await request("/api/task-history", {
        id: task.task_id, through_version: task.task_revision, after_version: target.at(-1),
      });
      if (!panel.isConnected || generationAtRequest !== generation) return;
      pages = target;
      nextAfter = page.next_after_version;
      for (const entry of page.items) {
        const row = element("li");
        const state = lifecycles[entry.lifecycle] ?? entry.lifecycle;
        row.append(element("strong", entry.reference.version + " · " + state));
        const time = element("time", entry.at);
        time.dateTime = entry.at;
        row.append(time);
        const properties = [["结果确认", entry.outcome_knowledge, true]];
        if (entry.run_id) properties.push(["Run ID", entry.run_id, true]);
        for (const ref of entry.operation_refs ?? []) {
          properties.push(["Operation", ref.resource_id + " · " + ref.version, true]);
        }
        if (entry.result_availability) properties.push(["制品状态", entry.result_availability, true]);
        row.append(fields(properties));
        for (const condition of entry.conditions ?? []) {
          row.append(element("p", condition.message || condition.reason || condition.type));
        }
        list.append(row);
      }
      message(notice, page.items.length ? "" : "此修订范围内没有历史记录。");
      back.disabled = pages.length === 1;
      forward.disabled = !nextAfter;
      controls.hidden = back.disabled && forward.disabled;
      if (focus && active === "tasks") list.focus();
    } catch (error) {
      if (!panel.isConnected || generationAtRequest !== generation) return;
      message(notice, error.message, true);
    }
  }

  panel.addEventListener("toggle", () => {
    if (panel.open) {
      loadPage([null]);
    } else {
      generation += 1;
      list.replaceChildren();
      controls.hidden = true;
      message(notice, "");
    }
  });
  back.addEventListener("click", () => loadPage(pages.slice(0, -1), true));
  forward.addEventListener("click", () => loadPage([...pages, nextAfter], true));
  return panel;
}

function pendingCancellation(task) {
  const controls = element("div", undefined, "task-cancel");
  const button = element("button", "取消待执行任务");
  button.type = "button";
  const notice = element("p", "", "message");
  notice.setAttribute("role", "status");
  controls.append(button, notice);
  button.addEventListener("click", async () => {
    if (button.disabled) return;
    button.disabled = true;
    message(notice, "正在提交取消请求…");
    try {
      const result = await request("/api/cancel-pending-task", {
        id: task.task_id, expected_resource_version: task.task_revision,
      });
      if (!controls.isConnected) return;
      renderTask(result);
      if (active === "tasks") taskDetail.focus();
    } catch (error) {
      if (!controls.isConnected) return;
      if (error.code === "Conflict" || error.code === "PreconditionFailed") {
        message(notice, "任务状态已改变。请刷新后查看最新修订。", true);
      } else if (error.code === "PermissionDenied") {
        message(notice, "当前主体没有取消此任务的权限。", true);
      } else {
        message(notice, "取消结果尚未确认。请刷新查看任务状态。", true);
      }
    }
  });
  return controls;
}

function renderTask(task) {
  taskContent.replaceChildren(fields([
    ["ID", task.task_id, true],
    ["当前修订", task.task_revision, true],
    ["状态", lifecycles[task.lifecycle] ?? task.lifecycle],
    ["结果确认", task.outcome_knowledge, true],
    ["Run ID", task.current_run_id, true],
  ]));
  if (task.conditions?.length) {
    const list = element("ul", undefined, "conditions");
    for (const condition of task.conditions) {
      list.append(element("li", condition.message || condition.code || condition.type));
    }
    taskContent.append(list);
  }
  if (canCancelPending && ["Queued", "WaitingApproval"].includes(task.lifecycle)) {
    taskContent.append(pendingCancellation(task));
  }
  taskContent.append(historyView(task));
  const references = task.result?.artifact_refs;
  if (references?.length) taskContent.append(artifactView(task.task_id, references));
}

async function inspectTask(id, focus = false) {
  const thisDetail = ++detailUpdate;
  selectedTask = id;
  for (const button of taskRows.querySelectorAll("button")) {
    button.setAttribute("aria-pressed", String(button.dataset.taskId === id));
  }
  taskDetail.hidden = false;
  taskContent.replaceChildren(element("p", "正在读取…"));
  if (focus) taskDetail.focus();
  try {
    const task = await request("/api/task", { id });
    if (thisDetail === detailUpdate) renderTask(task);
  } catch (error) {
    if (thisDetail !== detailUpdate) return;
    taskContent.replaceChildren(element("p", error.message, "message error"));
  }
}

function renderTasks(items) {
  const rows = document.createDocumentFragment();
  for (const task of items) {
    const row = element("tr");
    const field = element("td");
    const button = element("button", task.resource_id, "mono");
    button.type = "button";
    button.dataset.taskId = task.resource_id;
    button.setAttribute("aria-pressed", "false");
    button.addEventListener("click", () => inspectTask(task.resource_id, true));
    field.append(button);
    row.append(field, element("td", task.version, "mono"));
    rows.append(row);
  }
  taskRows.replaceChildren(rows);
  byId("tasks-empty").hidden = items.length > 0;
}

async function loadNode() {
  const revision = ++nodeUpdate;
  const notice = apps.get("node").status;
  nodeView.replaceChildren();
  message(notice, "正在读取…");
  try {
    const result = await request("/api/node", {});
    if (revision !== nodeUpdate) return;
    renderNode(result);
    message(notice, "");
  } catch (error) {
    if (revision === nodeUpdate) message(notice, error.message, true);
  }
}

function clearTaskView() {
  detailUpdate += 1;
  taskDetail.hidden = true;
  taskContent.replaceChildren();
  taskRows.replaceChildren();
  byId("tasks-empty").hidden = true;
  previous.disabled = next.disabled = true;
}

async function loadTasks(target = cursors, task = selectedTask) {
  const revision = ++taskUpdate;
  const notice = apps.get("tasks").status;
  clearTaskView();
  message(notice, "正在读取…");
  try {
    const result = await request("/api/tasks", { cursor: target.at(-1) });
    if (revision !== taskUpdate) return;
    cursors = target;
    nextCursor = result.next_cursor;
    renderTasks(result.items);
    previous.textContent = cursors.length === 2 ? "返回首页" : "上一页";
    previous.disabled = cursors.length === 1;
    next.disabled = !nextCursor;
    selectedTask = null;
    if (task && result.items.some((item) => item.resource_id === task)) inspectTask(task);
    message(notice, "");
  } catch (error) {
    if (revision !== taskUpdate) return;
    if (error.code === "CursorExpired") {
      cursors = [null];
      nextCursor = selectedTask = null;
      message(notice, "任务列表快照已过期。刷新后将从第一页读取。", true);
    } else {
      message(notice, error.message, true);
    }
  }
}

function showWindow(id, app, shown) {
  if (app.window.hidden === !shown) return;
  app.window.hidden = !shown;
  if (shown) {
    if (id === "node") loadNode();
    else loadTasks();
    return;
  }
  if (id === "node") {
    nodeUpdate += 1;
    nodeView.replaceChildren();
  } else {
    taskUpdate += 1;
    clearTaskView();
  }
  message(app.status, "");
}

function updateDesktop() {
  let visible = 0;
  for (const [id, app] of apps) {
    const shown = app.state === "open" && (!maximized || maximized === id);
    showWindow(id, app, shown);
    if (shown) visible += 1;
    app.window.classList.toggle("focused", id === active);
    app.window.classList.toggle("maximized", id === maximized);
    app.dock.dataset.state = app.state;
    app.dock.setAttribute("aria-expanded", String(shown));
    const stateLabel = { open: "已打开", minimized: "已最小化", closed: "已关闭" };
    app.dock.setAttribute("aria-label", app.title.textContent + "，" + stateLabel[app.state]);
    const maximize = app.window.querySelector('[data-action="maximize"]');
    maximize.textContent = maximized === id ? "还原" : "最大化";
    maximize.setAttribute("aria-pressed", String(maximized === id));
  }
  byId("desktop").classList.toggle("has-maximized", maximized !== null);
  byId("empty-desktop").hidden = visible !== 0;
  byId("active-app").textContent = active ? apps.get(active).title.textContent : "桌面";
}

function focusApp(id) {
  if (active === id) return;
  active = id;
  updateDesktop();
}

function openWindow(id) {
  const app = apps.get(id);
  if (maximized !== id) maximized = null;
  app.state = "open";
  active = id;
  updateDesktop();
  app.title.focus();
}

function hideWindow(id, state) {
  const app = apps.get(id);
  app.state = state;
  if (maximized === id) maximized = null;
  if (id === "tasks" && state === "closed") {
    cursors = [null];
    nextCursor = selectedTask = null;
  }
  if (active === id) active = Array.from(apps.keys()).find(key => apps.get(key).state === "open") ?? null;
  updateDesktop();
  if (!workspace.hidden) {
    if (active) apps.get(active).title.focus();
    else app.dock.focus();
  }
}

for (const [id, app] of apps) {
  const controls = byId("window-tools").content.cloneNode(true);
  controls.querySelector('[data-action="refresh"]').addEventListener("click", () => {
    if (id === "node") loadNode();
    else loadTasks();
  });
  controls.querySelector('[data-action="close"]').addEventListener("click", () => hideWindow(id, "closed"));
  controls.querySelector('[data-action="minimize"]').addEventListener("click", () => hideWindow(id, "minimized"));
  controls.querySelector('[data-action="maximize"]').addEventListener("click", () => {
    active = id;
    maximized = maximized === id ? null : id;
    updateDesktop();
  });
  app.window.querySelector("header").append(controls);
  app.window.addEventListener("focusin", () => focusApp(id));
  app.window.addEventListener("pointerdown", () => focusApp(id));
  app.dock.addEventListener("click", () => openWindow(id));
}

byId("pair-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const connect = byId("connect");
  if (connect.disabled) return;
  connect.disabled = true;
  message(byId("pair-message"), "正在连接…");
  const pairingSignal = sessionRequests.signal;
  const submitted = code.value.trim();
  code.value = "";
  try {
    const session = await request("/api/session", { code: submitted }, null);
    if (pairingSignal.aborted) return;
    bearer = session.token;
    void observeSession(bearer);
    canCancelPending = session.cancel_pending_tasks === true;
    byId("principal").textContent = session.principal_id;
    pairing.hidden = true;
    workspace.hidden = false;
    updateDesktop();
    apps.get("node").dock.focus();
  } catch (error) {
    if (pairingSignal.aborted) return;
    message(byId("pair-message"), error.message, true);
    code.focus();
  } finally {
    connect.disabled = false;
  }
});

byId("close-task").addEventListener("click", () => {
  const current = taskRows.querySelector('button[aria-pressed="true"]');
  detailUpdate += 1;
  selectedTask = null;
  taskDetail.hidden = true;
  taskContent.replaceChildren();
  current?.setAttribute("aria-pressed", "false");
  current?.focus();
});
previous.addEventListener("click", () => loadTasks(cursors.slice(0, -1), null));
next.addEventListener("click", () => loadTasks([...cursors, nextCursor], null));
byId("logout").addEventListener("click", async () => {
  if (!bearer) return;
  const button = byId("logout");
  button.disabled = true;
  try {
    await request("/api/logout", {});
    leave("会话已断开。");
  } catch (error) {
    if (bearer) message(status, error.message, true);
  } finally {
    button.disabled = false;
  }
});

window.addEventListener("pagehide", () => {
  leave("请在终端重新启动 Shell。");
});
