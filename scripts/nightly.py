"""Prepare a nightly changelog using the last published release of its cadence."""

import argparse
from datetime import datetime, timezone
import html
import json
from pathlib import Path
import re
import subprocess


def git(*args, cwd=None):
    return subprocess.check_output(
        ["git", *args], cwd=cwd, text=True, encoding="utf-8"
    ).strip()


def prepare(channel, repository, pages, cwd=None):
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
    commits = git("log", "--reverse", "--format=%H %s",
                  f"{base}..{head}" if base else head, "--", cwd=cwd).splitlines()
    if not commits:
        return head, "", f"No new commits since the previous {channel} nightly.\n"

    url = f"https://github.com/{repository}"
    lines = [f"# {channel.capitalize()} nightly changelog", "",
             f"Built from [`{head}`]({url}/commit/{head}).", ""]
    if base:
        lines += [f"[Changes since {previous['tag_name']}]({url}/compare/{base}...{head})", ""]
    else:
        lines += ["First release for this cadence; includes all commits through this build.", ""]
    for commit in commits:
        sha, _, subject = commit.partition(" ")
        # Commit subjects are data, including Markdown and HTML metacharacters.
        subject = re.sub(r"([\\`*_{}\[\]()#+.!|>~-])", r"\\\1", html.escape(subject))
        lines.append(f"- {subject} ([`{sha}`]({url}/commit/{sha}))")
    date = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    tag = f"nightly-{channel}-{date}-{head[:12]}"
    return head, tag, "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--channel", choices=["daily", "weekly"], required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--releases", type=Path, required=True)
    parser.add_argument("--notes", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    _, tag, notes = prepare(args.channel, args.repository,
                            json.loads(args.releases.read_text(encoding="utf-8")))
    args.notes.write_text(notes, encoding="utf-8")
    with args.output.open("a", encoding="utf-8") as output:
        output.write(f"changed={'true' if tag else 'false'}\ntag={tag}\n")


if __name__ == "__main__":
    main()
