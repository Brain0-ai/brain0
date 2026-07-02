import { test } from "node:test";
import assert from "node:assert/strict";
import { fusedScore, isGoldSignal, riskColor, riskTransition } from "./index.js";

test("fusion is probabilistic-or and bounded", () => {
  assert.equal(fusedScore({ apriori: 0, aposteriori: 0 }), 0);
  assert.ok(Math.abs(fusedScore({ apriori: 1, aposteriori: 0 }) - 1) < 1e-6);
  const both = fusedScore({ apriori: 0.5, aposteriori: 0.5 });
  assert.ok(both > fusedScore({ apriori: 0.5, aposteriori: 0 }));
});

test("gold signal: safe a-priori, dangerous a-posteriori", () => {
  assert.equal(riskTransition({ apriori: 0.1, aposteriori: 0.9 }), "safe_to_dangerous");
  assert.ok(isGoldSignal({ apriori: 0.1, aposteriori: 0.9 }));
  assert.equal(riskTransition({ apriori: 0.9, aposteriori: 0 }), "pending");
});

test("color endpoints are green and red", () => {
  const green = riskColor({ apriori: 0, aposteriori: 0 });
  assert.ok(green.rgb.g > 200 && green.rgb.r < 60);
  const red = riskColor({ apriori: 1, aposteriori: 1 });
  assert.ok(red.rgb.r > 200 && red.rgb.g < 60);
});
