#!/usr/bin/env python3
"""Transcribe one image of handwritten or printed maths into Org-flavoured LaTeX.

Called by `runtime/modes/mathsync.lisp', which owns all the policy — which image,
when, and where the answer goes. This script is the one part that has to be
somewhere other than Lisp: the editor's ECL image has no HTTP client and no JSON
*reader* (`rpc.lisp' encodes, and the decoding half lives in Rust for LSP), so
doing this in Lisp would mean writing base64, TLS and a JSON parser first.
Python's standard library has all three and is already a dependency of this
project — `math-code.lisp' builds venvs with it.

Nothing is installed: urllib, json and base64 are stdlib, so there is no venv, no
pip, and no network call except the one to OpenRouter.

Stdout is the transcription and nothing else. Every failure exits non-zero with a
one-line reason on stderr, because the caller shows that line in the status bar.
"""

import base64
import json
import mimetypes
import os
import sys
import urllib.error
import urllib.request

ENDPOINT = "https://openrouter.ai/api/v1/chat/completions"

# Org's own delimiters, and they are not a matter of taste here: `latex_fragments'
# in crates/syntax/src/org.rs scans for `$...$', `\(...\)', `\[...\]' and
# `\begin{env}...\end{env}', so a reply fenced in ```latex or written with $$...$$
# is a reply the editor will not draw. The last rule is the one models break most.
PROMPT = """\
Transcribe the mathematics in this image into Org-mode LaTeX.

Rules:
- Inline maths goes in \\( ... \\). Display maths goes in \\[ ... \\].
- Multi-line derivations go in \\begin{align} ... \\end{align}.
- Any prose in the image stays prose, outside the maths delimiters.
- Preserve the structure: numbered steps stay numbered steps.
- Output ONLY the transcription. No preamble, no commentary, no ``` fences.
- Never use $$ ... $$ — Org does not read it.

If the image contains no mathematics, transcribe whatever text it does contain.
"""


def die(msg):
    sys.stderr.write(f"{msg}\n")
    raise SystemExit(1)


def main():
    if len(sys.argv) != 4:
        die("usage: mathsync_transcribe.py IMAGE MODEL KEYFILE")
    image, model, keyfile = sys.argv[1:]

    keyfile = os.path.expanduser(keyfile)
    try:
        with open(keyfile) as f:
            key = f.read().strip()
    except OSError as e:
        die(f"cannot read key file {keyfile}: {e}")
    if not key:
        die(f"key file {keyfile} is empty")

    try:
        with open(image, "rb") as f:
            blob = f.read()
    except OSError as e:
        die(f"cannot read image {image}: {e}")
    mime = mimetypes.guess_type(image)[0] or "image/png"

    body = json.dumps(
        {
            "model": model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": PROMPT},
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": f"data:{mime};base64,"
                                + base64.b64encode(blob).decode()
                            },
                        },
                    ],
                }
            ],
        }
    ).encode()

    request = urllib.request.Request(
        ENDPOINT,
        data=body,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            # OpenRouter attributes calls by these two and they are optional;
            # they are here so this shows up as zemacs in the dashboard rather
            # than as an anonymous script.
            "HTTP-Referer": "https://github.com/zemacs",
            "X-Title": "zemacs mathsync",
        },
    )

    try:
        with urllib.request.urlopen(request, timeout=180) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace").replace("\n", " ")[:300]
        # 404 on a model slug is the failure worth naming, because it is the one
        # a config typo produces and it looks nothing like a network problem.
        die(f"openrouter HTTP {e.code} ({model}): {detail}")
    except Exception as e:
        die(f"openrouter: {e}")

    # An error can arrive with a 200, which is why this is checked before the
    # happy path is read.
    if isinstance(payload, dict) and payload.get("error"):
        err = payload["error"]
        die(f"openrouter: {err.get('message', json.dumps(err))[:300]}")

    try:
        text = payload["choices"][0]["message"]["content"]
    except (KeyError, IndexError, TypeError):
        die(f"unexpected reply: {json.dumps(payload)[:300]}")

    if not text or not text.strip():
        die("model returned nothing")

    sys.stdout.write(text.strip() + "\n")


if __name__ == "__main__":
    main()
