#!/usr/bin/env python3
"""Regenerate the buzz app's embedded web client from buzz-web/dist.

Run after every `npm run build:hosted` in buzz-web: vite hashes asset filenames, so a
stale list serves a page whose script 404s. From :

    (cd frontends/buzz-web && npm run build:hosted) && python3 scripts/embed-buzz-ui.py
"""
import os, shutil, sys

LABS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(LABS, "frontends", "buzz-web", "dist")
STATIC = os.path.join(LABS, "apps", "buzz", "src", "relay", "static", "ui")
OUT = os.path.join(LABS, "apps", "buzz", "src", "relay", "assets", "ui.rs")

if not os.path.isdir(DIST):
    sys.exit(f"no build at {DIST} — run `npm run build:hosted` in buzz-web first")

with open(os.path.join(DIST, "index.html")) as index:
    if '<meta name="buzz-relay-transport" content="http">' not in index.read():
        sys.exit("hosted Buzz requires HTTP relay transport; run `npm run build:hosted` before embedding")

shutil.rmtree(STATIC, ignore_errors=True)
shutil.copytree(DIST, STATIC)
files = sorted(
    os.path.relpath(os.path.join(r, f), STATIC).replace(os.sep, "/")
    for r, _, fs in os.walk(STATIC)
    for f in fs
)
header = '''//! The buzz web client, embedded.
//!
//! Generated from `buzz-web/dist` by `scripts/embed-buzz-ui.py` — do not edit
//! by hand, and regenerate after every UI build (vite hashes asset names, so a
//! stale list serves a broken page).
//!
//! The app is served by the platform's CONTENT face: the manifest maps
//! `/buzz/ui/` onto the `web-assets` blob with `key_prefix = "ui/"`, so each
//! entry is seeded at boot under `ui/{path}` and the gateway serves the bytes
//! straight from the blob (MIME inferred from the extension). The relay never
//! proxies them.

pub const UI_ASSETS: &[(&str, &[u8])] = &['''
body = "\n".join(
    f'    ("ui/{f}", include_bytes!("../static/ui/{f}")),' for f in files
)
open(OUT, "w").write(f"{header}\n{body}\n];\n")
print(f"embedded {len(files)} objects into {os.path.relpath(OUT, LABS)}")
