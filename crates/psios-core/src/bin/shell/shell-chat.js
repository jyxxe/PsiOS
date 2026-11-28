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

  function clearHistory() {
    historyRevision += 1;
    detail.hidden = true;
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
      earlier.disabled = history.length === 1;
      later.disabled = !nextMessage;
    } catch (error) {
      if (revision !== historyRevision) return;
      if (error.code === "CursorExpired" && pages.length > 1) {
        await loadHistory(id, name, [null], focus);
        return;
      }
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
  return { show: loadList, hide, reset, refresh: () => loadList([null], selection, [null]) };
}
