/**
 * Recency-aware ranking.
 *
 * The ranking combines semantic similarity, a recency bias, and risk. The recency term is
 * a hard requirement: at equal semantic relevance, the *more recent* change on the symbol
 * must be surfaced first, because it is statistically the more likely cause of a bug.
 */

export interface RankCandidate {
  taskId: string;
  /** Semantic similarity to the query, 0..1. */
  cosine: number;
  /** RFC3339 timestamp of the task. */
  createdAt: string;
  /** Optional fused risk of the change, 0..1. */
  risk?: number;
}

export interface RankWeights {
  semantic: number;
  recency: number;
  risk: number;
}

export interface RankOptions {
  now?: Date;
  /** Half-life of the recency decay, in days. */
  halfLifeDays?: number;
  weights?: Partial<RankWeights>;
}

export interface Ranked extends RankCandidate {
  recency: number;
  score: number;
}

const DEFAULT_WEIGHTS: RankWeights = { semantic: 0.6, recency: 0.3, risk: 0.1 };
const DEFAULT_HALF_LIFE_DAYS = 30;

const MS_PER_DAY = 86_400_000;

/** Recency decay in 0..1 (1 = now, 0.5 at one half-life ago). */
export function recencyDecay(createdAt: string, now: Date, halfLifeDays: number): number {
  const ts = Date.parse(createdAt);
  if (Number.isNaN(ts)) return 0;
  const ageDays = Math.max(0, (now.getTime() - ts) / MS_PER_DAY);
  return Math.pow(0.5, ageDays / halfLifeDays);
}

/** Rank candidates by a weighted blend of semantic similarity, recency, and risk. */
export function recencyAwareRank(candidates: RankCandidate[], options: RankOptions = {}): Ranked[] {
  const now = options.now ?? new Date();
  const halfLife = options.halfLifeDays ?? DEFAULT_HALF_LIFE_DAYS;
  const w: RankWeights = { ...DEFAULT_WEIGHTS, ...options.weights };

  const ranked = candidates.map((c) => {
    const recency = recencyDecay(c.createdAt, now, halfLife);
    const score = w.semantic * c.cosine + w.recency * recency + w.risk * (c.risk ?? 0);
    return { ...c, recency, score };
  });

  // Sort by score desc; break exact ties toward the more recent change.
  ranked.sort((a, b) => b.score - a.score || b.recency - a.recency);
  return ranked;
}
