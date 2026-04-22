import { rootFolderName } from "./format";
import type { LibraryRoot, MediaItem } from "./types";

export type FolderNode = {
  // Virtual path inside the library tree, e.g. "Movies" or "Movies/Action".
  // Empty string represents the synthetic library root.
  path: string;
  // Display label (last segment of `path`, or "Library" for the root).
  name: string;
  // Subfolders, sorted alphabetically by name.
  children: FolderNode[];
  // Media items that live directly in this folder, sorted by file name.
  items: MediaItem[];
  // Total media items at and below this node.
  descendantCount: number;
};

const ROOT_NAME = "Library";

export function buildFolderTree(items: MediaItem[], roots: LibraryRoot[]): FolderNode {
  const root = makeNode("", ROOT_NAME);

  // Pre-create one folder per configured library root so empty roots still
  // appear in the navigator. The display name is the last path segment.
  const rootSegmentByPath = new Map<string, string>();
  for (const libraryRoot of roots) {
    const segment = uniqueRootSegment(rootFolderName(libraryRoot.path), root, rootSegmentByPath);
    rootSegmentByPath.set(libraryRoot.path, segment);
    ensureChild(root, segment);
  }

  for (const item of items) {
    const rootSegment =
      rootSegmentByPath.get(item.rootPath) ??
      uniqueRootSegment(rootFolderName(item.rootPath), root, rootSegmentByPath);
    if (!rootSegmentByPath.has(item.rootPath)) {
      rootSegmentByPath.set(item.rootPath, rootSegment);
    }

    const relativeSegments = splitRelativePath(item.relativePath);
    if (relativeSegments.length === 0) {
      // Item at the synthetic root of its library root — attach to the root folder.
      const folder = ensureChild(root, rootSegment);
      folder.items.push(item);
      continue;
    }

    // Last segment is the file itself; everything before it is folder structure.
    const folderSegments = relativeSegments.slice(0, -1);
    let cursor = ensureChild(root, rootSegment);
    for (const segment of folderSegments) {
      cursor = ensureChild(cursor, segment);
    }
    cursor.items.push(item);
  }

  finalize(root);
  return root;
}

export function findFolder(tree: FolderNode, path: string | null): FolderNode | null {
  if (path === null || path === "") {
    return tree;
  }

  const segments = path.split("/").filter(Boolean);
  let cursor: FolderNode = tree;
  for (const segment of segments) {
    const next = cursor.children.find((child) => child.name === segment);
    if (!next) {
      return null;
    }
    cursor = next;
  }
  return cursor;
}

export function folderPathSegments(path: string): string[] {
  return path.split("/").filter(Boolean);
}

function makeNode(path: string, name: string): FolderNode {
  return {
    path,
    name,
    children: [],
    items: [],
    descendantCount: 0,
  };
}

function ensureChild(parent: FolderNode, name: string): FolderNode {
  const existing = parent.children.find((child) => child.name === name);
  if (existing) {
    return existing;
  }
  const childPath = parent.path === "" ? name : `${parent.path}/${name}`;
  const child = makeNode(childPath, name);
  parent.children.push(child);
  return child;
}

function uniqueRootSegment(
  baseName: string,
  root: FolderNode,
  taken: Map<string, string>,
): string {
  // Two configured roots can share the same trailing folder name (e.g.
  // /a/Movies and /b/Movies). Disambiguate by suffixing " (2)", " (3)" …
  // so each root still gets its own folder in the navigator.
  const usedNames = new Set(taken.values());
  if (!usedNames.has(baseName) && !root.children.some((child) => child.name === baseName)) {
    return baseName;
  }

  let counter = 2;
  while (true) {
    const candidate = `${baseName} (${counter})`;
    if (!usedNames.has(candidate) && !root.children.some((child) => child.name === candidate)) {
      return candidate;
    }
    counter += 1;
  }
}

function splitRelativePath(relativePath: string): string[] {
  return relativePath.split("/").filter(Boolean);
}

function finalize(node: FolderNode): number {
  node.children.sort((left, right) => left.name.localeCompare(right.name));
  node.items.sort((left, right) => left.fileName.localeCompare(right.fileName));

  let total = node.items.length;
  for (const child of node.children) {
    total += finalize(child);
  }
  node.descendantCount = total;
  return total;
}
