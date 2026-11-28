const lifecycles = {
  AwaitingModel: "等待模型", ModelInFlight: "模型执行中", WaitingTask: "等待任务",
  Paused: "已暂停", Succeeded: "成功", Failed: "失败", Cancelled: "已取消",
};
const outcomes = { Known: "已确认", Unknown: "未确认", NotStarted: "未执行", InFlight: "执行中" };

export function sessionsView(request, byId, element, message, fields) {
  const rows = byId("agent-session-rows");
  const empty = byId("agent-sessions-empty");
  const status = byId("status-agent-runtime");
  const detail = byId("agent-session-detail");
  const body = byId("agent-session-content");
  const previous = byId("agent-sessions-previous");
  const next = byId("agent-sessions-next");
  let pageRevision = 0;
  let detailRevision = 0;
  let pages = [null];
  let nextCursor = null;
  let selected = null;

  function clearDetail() {
    detailRevision += 1;
    detail.hidden = true;
    body.replaceChildren();
    rows.querySelector('[aria-pressed="true"]')?.setAttribute("aria-pressed", "false");
  }

  function hide() {
    pageRevision += 1;
    clearDetail();
    rows.replaceChildren();
    empty.hidden = true;
    previous.disabled = next.disabled = true;
    message(status, "");
  }

  function render(record) {
    const { reference, session: item } = record.status;
    const info = [["会话", reference.resource_id, true], ["修订", reference.version, true],
      ["状态", lifecycles[item.lifecycle] ?? item.lifecycle], ["轮次", `${item.turn} / ${item.max_turns}`],
      ["模型", item.model_ref.resource_id, true], ["Skill", item.skill_ref.resource_id, true]];
    if (item.last_task) info.push(["最近任务", item.last_task, true]);
    if (item.pause_reason) info.push(["暂停原因", item.pause_reason]);
    if (item.outcome_knowledge) info.push(["执行结局", outcomes[item.outcome_knowledge] ?? item.outcome_knowledge]);
    body.replaceChildren(fields(info), element("h3", "模型用量"));
    const meter = record.lease_time;
    const reports = record.provider_reported;
    const usage = [["已签发调用", record.charged_calls], ["执行中预扣（毫秒）", meter.active_reserved_ms],
      ["已结算（毫秒）", meter.settled_ms], ["结局未确认保留（毫秒）", meter.retained_unknown_ms],
      ["未计时调用", meter.unmetered_calls], ["提供者已报告的调用", reports.reported_calls]];
    if (reports.reported_calls) usage.push(["报告输入 Token", reports.input_tokens], ["报告输出 Token", reports.output_tokens]);
    body.append(fields(usage));
  }

  async function inspect(id, opener, focus = true) {
    clearDetail();
    const revision = detailRevision;
    selected = id;
    opener.setAttribute("aria-pressed", "true");
    detail.hidden = false;
    body.append(element("p", "正在读取会话…"));
    if (focus) detail.focus();
    try {
      const record = await request("/api/agent-session", { id });
      if (revision === detailRevision) render(record);
    } catch (error) {
      if (revision !== detailRevision) return;
      if (error.code === "PermissionDenied") await show([null], null);
      else body.replaceChildren(element("p", error.message, "message error"));
    }
  }

  async function show(requested = pages, session = selected) {
    hide();
    const revision = pageRevision;
    message(status, "正在读取 Agent 会话…");
    try {
      const result = await request("/api/agent-sessions", { cursor: requested.at(-1) });
      if (revision !== pageRevision) return;
      pages = requested;
      nextCursor = result.next_cursor;
      selected = null;
      previous.disabled = pages.length === 1;
      next.disabled = !nextCursor;
      empty.hidden = result.items.length !== 0;
      let reopen = null;
      for (const item of result.items) {
        const id = item.reference.resource_id;
        const button = element("button", id);
        button.type = "button";
        button.dataset.sessionId = id;
        button.setAttribute("aria-pressed", "false");
        button.addEventListener("click", () => inspect(id, button));
        const cell = element("td");
        cell.append(button);
        const row = element("tr");
        const lifecycle = item.session.lifecycle;
        row.append(cell, element("td", lifecycles[lifecycle] ?? lifecycle));
        rows.append(row);
        if (session === id) reopen = button;
      }
      message(status, "");
      if (reopen) void inspect(session, reopen, false);
    } catch (error) {
      if (revision !== pageRevision) return;
      if (error.code === "CursorExpired" && requested.length > 1) {
        await show([null], null);
      } else {
        message(status, error.message, true);
      }
    }
  }

  previous.addEventListener("click", () => show(pages.slice(0, -1), null));
  next.addEventListener("click", () => show([...pages, nextCursor], null));
  byId("close-agent-session").addEventListener("click", () => {
    const opener = rows.querySelector('[aria-pressed="true"]');
    selected = null;
    clearDetail();
    opener?.focus();
  });
  return {
    show, hide, refresh: () => show([null]),
    reset() { pages = [null]; nextCursor = selected = null; hide(); },
  };
}
