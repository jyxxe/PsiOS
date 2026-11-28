export function filesView(request, byId, element, message, downloadFile) {
  const rows = byId("files-rows");
  const location = byId("files-path");
  const status = byId("status-files");
  const empty = byId("files-empty");
  const parent = byId("files-parent");
  const previous = byId("files-previous");
  const next = byId("files-next");
  const retry = byId("files-retry");
  const preview = byId("files-preview");
  const previewTitle = byId("files-preview-title");
  const previewStatus = byId("files-preview-status");
  const previewText = byId("files-preview-text");
  const kinds = { directory: "目录", file: "文件", unsupported: "特殊条目" };
  let path = "";
  let pages = [null];
  let continuation = null;
  let pending = null;
  let revision = 0;
  let previewAllowed = false;
  let downloadAllowed = false;
  let download = null;
  let previewRevision = 0;
  let selectedFile = null;

  function clearPreview() {
    previewRevision += 1;
    preview.hidden = previewText.hidden = true;
    previewTitle.textContent = previewText.textContent = "";
    message(previewStatus, "");
    selectedFile = null;
  }

  async function openPreview(entry, volumeVersion, button) {
    clearPreview();
    const version = previewRevision;
    selectedFile = button;
    preview.hidden = false;
    previewTitle.textContent = entry.name;
    message(previewStatus, "正在读取…");
    const relative = [path, entry.name].filter(Boolean).join("/");
    try {
      const result = await request("/api/volume-preview", {
        key: crypto.randomUUID(), path: relative, volume_version: volumeVersion,
      });
      if (version !== previewRevision) return;
      const bytes = Uint8Array.from(atob(result.data_base64), char => char.charCodeAt(0));
      let text;
      try {
        text = new TextDecoder("utf-8", { fatal: true }).decode(bytes, { stream: !result.eof });
      } catch {
        message(previewStatus, "此预览需要 UTF-8 文本。", true);
        previewTitle.focus();
        return;
      }
      previewText.hidden = !text.length;
      previewText.textContent = text;
      message(previewStatus, !result.eof ? "显示文件开头最多 6 KiB。" : text ? "" : "文件为空。");
      previewTitle.focus();
    } catch (error) {
      if (version !== previewRevision) return;
      message(previewStatus, error.message, true);
      previewTitle.focus();
    }
  }

  async function saveFile(relative, volumeVersion, button, host) {
    download?.abort();
    const request = new AbortController();
    download = request;
    button.disabled = true;
    message(status, "正在读取并核对文件…");
    try {
      if (await downloadFile(relative, volumeVersion, host, request.signal)) {
        message(status, "文件已核对并交给浏览器下载。");
      }
    } catch (error) {
      if (download === request) message(status, error.message, true);
    } finally {
      if (download === request) download = null;
      button.disabled = false;
    }
  }

  function clear() {
    download?.abort();
    download = null;
    clearPreview();
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
        if (entry.kind === "directory" || entry.kind === "file" && previewAllowed) {
          const button = element("button", entry.name);
          button.type = "button";
          if (entry.kind === "directory") {
            button.addEventListener("click", () => start([path, entry.name].filter(Boolean).join("/")));
          } else {
            button.addEventListener("click", () => openPreview(entry, page.volume_version, button));
          }
          name.append(button);
        } else {
          name.textContent = entry.name;
        }
        if (entry.kind === "file" && downloadAllowed) {
          const save = element("button", "下载", "file-download");
          save.type = "button";
          save.setAttribute("aria-label", "下载 " + entry.name);
          const relative = [path, entry.name].filter(Boolean).join("/");
          save.addEventListener("click", () => saveFile(relative, page.volume_version, save, name));
          name.append(save);
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
  byId("files-preview-close").addEventListener("click", () => {
    const button = selectedFile;
    clearPreview();
    button?.focus();
  });

  return {
    capability(enabled, previewEnabled = false, downloadEnabled = false) {
      byId("nav-files").hidden = !enabled;
      previewAllowed = enabled && previewEnabled;
      downloadAllowed = enabled && downloadEnabled;
    },
    show() { start(path, [null], false); },
    refresh() { start(pending?.path ?? path); },
    hide() { revision += 1; pending = null; clear(); },
    reset() { path = ""; pages = [null]; },
  };
}
