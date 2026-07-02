/**
 * PixiJS (WebGL) renderer for the bipartite graph: force-directed layout,
 * the LOD magnifying glass on zoom, risk color (green→red), agent highlights, a timeline
 * cutoff, and selection for attribution. Pure graph math lives in graph.ts; this module
 * only draws and handles input.
 *
 * Node colors are precomputed (`colorHex`) by `buildGraphSnapshot`, so this module imports
 * only *types* from `@brain0/shared` and never its Node-only runtime.
 */

import { Application, Container, Graphics, Text, TextStyle } from "pixi.js";
import type { GraphNode, GraphSnapshot, Level } from "@brain0/shared";
import { ForceLayout } from "./graph.js";

// Linear design system: lavender accent for intents, hairline for edges, lavender-hover for the
// search highlight. Artifact colors stay risk-based (green→red) — data semantics, like Linear's
// in-product priority palette.
const TASK_COLOR = 0x5e6ad2;
const EDGE_COLOR = 0x23252a;
const HIGHLIGHT_COLOR = 0x828fff;
// Severity → highlight stroke color (matches the sidebar finding cards): gold/red/amber/accent.
const SEVERITY_COLOR: Record<string, number> = {
  gold: 0xd8b24a,
  critical: 0xd65c5c,
  warn: 0xe0a33a,
  info: HIGHLIGHT_COLOR,
};

function hexToNumber(hex: string | undefined, fallback: number): number {
  if (!hex) return fallback;
  const parsed = Number.parseInt(hex.replace("#", ""), 16);
  return Number.isNaN(parsed) ? fallback : parsed;
}

/**
 * The label shown under a node: the level-appropriate name — the repo, module, file, or symbol
 * name (the last path segment, or the part after the last `::` for symbols) — or a commit's short
 * SHA. Each artifact only appears at its own LOD level, so this single name is always the right one.
 */
function nodeDisplayName(node: GraphNode): string {
  if (node.kind === "task") return node.ref ? node.ref.slice(0, 7) : "commit";
  const path = node.label;
  // Modules: show the full path (e.g. "packages/server/src", not just "src") so it's clear which
  // module it is — bare last segments like "src" repeat across the tree and read as ambiguous.
  if (node.level === "module") return path;
  const sym = path.split("::");
  if (sym.length > 1) return sym[sym.length - 1] || path; // symbol: after the last "::"
  const seg = path.split("/");
  return seg[seg.length - 1] || path; // repo / file: last path segment
}

export class PixiGraph {
  private readonly app = new Application();
  private world = new Container();
  private edges = new Graphics();
  private readonly nodeGraphics = new Map<string, Graphics>();
  // Node name labels (repo/module/file/symbol, or a commit's short SHA), shown under each node.
  private readonly labels = new Container();
  private readonly nodeLabels = new Map<string, Text>();
  private readonly artifactLabelStyle = new TextStyle({
    fontFamily: "Inter, system-ui, sans-serif",
    fontSize: 12,
    fill: 0x8a8f98,
  });
  private readonly commitLabelStyle = new TextStyle({
    fontFamily: "JetBrains Mono, ui-monospace, monospace",
    fontSize: 11,
    fill: 0x828fff,
  });
  private snapshot: GraphSnapshot = { repo: "", nodes: [], edges: [] };
  private layout = new ForceLayout(this.snapshot);
  private byId = new Map<string, GraphNode>();
  private zoom = 1;
  // The graph level is chosen explicitly via the view control, decoupled from zoom: once a level
  // is selected the user can zoom freely (far out ↔ very close) without ever switching level.
  private level: Level = "file";
  private highlights = new Set<string>();
  private highlightSeverity = new Map<string, string>();
  private selectedId: string | null = null;
  private timeCutoff = Number.POSITIVE_INFINITY;

  /** Callback invoked when a node is selected. */
  onSelect: (node: GraphNode) => void = () => {};

  /** Callback invoked when the selection is cleared (background click). */
  onDeselect: () => void = () => {};

  /** Callback invoked whenever the zoom or the selected level changes. */
  onViewChange: (zoom: number, level: Level) => void = () => {};

