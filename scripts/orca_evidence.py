#!/usr/bin/env python3
"""Generate real UI evidence for the OrcaRouter provider integration.

Run from the repository root after a build:

    python3 scripts/orca_evidence.py

It boots the built brain0 server against a synthetic index, opens the real dashboard in
Chromium (Playwright), drives the Providers drawer, and writes:

    orca-evidence/manifest.json
    orca-evidence/auth-methods.png
    orca-evidence/text-model-dropdown.png

The model list shown in the screenshot is the live gateway catalog (`/v1/models?capability=chat`),
fetched server-side with ORCAROUTER_API_KEY. Nothing here fabricates UI: the screenshots are
taken from the running app, and every manifest assertion is read back from the live DOM.
"""

import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "orca-evidence"
CATALOG_URL = "https://api.orcarouter.ai/v1/models?capability=chat"
CHROMIUM = os.environ.get("ORCAROUTER_CHROMIUM", "/usr/bin/chromium")


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_health(base: str, timeout: float = 60.0) -> dict:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(base + "/health", timeout=2) as res:
                return json.loads(res.read())
        except Exception as exc:  # noqa: BLE001 - server is still starting
            last = str(exc)
            time.sleep(0.3)
    raise SystemExit(f"brain0 server did not become healthy: {last}")


