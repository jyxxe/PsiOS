const roles = { Owner: "所有者", Manager: "管理员", Reviewer: "审核者", Contributor: "协作者", Viewer: "查看者" };
const lifecycles = { Ready: "待处理", InReview: "审核中", Verified: "已验证", Closed: "已关闭", Abandoned: "已终止" };
const priorities = { Low: "低", Normal: "普通", High: "高", Urgent: "紧急" };

export function projectView(request, byId, element, message, fields) {
  const rows = byId("project-rows");
  const empty = byId("projects-empty");
  const notice = byId("status-projects");
  const detail = byId("project-detail");
  const title = byId("project-title");
  const content = byId("project-content");
  const previous = byId("projects-previous");
  const next = byId("projects-next");
  const previousItems = byId("work-items-previous");
  const nextItems = byId("work-items-next");
  const itemDetail = byId("work-item-detail");
  const itemTitle = byId("work-item-title");
  const itemContent = byId("work-item-content");
  let directoryRevision = 0;
  let itemRevision = 0;
  let detailRevision = 0;
  let directory = [null];
  let items = [null];
  let nextProject = null;
  let nextItem = null;
  let selection = null;

  function clearItemDetail() {
    detailRevision += 1;
    itemDetail.hidden = true;
    itemTitle.textContent = "工作项详情";
    itemContent.replaceChildren();
    content.querySelector('[aria-pressed="true"]')?.setAttribute("aria-pressed", "false");
  }

  function clearDetail() {
    itemRevision += 1;
    clearItemDetail();
    detail.hidden = true;
    title.textContent = "工作项";
    content.replaceChildren();
    previousItems.disabled = nextItems.disabled = true;
  }

  function hide() {
    directoryRevision += 1;
    rows.replaceChildren();
    empty.hidden = true;
    previous.disabled = next.disabled = true;
    clearDetail();
    message(notice, "");
  }

  function renderItemDetail(item) {
    itemTitle.textContent = item.title;
    itemContent.replaceChildren();
    if (item.description) itemContent.append(element("p", item.description));
    itemContent.append(fields([
      ["工作项修订", item.reference.version, true],
      ["状态", lifecycles[item.lifecycle] ?? item.lifecycle],
      ["负责人", item.assignee],
      ["优先级", priorities[item.priority] ?? item.priority],
    ]));
    if (item.review) itemContent.append(fields([
      ["审核结论", item.review.accepted ? "接受" : "退回"],
      ["审核人", item.review.reviewer],
      ["审核时间", item.review.reviewed_at],
    ]));
    if (item.evidence) {
      const { task_ref: task, artifact_ref: artifact, submitted_at: submitted } = item.evidence;
      itemContent.append(element("h3", "任务证据"), fields([
        ["任务", task.resource_id, true], ["任务修订", task.version, true],
        ["制品", artifact.resource_id, true], ["制品修订", artifact.version, true],
        ["制品引用摘要", artifact.digest, true], ["提交时间", submitted],
      ]));
    } else if (item.has_evidence) {
      itemContent.append(element("p", "当前授权下无法查看任务证据。", "message"));
    }
  }

  async function inspectItem(reference, button) {
    clearItemDetail();
    const revision = detailRevision;
    button.setAttribute("aria-pressed", "true");
    itemDetail.hidden = false;
    itemContent.append(element("p", "正在读取工作项修订…"));
    itemDetail.focus();
    try {
      const item = await request("/api/work-item-revision", { reference });
      if (revision === detailRevision) renderItemDetail(item);
    } catch (error) {
      if (revision !== detailRevision) return;
      if (error.code === "PermissionDenied") await loadProjects(directory, null);
      else itemContent.replaceChildren(element("p", error.message, "message error"));
    }
  }

  function renderItems(records) {
    if (!records.length) return element("p", "这个项目还没有工作项。", "empty");
    const list = element("ul", undefined, "project-items");
    for (const item of records) {
      const status = [lifecycles[item.lifecycle] ?? item.lifecycle];
      if (item.blocked) status.push("有未完成的依赖");
      status.push("优先级 " + (priorities[item.priority] ?? item.priority));
      const entry = element("li");
      entry.dataset.workItemId = item.id;
      const button = element("button");
      button.type = "button";
      button.setAttribute("aria-pressed", "false");
      button.append(element("strong", item.title));
      button.addEventListener("click", () => inspectItem(item.reference, button));
      entry.append(button, element("p", status.join(" · ")),
                   element("p", "负责人 " + item.assignee));
      list.append(entry);
    }
    return list;
  }

  async function loadItems(id, pages = [null], focus = false) {
    clearDetail();
    const revision = itemRevision;
    selection = id;
    for (const button of rows.querySelectorAll("button")) {
      button.setAttribute("aria-pressed", String(button.dataset.projectId === id));
    }
    detail.hidden = false;
    content.append(element("p", "正在读取工作项…"));
    if (focus) detail.focus();
    try {
      const page = await request("/api/project-work-items", { project_id: id, after_id: pages.at(-1) });
      if (revision !== itemRevision) return;
      items = pages;
      nextItem = page.next_after_id;
      title.textContent = page.title;
      content.replaceChildren(renderItems(page.items));
      previousItems.disabled = items.length === 1;
      nextItems.disabled = !nextItem;
    } catch (error) {
      if (revision === itemRevision) {
        if (error.code === "PermissionDenied") {
          await loadProjects(directory, null);
          return;
        }
        content.replaceChildren(element("p", error.message, "message error"));
      }
    }
  }

  async function loadProjects(pages = directory, selected = selection, workPages = items) {
    hide();
    const revision = directoryRevision;
    message(notice, "正在读取项目…");
    try {
      const page = await request("/api/projects", { after_id: pages.at(-1) });
      if (revision !== directoryRevision) return;
      directory = pages;
      nextProject = page.next_after_id;
      previous.disabled = directory.length === 1;
      next.disabled = !nextProject;
      empty.hidden = page.items.length !== 0;
      const result = document.createDocumentFragment();
      for (const project of page.items) {
        const button = element("button", project.title);
        button.type = "button";
        button.dataset.projectId = project.id;
        button.setAttribute("aria-pressed", "false");
        button.addEventListener("click", () => loadItems(project.id, [null], true));
        const cell = element("td");
        cell.append(button);
        const row = element("tr");
        row.append(cell, element("td", roles[project.role] ?? project.role));
        result.append(row);
      }
      rows.append(result);
      message(notice, "");
      selection = null;
      if (page.items.some(project => project.id === selected)) void loadItems(selected, workPages);
    } catch (error) {
      if (revision === directoryRevision) message(notice, error.message, true);
    }
  }

  previous.addEventListener("click", () => loadProjects(directory.slice(0, -1), null));
  next.addEventListener("click", () => loadProjects([...directory, nextProject], null));
  previousItems.addEventListener("click", () => loadItems(selection, items.slice(0, -1)));
  nextItems.addEventListener("click", () => loadItems(selection, [...items, nextItem]));
  byId("close-work-item").addEventListener("click", () => {
    const opener = content.querySelector('[aria-pressed="true"]');
    clearItemDetail();
    opener?.focus();
  });
  byId("close-project").addEventListener("click", () => {
    const button = rows.querySelector('button[aria-pressed="true"]');
    selection = null;
    items = [null];
    clearDetail();
    button?.setAttribute("aria-pressed", "false");
    button?.focus();
  });
  return {
    show: loadProjects,
    hide,
    reset() {
      directory = items = [null];
      nextProject = nextItem = selection = null;
      hide();
    },
    refresh: () => loadProjects(directory, selection, [null]),
  };
}