  async init(container: HTMLElement): Promise<void> {
    await this.app.init({
      background: "#010102", // Linear canvas — near-black with a faint blue tint
      resizeTo: container,
      antialias: true,
    });
    container.appendChild(this.app.canvas);
    this.app.stage.addChild(this.world);
    this.world.addChild(this.edges);
    this.edges.eventMode = "none"; // edges are decorative — let clicks fall through to nodes/stage
    this.world.addChild(this.labels); // name labels, above edges
    this.labels.eventMode = "none"; // labels never intercept clicks/drags
    this.app.stage.eventMode = "static";
    // Without a hitArea a container only receives pointer events where it has rendered content
    // (the nodes). Cover the whole viewport so clicks/drags on empty background register — this is
    // what makes panning on empty space and click-to-unfocus work. `app.screen` is updated in
    // place on resize, so the same reference keeps tracking the viewport.
    this.app.stage.hitArea = this.app.screen;

    this.installPanAndZoom(container);
    // The layout is settled once in setSnapshot (not stepped every frame), so nodes stay put
    // instead of drifting/exploding off-screen. The ticker only redraws (cheap) to reflect
    // pan/zoom/LOD/highlight changes.
    this.app.ticker.add(() => this.draw());
  }

  /** Number of settle iterations run once when a snapshot is set. */
  private static readonly SETTLE_ITERS = 400;
  /** Target span (world px) the settled layout is normalized to before relaxing. */
  private static readonly LAYOUT_SPAN = 1400;
  /** Minimum distance (world px) enforced between any two nodes, so labels never overlap. Deeper
   *  LOD levels are seen at higher zoom, so this world gap reads as an even larger on-screen gap. */
  private static readonly MIN_NODE_DIST = 120;
  /** Cap on collision-relaxation passes (it stops early once nothing overlaps). */
  private static readonly RELAX_ITERS = 160;
  /** Free-zoom range, decoupled from the level: far-out overview ↔ very close inspection. */
  private static readonly MIN_ZOOM = 0.1;
  private static readonly MAX_ZOOM = 12;
  /** Cap on the auto-fit zoom when framing a level, so sparse levels aren't absurdly magnified. */
  private static readonly FIT_MAX_ZOOM = 2.5;

  setSnapshot(snapshot: GraphSnapshot): void {
    this.snapshot = snapshot;
    this.byId = new Map(snapshot.nodes.map((n) => [n.id, n]));
    this.layout = new ForceLayout(snapshot);
    // Settle the layout once, then normalize it to a fixed span so any graph size frames the
    // same way (no per-frame stepping ⇒ no drift/explosion).
    this.layout.run(PixiGraph.SETTLE_ITERS);
    this.layout.normalize(PixiGraph.LAYOUT_SPAN);
    this.layout.relax(PixiGraph.MIN_NODE_DIST, PixiGraph.RELAX_ITERS);

    // Rebuild node graphics. Select/deselect is resolved centrally on stage pointerup (see
    // installPanAndZoom); here each node is just made hittable and tagged with its id via `label`.
    for (const g of this.nodeGraphics.values()) g.destroy();
    this.nodeGraphics.clear();
    for (const t of this.nodeLabels.values()) t.destroy();
    this.nodeLabels.clear();
    for (const node of snapshot.nodes) {
      const g = new Graphics();
      g.eventMode = "static";
      g.cursor = "pointer";
      g.label = node.id;
      this.world.addChild(g);
      this.nodeGraphics.set(node.id, g);

      const text = new Text({
        text: nodeDisplayName(node),
        style: node.kind === "task" ? this.commitLabelStyle : this.artifactLabelStyle,
      });
      text.anchor.set(0.5, 0); // top-center, so it sits centered just below the node
      text.eventMode = "none";
      text.resolution = 3; // rasterize dense so the counter-scaled text stays crisp when zoomed out
      this.labels.addChild(text);
      this.nodeLabels.set(node.id, text);
    }
    this.world.addChild(this.labels); // keep labels above the freshly-added node graphics

    this.frameLevel();
  }

  /** Whether a node belongs to the current level (commits ride the file & symbol levels). */
  private visibleAtLevel(node: GraphNode): boolean {
    return node.kind === "task"
      ? this.level === "file" || this.level === "symbol"
      : node.level === this.level;
  }

  /**
   * Select the graph level to display (repo/module/file/symbol). The level is explicit and
   * decoupled from zoom — choosing one re-frames its nodes to fit the viewport; the user can then
   * zoom freely within it (far out ↔ very close) without ever crossing into another level.
   */
  setLevel(level: Level): void {
    this.level = level;
    this.frameLevel();
  }