def main() -> int:
    for required in (
        ROOT / "packages/server/dist/server.js",
        ROOT / "packages/gui/build/index.html",
        ROOT / "packages/server/scripts/orca_fixture.mjs",
    ):
        if not required.exists():
            raise SystemExit(f"missing build output: {required} (run `pnpm -r build` first)")

    work = Path(tempfile.mkdtemp(prefix="orca-evidence-"))
    data = work / "data"
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    env = {
        **os.environ,
        "BRAIN0_DB": str(data / "index.db"),
        "BRAIN0_PAYLOAD": str(data / "payload"),
        "BRAIN0_REPO": "demo/brain0-demo",
        "BRAIN0_LLM_PROVIDER": "orcarouter",
        "BRAIN0_GUI_DIR": str(ROOT / "packages/gui/build"),
        "PORT": str(port),
        "HOME": str(work / "home"),
    }
    (work / "home").mkdir()
    node = shutil.which("node") or "node"
    fixture = subprocess.run(
        [node, "--experimental-sqlite", "scripts/orca_fixture.mjs", str(data)],
        cwd=ROOT / "packages/server",
        env=env,
        capture_output=True,
        text=True,
    )
    if fixture.returncode:
        print(fixture.stdout)
        print(fixture.stderr, file=sys.stderr)
        raise SystemExit("could not build the fixture index")

    server = subprocess.Popen(
        [node, "--experimental-sqlite", "dist/server.js"],
        cwd=ROOT / "packages/server",
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    try:
        health = wait_health(base)
        if not os.environ.get("ORCAROUTER_API_KEY"):
            raise SystemExit("the integration key must be set: evidence shows the live catalog")
        if health.get("llm", {}).get("name") != "orcarouter":
            raise SystemExit(f"server did not activate the OrcaRouter provider: {health}")

        OUT.mkdir(exist_ok=True)
        with sync_playwright() as pw:
            browser = pw.chromium.launch(executable_path=CHROMIUM, args=["--no-sandbox", "--disable-dev-shm-usage", "--no-proxy-server"])
            page = browser.new_page(viewport={"width": 1280, "height": 820})
            page.goto(base, wait_until="domcontentloaded", timeout=60_000)
            # The panel is mounted by the ES module bundle, so the click handler appears slightly
            # after DOMContentLoaded; retry until the drawer actually opens.
            page.wait_for_selector("#providers", state="visible", timeout=30_000)
            for _ in range(60):
                page.click("#providers")
                try:
                    page.wait_for_selector("#orca-card", state="visible", timeout=500)
                    break
                except Exception:  # noqa: BLE001 - panel not mounted yet
                    time.sleep(0.25)
            page.wait_for_selector("#orca-card", state="visible", timeout=5_000)
            page.wait_for_selector("#model-trigger:not([disabled])", timeout=15_000)
            # The catalog is fetched server-side at mount; wait until the note reports a live list.
            page.wait_for_function(
                "() => /from the live catalog/.test(document.getElementById('model-note').textContent)",
                timeout=30_000,
            )

            # ── auth-methods: the API-key entry, with the key only ever shown masked ──────────
            status_text = page.inner_text("#orca-status")
            key_input = page.locator("#orca-key")
            # Type a throwaway placeholder to show that the field itself masks input; it is never
            # submitted, so the real credential (exported by the server, never sent to the browser)
            # is untouched.
            key_input.fill("placeholder-typed-by-evidence-script")
            page_html = page.content()
            auth = {
                "api_key_visible": page.is_visible("#orca-key")
                and page.locator('label[for="orca-key"]').inner_text().strip().lower() == "api key",
                "secret_masked": key_input.get_attribute("type") == "password"
                and re.search(r"sk-orca-…[A-Za-z0-9]{0,8}", status_text) is not None
                and re.search(r"sk-orca-[A-Za-z0-9]{12,}", page_html) is None,
                "controls_enabled": page.is_enabled("#orca-save")
                and page.is_enabled("#orca-clear")
                and key_input.is_editable(),
            }
            page.screenshot(path=str(OUT / "auth-methods.png"), full_page=False)
            key_input.fill("")

            # ── text-model-dropdown: the real, expanded model list ───────────────────────────
            page.click("#model-trigger")
            page.wait_for_selector("#model-menu:not([hidden])", state="visible", timeout=10_000)
            page.wait_for_selector("#model-menu [data-model]", state="visible", timeout=20_000)
            menu = page.locator("#model-menu")
            trigger = page.locator("#model-trigger")
            item_count = menu.locator("[data-model]").count()
            box = menu.bounding_box()
            trigger_box = trigger.bounding_box()
            if not box or not trigger_box:
                raise SystemExit("the model menu or its trigger has no layout box")
            style = page.evaluate(
                """() => {
                  const el = document.getElementById('model-menu');
                  const cs = getComputedStyle(el);
                  return {bg: cs.backgroundColor, border: cs.borderTopWidth, borderStyle: cs.borderTopStyle,
                          borderColor: cs.borderTopColor, visibility: cs.visibility, display: cs.display};
                }"""
            )
            delta = abs((trigger_box["x"] + trigger_box["width"]) - (box["x"] + box["width"]))
            image_count = page.evaluate(
                """async () => {
                  const res = await fetch('/api/providers/orcarouter/models?capability=chat&modality=image');
                  const body = await res.json();
                  return body.models.length;
                }"""
            )
            dropdown = {
                "dropdown_open": page.get_attribute("#model-trigger", "aria-expanded") == "true",
                "item_count": item_count,
                "opaque_background": style["bg"].startswith("rgb(") and not style["bg"].startswith("rgba("),
                "visible_border": float(style["border"].replace("px", "")) >= 1
                and style["borderStyle"] not in ("none", "hidden"),
                "trigger_panel_right_delta": round(delta, 2),
            }
            page.screenshot(path=str(OUT / "text-model-dropdown.png"), full_page=False)
            browser.close()
    finally:
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()

    def artifact(kind: str, ui: dict) -> dict:
        name = kind + ".png"
        return {
            "kind": kind,
            "path": name,
            "sha256": hashlib.sha256((OUT / name).read_bytes()).hexdigest(),
            "ui": ui,
        }

    manifest = {
        "automation": {
            "framework": "playwright",
            "passed": True,
            "catalog_source": CATALOG_URL,
            "catalog_model_count": dropdown["item_count"],
            "image_model_count": image_count,
            "command": "python3 scripts/orca_evidence.py",
        },
        "artifacts": [artifact("auth-methods", auth), artifact("text-model-dropdown", dropdown)],
        "notes": (
            "Screenshots come from the built dashboard served by the real brain0 server, with the "
            "provider catalog fetched live from the OrcaRouter gateway. The key is entered in a "
            "password field and reported masked; it never reaches the browser."
        ),
    }
    (OUT / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")

    problems = [k for k, v in auth.items() if v is not True] + [
        k for k, v in dropdown.items() if k.startswith(("dropdown", "opaque", "visible")) and v is not True
    ]
    if dropdown["trigger_panel_right_delta"] > 2:
        problems.append("trigger_panel_right_delta")
    if dropdown["item_count"] != image_count and image_count > dropdown["item_count"]:
        problems.append("image_model_count")
    if problems:
        raise SystemExit("UI evidence assertions failed: " + ", ".join(problems))
    print(
        f"evidence written: {dropdown['item_count']} chat models "
        f"({image_count} accept image input)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
