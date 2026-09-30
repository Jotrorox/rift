"""Prepare a nightly tag using the last published release of its cadence."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess


def git(*args, cwd=None):
    return subprocess.check_output(
        ["git", *args], cwd=cwd, text=True, encoding="utf-8"
    ).strip()


def prepare(channel, pages, cwd=None):
    head = git("rev-parse", "HEAD", cwd=cwd)
    releases = [
        release for page in pages for release in page
        if not release["draft"]
        and release["published_at"]
        and release["tag_name"].startswith(f"nightly-{channel}-")
    ]
    previous = max(releases, key=lambda release: release["published_at"], default=None)
    base = None
    if previous:
        base = git("rev-parse", f"refs/tags/{previous['tag_name']}^{{commit}}", cwd=cwd)
        # Fail rather than silently omit changes after a history rewrite.
        subprocess.run(["git", "merge-base", "--is-ancestor", base, head],
                       cwd=cwd, check=True)
    if base == head:
        return head, ""

    date = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    tag = f"nightly-{channel}-{date}-{head[:12]}"
    return head, tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--channel", choices=["daily", "weekly"], required=True)
    parser.add_argument("--releases", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    _, tag = prepare(args.channel, json.loads(args.releases.read_text(encoding="utf-8")))
    with args.output.open("a", encoding="utf-8") as output:
        output.write(f"changed={'true' if tag else 'false'}\ntag={tag}\n")


if __name__ == "__main__":
    main()