  /** Fit the nodes of the current level into the viewport (the initial view and on level change). */
  private frameLevel(): void {
    const sw = this.app.screen.width;
    const sh = this.app.screen.height;
    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const node of this.snapshot.nodes) {
      if (!this.visibleAtLevel(node)) continue;
      const p = this.layout.position(node.id);
      if (!p) continue;
      if (p.x < minX) minX = p.x;
      if (p.x > maxX) maxX = p.x;
      if (p.y < minY) minY = p.y;
      if (p.y > maxY) maxY = p.y;
    }
    if (!Number.isFinite(minX)) {
      // No nodes at this level: center at a neutral zoom.
      this.zoom = 1;
      this.world.scale.set(1);
      this.world.position.set(sw / 2, sh / 2);
      this.onViewChange(this.zoom, this.level);
      return;
    }
    const margin = 1.16; // breathing room around the level's bounding box
    const w = (maxX - minX) * margin || 1;
    const h = (maxY - minY) * margin || 1;
    const z = Math.min(
      PixiGraph.FIT_MAX_ZOOM,
      Math.max(PixiGraph.MIN_ZOOM, Math.min(sw / w, sh / h)),
    );
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    this.zoom = z;
    this.world.scale.set(z);
    this.world.position.set(sw / 2 - cx * z, sh / 2 - cy * z);
    this.onViewChange(this.zoom, this.level);
  }

  /** Set the zoom (free, clamped) keeping the viewport center fixed; the level is unchanged. */
  setZoom(zoom: number): void {
    const z = Math.min(PixiGraph.MAX_ZOOM, Math.max(PixiGraph.MIN_ZOOM, zoom));
    // Keep the world point currently under the viewport center fixed, so zoom feels anchored.
    const sw = this.app.screen.width / 2;
    const sh = this.app.screen.height / 2;
    const wx = (sw - this.world.x) / this.zoom;
    const wy = (sh - this.world.y) / this.zoom;
    this.zoom = z;
    this.world.scale.set(z);
    this.world.position.set(sw - wx * z, sh - wy * z);
    this.onViewChange(this.zoom, this.level);
  }

  /** Multiply the current zoom (for the +/- buttons and the wheel). */
  zoomBy(factor: number): void {
    this.setZoom(this.zoom * factor);
  }

  /** Re-fit the current level to the viewport. */
  resetView(): void {
    this.frameLevel();
  }

  setHighlights(ids: Set<string>, severityMap?: Map<string, string>): void {
    this.selectedId = null;
    this.highlights = ids;
    this.highlightSeverity = severityMap ?? new Map();
  }

  /**
   * Select a node: highlight it plus its directly connected nodes, emphasize the incident edges,
   * and dim the rest — so a node's dependencies and change links read at a glance. Highlighted
   * neighbors are forced visible even if they sit at another LOD level. Pass `null` to clear.
   */
  setSelection(id: string | null): void {
    this.selectedId = id;
    this.highlightSeverity = new Map();
    const set = new Set<string>();
    if (id) {
      set.add(id);
      for (const edge of this.snapshot.edges) {
        if (edge.src === id) set.add(edge.dst);
        else if (edge.dst === id) set.add(edge.src);
      }
    }
    this.highlights = set;
  }

  /** Hide nodes/edges whose timestamp is after the cutoff (ms epoch); Infinity shows all. */
  setTimeCutoff(cutoffMs: number): void {
    this.timeCutoff = cutoffMs;
  }

  private nodeRadius(node: GraphNode): number {
    if (node.kind === "task") return 7;
    switch (node.level) {
      case "repo":
        return 16;
      case "module":
        return 12;
      case "file":
        return 9;
      default:
        return 6;
    }
  }

  private isVisible(node: GraphNode): boolean {
    if (node.timestamp && Date.parse(node.timestamp) > this.timeCutoff) return false;
    // Highlighted nodes (the selection and its neighbors, or a search chain) are always shown, so
    // dependencies stay visible even if they belong to another level.
    if (this.highlights.has(node.id)) return true;
    return this.visibleAtLevel(node);
  }

  private draw(): void {
    // Nodes (+ their name labels).
    for (const node of this.snapshot.nodes) {
      const g = this.nodeGraphics.get(node.id);
      const pos = this.layout.position(node.id);
      const label = this.nodeLabels.get(node.id);
      if (!g || !pos) continue;
      const visible = this.isVisible(node);
      g.visible = visible;
      if (label) label.visible = visible;
      if (!visible) continue;

      g.position.set(pos.x, pos.y);
      g.clear();
      const radius = this.nodeRadius(node);
      const color = node.kind === "task" ? TASK_COLOR : hexToNumber(node.colorHex, 0x6e7681);
      g.circle(0, 0, radius).fill(color);
      let alpha = 1;
      if (node.id === this.selectedId) {
        g.stroke({ width: 3.5, color: HIGHLIGHT_COLOR });
      } else if (this.highlights.has(node.id)) {
        const sev = this.highlightSeverity.get(node.id);
        g.stroke({ width: sev && sev !== "info" ? 3 : 2, color: SEVERITY_COLOR[sev ?? "info"] ?? HIGHLIGHT_COLOR });
      } else {
        alpha = this.highlights.size > 0 ? 0.28 : 1; // dim non-neighbors when something is selected
      }
      g.alpha = alpha;

      if (label) {
        // Counter-scale by 1/zoom so the text keeps a constant on-screen size at every LOD, and
        // offset by a screen-constant gap (radius is world units → ×zoom on screen; 7/zoom ⇒ 7px).
        label.scale.set(1 / this.zoom);
        label.position.set(pos.x, pos.y + radius + 7 / this.zoom);
        label.alpha = alpha;
      }
    }

    // Edges (only between currently visible endpoints). When something is selected/highlighted,
    // emphasize the connected edges and dim the rest.
    const selecting = this.selectedId !== null;
    const focusing = selecting || this.highlights.size > 0;
    this.edges.clear();
    for (const edge of this.snapshot.edges) {
      const a = this.byId.get(edge.src);
      const b = this.byId.get(edge.dst);
      if (!a || !b || !this.isVisible(a) || !this.isVisible(b)) continue;
      const pa = this.layout.position(edge.src);
      const pb = this.layout.position(edge.dst);
      if (!pa || !pb) continue;
      const active = selecting
        ? edge.src === this.selectedId || edge.dst === this.selectedId
        : this.highlights.has(edge.src) && this.highlights.has(edge.dst);
      this.edges.moveTo(pa.x, pa.y).lineTo(pb.x, pb.y).stroke({
        width: active ? 2 : 1,
        color: active ? HIGHLIGHT_COLOR : EDGE_COLOR,
        alpha: focusing && !active ? 0.12 : 1,
      });
    }
  }

  private installPanAndZoom(container: HTMLElement): void {
    let dragging = false;
    let moved = false;
    let lastX = 0;
    let lastY = 0;
    let downX = 0;
    let downY = 0;
    let pressedId: string | null = null;
    const DRAG_THRESHOLD = 4;

    // All pointer interaction is resolved here (the stage covers the whole viewport via hitArea):
    // a press anywhere starts a potential pan; on release, a press that didn't move is a click —
    // selecting the node under it, or clearing the selection if it was on empty background. This
    // lets the user pan by dragging from anywhere, including over nodes, at any zoom.
    this.app.stage.on("pointerdown", (event) => {
      dragging = true;
      moved = false;
      lastX = downX = event.globalX;
      lastY = downY = event.globalY;
      const label = (event.target as Container | null)?.label ?? "";
      pressedId = this.byId.has(label) ? label : null;
    });
    this.app.stage.on("globalpointermove", (event) => {
      if (!dragging) return;
      if (Math.hypot(event.globalX - downX, event.globalY - downY) > DRAG_THRESHOLD) moved = true;
      this.world.x += event.globalX - lastX;
      this.world.y += event.globalY - lastY;
      lastX = event.globalX;
      lastY = event.globalY;
    });
    const endDrag = (): void => {
      if (dragging && !moved) {
        const node = pressedId ? this.byId.get(pressedId) : undefined;
        if (node) {
          this.setSelection(node.id);
          this.onSelect(node);
        } else if (this.selectedId !== null || this.highlights.size > 0) {
          // A background click clears whatever is lit — a single selection OR a whole search/audit
          // highlight set (after a search `selectedId` is null but highlights are non-empty).
          this.setSelection(null);
          this.onDeselect();
        }
      }
      dragging = false;
      pressedId = null;
    };
    this.app.stage.on("pointerup", endDrag);
    this.app.stage.on("pointerupoutside", endDrag);

    container.addEventListener("wheel", (event) => {
      event.preventDefault();
      this.zoomBy(event.deltaY < 0 ? 1.1 : 1 / 1.1);
    });

    // Center the world initially.
    this.world.position.set(this.app.screen.width / 2, this.app.screen.height / 2);
  }
}
