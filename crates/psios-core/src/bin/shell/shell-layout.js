export const MAX_WORKSPACES = 8;
const MAX_GEOMETRY = 65536;
export const MAX_LAYOUT_BYTES = 8 * 1024;
export const LAYOUT_FILE_NAME = "psios-shell-layout.json";

function fields(value, keys) {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

export function workspaceName(input) {
  const name = input.normalize("NFKC").trim();
  return name.length > 0 && name.length <= 32 && !/[\x00-\x1f\x7f]/u.test(name) ? name : null;
}

function geometry(value) {
  if (value === null) return true;
  const keys = ["x", "y", "width", "height"];
  return fields(value, keys) && keys.every(key => Number.isFinite(value[key]) && value[key] <= MAX_GEOMETRY)
    && value.x >= 0 && value.y >= 0 && value.width > 0 && value.height > 0;
}

export function decodeLayout(source, scope, appIds) {
  const invalid = new Error("布局文件格式无效。");
  let layout;
  try {
    layout = JSON.parse(source);
  } catch {
    throw invalid;
  }
  if (!fields(layout, ["version", "scope", "current", "workspaces", "windows"]) || layout.version !== 1) throw invalid;
  if (layout.scope !== scope) throw new Error("此布局属于其他登录身份或节点。");
  const { workspaces, windows, current } = layout;
  if (!Array.isArray(workspaces) || workspaces.length < 1 || workspaces.length > MAX_WORKSPACES
      || !Array.isArray(windows) || windows.length > appIds.size) throw invalid;
  const isWorkspace = id => Number.isInteger(id) && id >= 0 && id < workspaces.length;
  if (!isWorkspace(current) || workspaces[0]?.name !== "桌面") throw invalid;

  const names = new Set();
  for (const workspace of workspaces) {
    if (!fields(workspace, ["name", "active", "maximized"]) || typeof workspace.name !== "string"
        || workspaceName(workspace.name) !== workspace.name || names.has(workspace.name)) throw invalid;
    names.add(workspace.name);
  }
  const registered = new Map();
  const tiles = new Set();
  for (const window of windows) {
    if (!fields(window, ["id", "workspace", "state", "tile", "geometry"]) || !appIds.has(window.id)
        || registered.has(window.id) || !isWorkspace(window.workspace) || !["open", "minimized"].includes(window.state)
        || ![null, "left", "right"].includes(window.tile) || !geometry(window.geometry)) throw invalid;
    if (window.tile) {
      const tile = window.workspace + "/" + window.tile;
      if (tiles.has(tile)) throw invalid;
      tiles.add(tile);
    }
    registered.set(window.id, window);
  }
  for (const [workspace, views] of workspaces.entries()) {
    for (const id of [views.active, views.maximized]) {
      if (id === null) continue;
      const window = registered.get(id);
      if (window?.workspace !== workspace || window.state !== "open") throw invalid;
    }
  }
  return layout;
}
