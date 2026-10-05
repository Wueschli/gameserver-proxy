import type { FleetInstanceView } from "../types";

export interface GroupNode {
  /** This node's own path segment, e.g. "frankfurt" ("" for the root). */
  segment: string;
  /** Full path to this node, e.g. "eu/frankfurt" ("" for the root). */
  path: string;
  children: GroupNode[];
  /** Instances whose `group` is exactly this node's `path`. */
  instances: FleetInstanceView[];
}

function countUnhealthy(inst: FleetInstanceView): number {
  return inst.pools.flatMap((p) => p.backends).filter((b) => !b.healthy).length;
}

/** Every backend across every instance under `node`, healthy or not. */
export function subtreeUnhealthyCount(node: GroupNode): number {
  const own = node.instances.reduce((n, inst) => n + countUnhealthy(inst), 0);
  return own + node.children.reduce((n, c) => n + subtreeUnhealthyCount(c), 0);
}

export function subtreeInstanceCount(node: GroupNode): number {
  return (
    node.instances.length + node.children.reduce((n, c) => n + subtreeInstanceCount(c), 0)
  );
}

/**
 * Builds a tree from each instance's self-reported `group` path
 * (slash-separated, e.g. "eu/frankfurt/cluster-a" — see `settings.group`,
 * `docs/05-configuration.md`). An instance with no `group` lands directly on
 * the root node, alongside any real top-level groups — kept as one flat
 * "ungrouped" bucket would hide it, sorting it in with everything else is
 * simpler and matches "no group" reading as "no opinion," not "excluded."
 */
export function buildGroupTree(instances: FleetInstanceView[]): GroupNode {
  const root: GroupNode = { segment: "", path: "", children: [], instances: [] };

  for (const inst of instances) {
    const segments = inst.group ? inst.group.split("/").filter(Boolean) : [];
    let node = root;
    let path = "";
    for (const segment of segments) {
      path = path ? `${path}/${segment}` : segment;
      let next = node.children.find((c) => c.segment === segment);
      if (!next) {
        next = { segment, path, children: [], instances: [] };
        node.children.push(next);
      }
      node = next;
    }
    node.instances.push(inst);
  }

  const sortNode = (node: GroupNode) => {
    node.children.sort((a, b) => a.segment.localeCompare(b.segment));
    node.instances.sort((a, b) => a.instance.localeCompare(b.instance));
    node.children.forEach(sortNode);
  };
  sortNode(root);
  return root;
}
