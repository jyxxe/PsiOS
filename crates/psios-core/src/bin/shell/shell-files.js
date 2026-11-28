export function filesView(request, byId, element, message) {
  const rows = byId("files-rows");
  const location = byId("files-path");
  const status = byId("status-files");
  const empty = byId("files-empty");
  const parent = byId("files-parent");
  const previous = byId("files-previous");
  const next = byId("files-next");
  const retry = byId("files-retry");
  const kinds = { directory: "目录", file: "文件", unsupported: "特殊条目" };
  let path = "";
  let pages = [null];
  let continuation = null;
  let pending = null;
  let revision = 0;

  function clear() {
    rows.replaceChildren();
    location.textContent = "";
    empty.hidden = retry.hidden = true;
    parent.disabled = previous.disabled = next.disabled = true;
    continuation = null;
    message(status, "");
  }

  async function load(attempt) {
    const version = ++revision;
    clear();
    location.textContent = "/" + attempt.path;
    message(status, "正在读取…");
    try {
      const page = await request("/api/volume-directory", {
        key: attempt.key,
        query: { path: attempt.path, ...attempt.pages.at(-1) },
      });
      if (version !== revision) return;
      path = attempt.path;
      pages = attempt.pages;
      continuation = page.next_after === null ? null : {
        after: page.next_after,
        volume_version: page.volume_version,
        directory_revision: page.directory_revision,
      };
      const content = document.createDocumentFragment();
      for (const entry of page.entries) {
        const row = element("tr");
        const name = element("td");
        if (entry.kind === "directory") {
          const button = element("button", entry.name);
          button.type = "button";
          button.addEventListener("click", () => start([path, entry.name].filter(Boolean).join("/")));
          name.append(button);
        } else {
          name.textContent = entry.name;
        }
        row.append(name, element("td", kinds[entry.kind] ?? "未知"),
          element("td", entry.kind === "file" ? entry.bytes + " B" : "—", "mono"));
        content.append(row);
      }
      rows.append(content);
      empty.hidden = page.entries.length !== 0;
      parent.disabled = path === "";
      previous.disabled = pages.length === 1;
      next.disabled = continuation === null;
      pending = null;
      message(status, "");
      if (attempt.focus) location.focus();
    } catch (error) {
      if (version !== revision) return;
      parent.disabled = path === "";
      const changed = ["Conflict", "CursorExpired", "PreconditionFailed"].includes(error.code);
      message(status, changed ? "目录已变化。刷新后将从第一页读取。" : error.message, true);
      retry.hidden = !["QueueFull", "DeadlineExceeded", "DependencyUnavailable"].includes(error.code);
    }
  }

  function start(target, checkpoints = [null], focus = true) {
    pending = { path: target, pages: checkpoints, focus, key: crypto.randomUUID() };
    void load(pending);
  }

  parent.addEventListener("click", () => start(path.split("/").slice(0, -1).join("/")));
  previous.addEventListener("click", () => start(path, pages.slice(0, -1)));
  next.addEventListener("click", () => start(path, [...pages, continuation]));
  retry.addEventListener("click", () => { if (pending) void load(pending); });

  return {
    capability(enabled) { byId("nav-files").hidden = !enabled; },
    show() { start(path, [null], false); },
    refresh() { start(pending?.path ?? path); },
    hide() { revision += 1; pending = null; clear(); },
    reset() { path = ""; pages = [null]; },
  };
}
