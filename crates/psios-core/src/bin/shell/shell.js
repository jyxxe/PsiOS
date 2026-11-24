"use strict";

const byId = (id) => document.getElementById(id);
const pairing = byId("pairing");
const workspace = byId("workspace");
const code = byId("code");
const status = byId("status");
const heading = byId("view-title");
const nodeView = byId("node-view");
const taskView = byId("task-view");
const taskRows = byId("task-rows");
const taskDetail = byId("task-detail");
const taskContent = byId("task-content");
const previous = byId("previous");
const next = byId("next");

let bearer = null;
let view = "node";
let update = 0;
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
  update += 1;
  detailUpdate += 1;
  workspace.hidden = true;
  nodeView.replaceChildren();
  taskRows.replaceChildren();
  taskContent.replaceChildren();
  byId("principal").textContent = "";
  message(status, "");
  code.value = "";
  pairing.hidden = false;
  message(byId("pair-message"), text, error);
  code.focus();
}

async function request(path, body, token = bearer) {
  const headers = { "Content-Type": "application/json" };
  if (token) headers.Authorization = "Bearer " + token;
  let response;
  try {
    response = await fetch(path, {
      method: "POST",
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
  const result = await response.json();
  if (!response.ok) {
    const description = result.error?.message || "请求失败";
    const error = new Error(description);
    error.code = result.error?.code;
    throw error;
  }
  return result;
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
  const section = card(title, [
    ["可用空间", capacity(storage.available_bytes)],
    ["总容量", capacity(storage.capacity_bytes)],
    ["预留空间", capacity(storage.reserve_bytes)],
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
  if (storage.below_reserve) {
    section.append(element("p", "可用空间低于预留阈值。", "reserve-low"));
  }
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
  Succeeded: "成功", Failed: "失败", Cancelled: "已取消", Paused: "已暂停",
};

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

async function load(target = cursors, task = selectedTask) {
  const thisUpdate = ++update;
  detailUpdate += 1;
  selectedTask = null;
  taskDetail.hidden = true;
  taskContent.replaceChildren();
  previous.disabled = next.disabled = true;
  taskRows.replaceChildren();
  byId("tasks-empty").hidden = true;
  nodeView.replaceChildren();
  message(status, "正在读取…");
  try {
    if (view === "node") {
      const result = await request("/api/node", {});
      if (thisUpdate !== update) return;
      renderNode(result);
    } else {
      const result = await request("/api/tasks", { cursor: target.at(-1) });
      if (thisUpdate !== update) return;
      cursors = target;
      nextCursor = result.next_cursor;
      renderTasks(result.items);
      previous.textContent = cursors.length === 2 ? "返回首页" : "上一页";
      previous.disabled = cursors.length === 1;
      next.disabled = !nextCursor;
      if (task && result.items.some((item) => item.resource_id === task)) inspectTask(task);
    }
    message(status, "");
  } catch (error) {
    if (thisUpdate !== update) return;
    if (error.code === "CursorExpired") {
      cursors = [null];
      nextCursor = null;
      message(status, "任务列表快照已过期。刷新后将从第一页读取。", true);
    } else {
      message(status, error.message, true);
    }
  }
}

function navigate(page) {
  view = page;
  const isNode = view === "node";
  nodeView.hidden = !isNode;
  taskView.hidden = isNode;
  heading.textContent = isNode ? "节点概览" : "任务";
  for (const name of ["node", "tasks"]) {
    const button = byId("nav-" + name);
    if (view === name) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
  heading.focus();
  load([null], null);
}

byId("pair-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const connect = byId("connect");
  if (connect.disabled) return;
  connect.disabled = true;
  message(byId("pair-message"), "正在连接…");
  const submitted = code.value.trim();
  code.value = "";
  try {
    const session = await request("/api/session", { code: submitted }, null);
    bearer = session.token;
    byId("principal").textContent = session.principal_id;
    pairing.hidden = true;
    workspace.hidden = false;
    navigate("node");
  } catch (error) {
    message(byId("pair-message"), error.message, true);
    code.focus();
  } finally {
    connect.disabled = false;
  }
});

byId("nav-node").addEventListener("click", () => navigate("node"));
byId("nav-tasks").addEventListener("click", () => navigate("tasks"));
byId("refresh").addEventListener("click", () => load());
byId("close-task").addEventListener("click", () => {
  const current = taskRows.querySelector('button[aria-pressed="true"]');
  detailUpdate += 1;
  selectedTask = null;
  taskDetail.hidden = true;
  taskContent.replaceChildren();
  current?.setAttribute("aria-pressed", "false");
  current?.focus();
});
previous.addEventListener("click", () => load(cursors.slice(0, -1), null));
next.addEventListener("click", () => load([...cursors, nextCursor], null));
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
