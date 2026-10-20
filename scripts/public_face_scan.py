#!/usr/bin/env python3
"""evorule public-face disclosure scanner (v1.0, 2026-10-20).

Rules: see PUBLIC-FACE.md in repo root. Stdlib only.

Modes:
  public_face_scan.py                          full scan; exit 1 on any hit
  public_face_scan.py --dump                   print baseline lines (current hits)
  public_face_scan.py --ratchet FILE           exit 1 on hits not covered by baseline
  public_face_scan.py --commits-since DATE     scan commit messages since DATE (ISO)
  public_face_scan.py --message-file F         scan one commit message file (hook mode)
"""
import hashlib
import os
import re
import subprocess
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SELF_EXEMPT = {
    "PUBLIC-FACE.md",
    "AGENTS.md",
    os.path.join("scripts", "public_face_scan.py"),
    os.path.join("scripts", "public-face-baseline.txt"),
    os.path.join(".github", "workflows", "public-face.yml"),
}

SKIP_DIRS = {".git", "node_modules", "__pycache__", "target-sc1b", ".obsidian"}
TEXT_EXT = {
    ".md", ".rs", ".py", ".ts", ".tsx", ".js", ".mjs", ".json", ".toml",
    ".yml", ".yaml", ".html", ".htm", ".css", ".txt", ".sh", ".ps1",
    ".bat", ".sql", ".cfg", ".ini", ".xml", ".svg",
}

# (pattern_id, regex) — keep in sync with the rules master (knowledge, 03).
DENY = [
    ("internal-obs-id", re.compile(r"\bO-\d{2,3}\b")),
    ("internal-lm-id", re.compile(r"\bLM-\d{1,3}\b")),
    ("internal-design-id", re.compile(r"\bdesign-\d{1,3}\b")),
    ("internal-doc-id", re.compile(r"\d{1,3}\s*号档")),
    ("private-repo-path", re.compile(r"(?i)D:[\\/]+knowledge")),
    ("private-repo-path", re.compile(r"(?i)knowledge[\\/](论文|2-Projects|3-Topics|确定性)")),
    ("secret-like-key", re.compile(r"\bsk-[A-Za-z0-9_-]{20,}")),
]


def is_text(path):
    if os.path.splitext(path)[1].lower() not in TEXT_EXT:
        return False
    if os.path.getsize(path) > 2 * 1024 * 1024:
        return False
    try:
        with open(path, "rb") as f:
            return b"\x00" not in f.read(4096)
    except OSError:
        return False


def scan_text(text):
    found = []
    for pid, rx in DENY:
        for m in rx.finditer(text):
            found.append((pid, m.group(0)))
    return found


def iter_files():
    for dirpath, dirnames, filenames in os.walk(REPO_ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, REPO_ROOT).replace(os.sep, "/")
            if rel in SELF_EXEMPT or not is_text(full):
                continue
            yield rel, full


def scan_tree():
    """Yield baseline-form lines: pid|relpath|md5(line)."""
    hits = []
    for rel, full in iter_files():
        try:
            with open(full, "r", encoding="utf-8", errors="replace") as f:
                for lineno, line in enumerate(f, 1):
                    for pid, _ in scan_text(line):
                        digest = hashlib.md5(line.strip().encode("utf-8", "replace")).hexdigest()
                        hits.append("%s|%s|%s" % (pid, rel, digest))
        except OSError:
            continue
    return sorted(set(hits))


def load_baseline(path):
    if not os.path.exists(path):
        return set()
    with open(path, "r", encoding="utf-8") as f:
        return {ln.strip() for ln in f if ln.strip() and not ln.startswith("#")}


def git_log_since(date):
    out = subprocess.run(
        ["git", "log", "--since=" + date, "--no-merges",
         "--format=%H%x00%s%x00%b", "-z"],
        cwd=REPO_ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    bad = []
    for record in out.stdout.split("\0"):
        parts = record.split("\x00", 2)
        if len(parts) < 2 or not parts[0].strip():
            continue
        sha, subject = parts[0], parts[1]
        body = parts[2] if len(parts) > 2 else ""
        for pid, _ in scan_text(subject + "\n" + body):
            bad.append("%s %s" % (sha[:10], pid))
    return bad


def main(argv):
    if "--dump" in argv:
        for ln in scan_tree():
            print(ln)
        return 0
    if "--message-file" in argv:
        i = argv.index("--message-file")
        with open(argv[i + 1], "r", encoding="utf-8", errors="replace") as f:
            hits = scan_text(f.read())
        if hits:
            print("REJECTED by public-face rules: " + ", ".join(sorted({p for p, _ in hits})))
            print("See PUBLIC-FACE.md. Rewrite the message without internal ids/paths/secrets.")
            return 1
        return 0
    if "--commits-since" in argv:
        i = argv.index("--commits-since")
        bad = git_log_since(argv[i + 1])
        if bad:
            print("commit-message violations since deploy:")
            for b in bad:
                print("  " + b)
            return 1
        return 0
    hits = scan_tree()
    if "--ratchet" in argv:
        i = argv.index("--ratchet")
        base = load_baseline(os.path.join(REPO_ROOT, argv[i + 1]))
        new = [h for h in hits if h not in base]
        stale = [h for h in sorted(base) if h not in set(hits)]
        if new:
            print("NEW public-face violations (not in baseline):")
            for h in new:
                print("  " + h)
            print("Fix them, or shrink-then-freeze via --dump if intentional.")
            return 1
        if stale:
            print("info: %d baseline entries no longer present; consider shrinking baseline." % len(stale))
        return 0
    if hits:
        print("public-face violations: %d" % len(hits))
        for h in hits:
            print("  " + h)
        return 1
    print("public-face: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
