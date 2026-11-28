export function chatView(request, byId, element, message) {
  const rows = byId("chat-rows");
  const detail = byId("chat-detail");
  const content = byId("chat-content");
  const title = byId("chat-title");
  const notice = byId("status-chat");
  const previous = byId("chat-previous");
  const next = byId("chat-next");
  const earlier = byId("chat-history-previous");
  const later = byId("chat-history-next");

  let listRevision = 0;
  let historyRevision = 0;
  let directory = [null];
  let nextConversation = null;
  let history = [null];
  let nextMessage = null;
  let selection = null;
  const composer = chatComposer(request, byId, message, id => {
    if (selection === id) void loadHistory(id, title.textContent, history);
  });

  function clearHistory() {
    historyRevision += 1;
    detail.hidden = true;
    composer.hide();
    title.textContent = "会话历史";
    content.replaceChildren();
    earlier.disabled = later.disabled = true;
  }

  function hide() {
    listRevision += 1;
    rows.replaceChildren();
    byId("chat-empty").hidden = true;
    previous.disabled = next.disabled = true;
    clearHistory();
    message(notice, "");
  }

  function reset() {
    directory = history = [null];
    nextConversation = nextMessage = selection = null;
    hide();
  }

  function renderHistory(items) {
    const thread = element("ol", undefined, "chat-thread");
    for (const item of items) {
      const entry = element("li", undefined, "chat-entry");
      const at = element("time", item.accepted_at);
      at.dateTime = item.accepted_at;
      entry.append(element("strong", item.from, "mono"), at,
                   element("p", item.text ?? "当前消息类型无法显示。"));
      thread.append(entry);
    }
    content.replaceChildren(items.length ? thread : element("p", "当前没有可见消息。", "empty"));
  }

  async function loadHistory(id, name, pages = [null], focus = false) {
    const revision = ++historyRevision;
    if (selection !== id) composer.hide();
    selection = id;
    for (const button of rows.querySelectorAll("button")) {
      button.setAttribute("aria-pressed", String(button.dataset.conversationId === id));
    }
    title.textContent = name;
    detail.hidden = false;
    content.replaceChildren(element("p", "正在读取消息…"));
    earlier.disabled = later.disabled = true;
    if (focus) detail.focus();
    try {
      const page = await request("/api/conversation-messages", { id, cursor: pages.at(-1) });
      if (revision !== historyRevision) return;
      history = pages;
      nextMessage = page.next_cursor;
      renderHistory(page.items);
      composer.show(id);
      earlier.disabled = history.length === 1;
      later.disabled = !nextMessage;
    } catch (error) {
      if (revision !== historyRevision) return;
      if (error.code === "CursorExpired" && pages.length > 1) {
        await loadHistory(id, name, [null], focus);
        return;
      }
      composer.hide();
      content.replaceChildren(element("p", error.message, "message error"));
    }
  }

  async function loadList(pages = directory, selected = selection, savedHistory = history) {
    hide();
    const revision = listRevision;
    message(notice, "正在读取会话…");
    try {
      const page = await request("/api/conversations", { after_id: pages.at(-1) });
      if (revision !== listRevision) return;
      directory = pages;
      nextConversation = page.next_after_id;
      next.disabled = !nextConversation;
      previous.disabled = directory.length === 1;
      const result = document.createDocumentFragment();
      for (const conversation of page.items) {
        const name = element("button", conversation.name);
        name.type = "button";
        name.dataset.conversationId = conversation.id;
        name.setAttribute("aria-pressed", "false");
        name.addEventListener("click", () => loadHistory(conversation.id, conversation.name, [null], true));
        const cell = element("td");
        cell.append(name);
        const kind = conversation.kind === "DirectMessage" ? "私聊" : "房间";
        const label = conversation.lifecycle === "Archived" ? kind + " · 已归档" : kind;
        const row = element("tr");
        row.append(cell, element("td", label));
        result.append(row);
      }
      rows.append(result);
      byId("chat-empty").hidden = page.items.length !== 0;
      message(notice, "");
      selection = null;
      const restored = page.items.find(conversation => conversation.id === selected);
      if (restored) void loadHistory(restored.id, restored.name, savedHistory);
    } catch (error) {
      if (revision === listRevision) message(notice, error.message, true);
    }
  }

  previous.addEventListener("click", () => loadList(directory.slice(0, -1), null, [null]));
  next.addEventListener("click", () => loadList([...directory, nextConversation], null, [null]));
  earlier.addEventListener("click", () => loadHistory(selection, title.textContent, history.slice(0, -1)));
  later.addEventListener("click", () => loadHistory(selection, title.textContent, [...history, nextMessage]));
  byId("close-chat").addEventListener("click", () => {
    const button = rows.querySelector('button[aria-pressed="true"]');
    selection = null;
    history = [null];
    clearHistory();
    button?.setAttribute("aria-pressed", "false");
    button?.focus();
  });
  return {
    show: loadList, hide,
    reset() { reset(); composer.reset(); },
    capability: composer.capability,
    refresh: () => loadList([null], selection, [null]),
  };
}


function chatComposer(request, byId, message, committed) {
  const form = byId("chat-composer");
  const draft = byId("chat-draft");
  const send = byId("chat-send");
  const forget = byId("chat-forget");
  const status = byId("chat-send-status");
  const unconfirmed = new Map();
  let enabled = false;
  let conversation = null;
  let revision = 0;
  let busy = false;
  let prepared = null;

  function update() {
    draft.readOnly = busy || prepared !== null;
    send.disabled = busy;
    send.textContent = prepared ? "核对并重试" : "发送";
    forget.hidden = prepared === null;
    forget.disabled = busy;
  }

  function hide() {
    revision += 1;
    conversation = null;
    busy = false;
    prepared = null;
    draft.value = "";
    form.hidden = true;
    message(status, "");
    update();
  }

  function show(id) {
    if (!enabled || conversation === id) return;
    hide();
    conversation = id;
    form.hidden = false;
    if (unconfirmed.has(id)) message(status, "上次发送结果待核实。请先查看会话历史。", true);
  }

  form.addEventListener("submit", async event => {
    event.preventDefault();
    if (busy || !conversation) return;
    const id = conversation;
    const attempt = ++revision;
    busy = true;
    update();
    try {
      if (!prepared) {
        if (new TextEncoder().encode(draft.value).length > 4096) {
          message(status, "消息最多包含 4096 个 UTF-8 字节。", true);
          return;
        }
        message(status, "正在检查消息…");
        const reply = await request("/api/chat-prepare", { id, text: draft.value });
        if (attempt !== revision) return;
        prepared = reply.prepared;
        update();
      }
      unconfirmed.set(id, attempt);
      message(status, "正在提交消息…");
      await request("/api/chat-send", { prepared });
      if (unconfirmed.get(id) === attempt) unconfirmed.delete(id);
      if (attempt !== revision) return;
      prepared = null;
      draft.value = "";
      message(status, "已记录发送回执。");
      committed(id);
    } catch (error) {
      if (attempt !== revision) return;
      const note = prepared ? "发送结果待核实。重试会复用同一请求。" : "";
      message(status, note ? error.message + " " + note : error.message, true);
    } finally {
      if (attempt === revision) {
        busy = false;
        update();
      }
    }
  });

  forget.addEventListener("click", () => {
    prepared = null;
    draft.value = "";
    message(status, "请先查看会话历史，核对上一次发送结果。");
    update();
    draft.focus();
  });
  return {
    show, hide,
    reset() { hide(); unconfirmed.clear(); },
    capability(value) { enabled = value; },
  };
}
