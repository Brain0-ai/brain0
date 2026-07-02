/**
 * Risk color fusion — a faithful TypeScript port of the Rust `brain0_model::risk` logic,
 * so the GUI and agent render exactly the same green→red colors and the same
 * a-priori→a-posteriori transition the core computes.
 */

import type { RiskState } from "./types.js";

export type RiskTransition =
  | "pending"
  | "stable"
  | "safe_to_dangerous"
  | "confirmed_dangerous"
  | "overestimated_risk";

export interface Rgb {
  r: number;
  g: number;
  b: number;
}

export interface RiskColor {
  fused: number;
  rgb: Rgb;
  hex: string;
  transition: RiskTransition;
}

const LOW = 0.34;
const HIGH = 0.66;
const APOSTERIORI_EVIDENCE = 0.05;

export function clampUnit(value: number): number {
  return Math.max(0, Math.min(1, value));
}

/** Probabilistic-OR fusion of the two scores (matches the Rust implementation). */
export function fusedScore(risk: RiskState): number {
  const a = clampUnit(risk.apriori);
  const p = clampUnit(risk.aposteriori);
  return 1 - (1 - a) * (1 - p);
}

export function riskTransition(risk: RiskState): RiskTransition {
  const a = clampUnit(risk.apriori);
  const p = clampUnit(risk.aposteriori);
  if (p < APOSTERIORI_EVIDENCE) return "pending";

  const lookedSafe = a < LOW;
  const lookedRisky = a >= HIGH;
  const provedDangerous = p >= HIGH;
  const provedSafe = p < LOW;

  if (lookedSafe && provedDangerous) return "safe_to_dangerous";
  if (lookedRisky && provedDangerous) return "confirmed_dangerous";
  if (lookedRisky && provedSafe) return "overestimated_risk";
  if (lookedSafe && provedSafe) return "stable";
  return provedDangerous ? "confirmed_dangerous" : "stable";
}

function hslToRgb(hDeg: number, s: number, l: number): Rgb {
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const h = hDeg / 60;
  const x = c * (1 - Math.abs((h % 2) - 1));
  let r1 = 0;
  let g1 = 0;
  let b1 = 0;
  const sextant = Math.floor(h);
  if (sextant === 0) [r1, g1, b1] = [c, x, 0];
  else if (sextant === 1) [r1, g1, b1] = [x, c, 0];
  else if (sextant === 2) [r1, g1, b1] = [0, c, x];
  else if (sextant === 3) [r1, g1, b1] = [0, x, c];
  else if (sextant === 4) [r1, g1, b1] = [x, 0, c];
  else [r1, g1, b1] = [c, 0, x];
  const m = l - c / 2;
  return {
    r: Math.round((r1 + m) * 255),
    g: Math.round((g1 + m) * 255),
    b: Math.round((b1 + m) * 255),
  };
}

/** Map a fused score in 0..1 to an RGB on the green→yellow→red sweep. */
export function hueSweepRgb(t: number): Rgb {
  const clamped = clampUnit(t);
  return hslToRgb(120 * (1 - clamped), 1, 0.5);
}

export function toHex({ r, g, b }: Rgb): string {
  const h = (n: number) => n.toString(16).padStart(2, "0");
  return `#${h(r)}${h(g)}${h(b)}`;
}

export function riskColor(risk: RiskState): RiskColor {
  const fused = fusedScore(risk);
  const rgb = hueSweepRgb(fused);
  return { fused, rgb, hex: toHex(rgb), transition: riskTransition(risk) };
}

export function isGoldSignal(risk: RiskState): boolean {
  return riskTransition(risk) === "safe_to_dangerous";
}
