/**
 * Pure graph logic for the GUI (no PixiJS, no DOM) so it is unit-testable: a deterministic
 * force-directed layout, the level-of-detail "magnifying glass" selection, and highlight
 * computation. The PixiJS renderer (renderer.ts) drives this and draws the result.
 */
export const DEFAULT_FORCES = {
    repulsion: 2000,
    spring: 0.02,
    restLength: 60,
    gravity: 0.01,
    damping: 0.85,
};
/**
 * Maximum per-tick displacement. Without this, large graphs (hundreds of nodes seeded close
 * together on the initial circle) accumulate enormous repulsion velocities and fly off-screen
 * within a few frames — the nodes "appear then vanish". Clamping the speed keeps the layout
 * bounded and convergent.
 */
const MAX_SPEED = 30;
/**
 * A deterministic force-directed layout. Initial positions are seeded on a circle by node
 * index, so a given snapshot always lays out identically (important for stable visuals and
 * for tests).
 */
export class ForceLayout {
    params;
    nodes;
    index = new Map();
    edges;
    constructor(snapshot, params = DEFAULT_FORCES) {
        this.params = params;
        const n = Math.max(snapshot.nodes.length, 1);
        this.nodes = snapshot.nodes.map((node, i) => {
            this.index.set(node.id, i);
            const angle = (2 * Math.PI * i) / n;
            return { id: node.id, x: Math.cos(angle) * 200, y: Math.sin(angle) * 200, vx: 0, vy: 0 };
        });
        this.edges = [];
        for (const edge of snapshot.edges) {
            const a = this.index.get(edge.src);
            const b = this.index.get(edge.dst);
            if (a !== undefined && b !== undefined)
                this.edges.push([a, b]);
        }
    }
    /** Advance the simulation by one tick. */
    step() {
        const { repulsion, spring, restLength, gravity, damping } = this.params;
        const n = this.nodes.length;
        // Pairwise repulsion.
        for (let i = 0; i < n; i++) {
            const a = this.nodes[i];
            for (let j = i + 1; j < n; j++) {
                const b = this.nodes[j];
                let dx = a.x - b.x;
                let dy = a.y - b.y;
                let dist2 = dx * dx + dy * dy;
                if (dist2 < 0.01) {
                    // Deterministic tiny separation for coincident nodes.
                    dx = (i - j) * 0.01 + 0.01;
                    dy = 0.01;
                    dist2 = dx * dx + dy * dy;
                }
                const force = repulsion / dist2;
                const dist = Math.sqrt(dist2);
                const fx = (dx / dist) * force;
                const fy = (dy / dist) * force;
                a.vx += fx;
                a.vy += fy;
                b.vx -= fx;
                b.vy -= fy;
            }
        }
        // Spring attraction along edges.
        for (const [ai, bi] of this.edges) {
            const a = this.nodes[ai];
            const b = this.nodes[bi];
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const dist = Math.sqrt(dx * dx + dy * dy) || 0.01;
            const force = spring * (dist - restLength);
            const fx = (dx / dist) * force;
            const fy = (dy / dist) * force;
            a.vx += fx;
            a.vy += fy;
            b.vx -= fx;
            b.vy -= fy;
        }
        // Gravity toward the origin + integrate with damping, with a speed clamp so the layout
        // cannot explode off-screen for large graphs.
        for (const node of this.nodes) {
            node.vx = (node.vx - node.x * gravity) * damping;
            node.vy = (node.vy - node.y * gravity) * damping;
            const speed = Math.hypot(node.vx, node.vy);
            if (speed > MAX_SPEED) {
                node.vx = (node.vx / speed) * MAX_SPEED;
                node.vy = (node.vy / speed) * MAX_SPEED;
            }
            node.x += node.vx;
            node.y += node.vy;
        }
    }
    /** Axis-aligned bounding box of all node positions (after layout). */
    bounds() {
        let minX = Infinity;
        let minY = Infinity;
        let maxX = -Infinity;
        let maxY = -Infinity;
        for (const node of this.nodes) {
            if (node.x < minX)
                minX = node.x;
            if (node.x > maxX)
                maxX = node.x;
            if (node.y < minY)
                minY = node.y;
            if (node.y > maxY)
                maxY = node.y;
        }
        if (!Number.isFinite(minX))
            return { minX: 0, minY: 0, maxX: 0, maxY: 0 };
        return { minX, minY, maxX, maxY };
    }
    /** Run `n` ticks. */
    run(n) {
        for (let i = 0; i < n; i++)
            this.step();
    }
    /**
     * Recenter the settled layout on the origin and scale it so its bounding box fits within
     * `span` pixels. Called once after settling so a fixed initial zoom frames the whole graph
     * regardless of node count, and velocities are zeroed so it stays put.
     */
    normalize(span) {
        const { minX, minY, maxX, maxY } = this.bounds();
        const cx = (minX + maxX) / 2;
        const cy = (minY + maxY) / 2;
        const scale = span / Math.max(maxX - minX, maxY - minY, 1);
        for (const node of this.nodes) {
            node.x = (node.x - cx) * scale;
            node.y = (node.y - cy) * scale;
            node.vx = 0;
            node.vy = 0;
        }
    }
    /**
     * Collision relaxation: push apart any pair of nodes closer than `minDist`, for up to
     * `iterations` passes (stopping early once nothing overlaps). Run after settling so nodes — and
     * thus their name labels — never sit on top of each other in any view, while preserving the
     * settled structure's relative arrangement (it only translates nodes). Ends recentered.
     */
    relax(minDist, iterations) {
        const n = this.nodes.length;
        for (let it = 0; it < iterations; it++) {
            let moved = false;
            for (let i = 0; i < n; i++) {
                const a = this.nodes[i];
                for (let j = i + 1; j < n; j++) {
                    const b = this.nodes[j];
                    let dx = b.x - a.x;
                    let dy = b.y - a.y;
                    let dist = Math.sqrt(dx * dx + dy * dy);
                    if (dist < 1e-6) {
                        // Deterministic separation for coincident nodes.
                        dx = (j - i) * 0.01 + 0.01;
                        dy = 0.01;
                        dist = Math.sqrt(dx * dx + dy * dy);
                    }
                    if (dist < minDist) {
                        const push = (minDist - dist) / 2;
                        const ux = dx / dist;
                        const uy = dy / dist;
                        a.x -= ux * push;
                        a.y -= uy * push;
                        b.x += ux * push;
                        b.y += uy * push;
                        moved = true;
                    }
                }
            }
            if (!moved)
                break; // converged — no overlaps remain
        }
        this.recenter();
    }
    /** Recenter the layout's bounding box on the origin (a translation; preserves distances). */
    recenter() {
        const { minX, minY, maxX, maxY } = this.bounds();
        const cx = (minX + maxX) / 2;
        const cy = (minY + maxY) / 2;
        for (const node of this.nodes) {
            node.x -= cx;
            node.y -= cy;
        }
    }
    position(id) {
        const i = this.index.get(id);
        return i === undefined ? undefined : { x: this.nodes[i].x, y: this.nodes[i].y };
    }
}
/** The artifact level shown at a given zoom — the magnifying glass descending the lens. */
export function lodLevelForZoom(zoom) {
    if (zoom < 0.6)
        return "repo";
    if (zoom < 1.2)
        return "module";
    if (zoom < 2.4)
        return "file";
    return "symbol";
}
/** Nodes visible at a given zoom: artifacts at the current LOD level; tasks once zoomed in. */
export function visibleNodes(snapshot, zoom) {
    const level = lodLevelForZoom(zoom);
    return snapshot.nodes.filter((node) => node.kind === "task" ? zoom >= 1.2 : node.level === level);
}
/** The set of node ids to highlight given the tasks the agent surfaced. */
export function highlightSet(snapshot, taskIds) {
    const tasks = new Set(taskIds);
    const highlight = new Set(taskIds);
    for (const edge of snapshot.edges) {
        if (edge.kind === "task_modifies_artifact" && tasks.has(edge.src)) {
            highlight.add(edge.dst);
        }
    }
    return highlight;
}
//# sourceMappingURL=graph.js.map