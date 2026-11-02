#!/usr/bin/env python3
"""Commit-message structural lint (layer 1), single-master conversion batch.

Rules: structural only (type/subject/CJK/banned ids). Disclosure scanning
(internal ids/paths/secrets) stays in public_face_scan.py (layer 2); the two
are orthogonal and intentionally not merged.

Modes:
  commit_lint.py --message-file F       scan one commit message (hook mode)
  commit_lint.py --range BASE..HEAD     scan commits in range (PR mode)
  commit_lint.py --commits-since DATE   scan commits since DATE (push ratchet)

Degraded mode (no python / missing script) silently skips — enforced at the
hook wrapper and CI layer, never blocks development by absence.
"""
import re
import subprocess
import sys

# D3: subject hard limit (50/72 convention)
SUBJECT_LIMIT = 72

# R1/D4: type whitelist, mandatory; merge/revert commits exempt.
TYPE_RX = re.compile(
    r"^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)"
    r"(\(.+\))?(!)?: .+"
)
MERGE_RX = re.compile(r"^Merge(?:\s|\b)")
REVERT_RX = re.compile(r'^Revert "')

# R5/B4: CJK anywhere in subject/body/footer.
CJK_RX = re.compile(r"[\u4e00-\u9fff\u3400-\u4dbf\uf900-\ufaff]")

# D1: banned internal board ids in commit messages (extensible word list).
BANNED = [
    (r"\bB-\d+\b", "internal board id (letter-number form)"),
]

WARNINGS = [
    # R4: subject should start uppercase and not end with a period.
    ("subject ends with a period", lambda s: s.rstrip().endswith(".")),
    ("subject does not start with an uppercase letter",
     lambda s: s[:1].isalpha() and not s[:1].isupper()),
]


def _violations_for_message(text):
    """Return (errors, warnings) lists for one commit message."""
    errors, warns = [], []
    lines = text.splitlines()
    subject = lines[0].rstrip() if lines else ""
    body = "\n".join(lines[1:])

    if MERGE_RX.match(subject) or REVERT_RX.match(subject):
        return errors, warns

    if not TYPE_RX.match(subject):
        errors.append(
            "subject must match '<type>(<scope>)?: <summary>' with type in "
            "feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert "
            "(merge/revert exempt); got: %s" % (subject[:80] or "<empty>"))
    if len(subject) > SUBJECT_LIMIT:
        errors.append("subject longer than %d chars (%d)" % (SUBJECT_LIMIT, len(subject)))
    if CJK_RX.search(subject):
        errors.append("subject contains CJK characters")
    if CJK_RX.search(body):
        errors.append("body/footer contains CJK characters")
    for rx, label in BANNED:
        if re.search(rx, text):
            errors.append("banned token: %s" % label)

    for label, fn in WARNINGS:
        if fn(subject):
            warns.append(label)

    for ln in lines[1:]:
        stripped = ln.lstrip()
        if stripped.startswith("#"):
            continue
        if len(ln) > SUBJECT_LIMIT:
            warns.append("body line longer than %d columns: %.40s..." % (SUBJECT_LIMIT, ln))
    return errors, warns


def _format(sha, errors, warns):
    out = []
    if errors:
        out.append("%s ERROR:" % sha)
        out.extend("  " + e for e in errors)
    if warns:
        out.append("%s warn:" % sha)
        out.extend("  " + w for w in warns)
    return out


def scan_messages(messages):
    """messages: iterable of (sha, text). Returns exit code (warnings never fail)."""
    errored = 0
    for sha, text in messages:
        errors, warns = _violations_for_message(text)
        if errors or warns:
            errored += 1 if errors else 0
            for ln in _format(sha[:10], errors, warns):
                print(ln)
    if errored:
        print("commit-lint: %d commit(s) with errors. Rewrite subject as "
              "'type(scope): english summary', <=72 chars, no CJK, no board ids." % errored)
        return 1
    return 0


def git_messages(args):
    out = subprocess.run(
        ["git", "log", "--no-merges", "--format=%H%x00%s%x00%b", "-z"] + args,
        capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    if out.returncode != 0:
        print("commit-lint: git log failed: %s" % out.stderr.strip())
        sys.exit(2)
    for record in out.stdout.split("\0"):
        parts = record.split("\x00", 2)
        if len(parts) < 2 or not parts[0].strip():
            continue
        yield parts[0], parts[1] + "\n" + (parts[2] if len(parts) > 2 else "")


def main(argv):
    def value(flag):
        return argv[argv.index(flag) + 1] if flag in argv else None

    if "--message-file" in argv:
        with open(value("--message-file"), "r", encoding="utf-8", errors="replace") as f:
            return scan_messages([("HEAD", f.read())])
    rng = value("--range")
    if rng:
        return scan_messages(git_messages([rng]))
    since = value("--commits-since")
    if since:
        return scan_messages(git_messages(["--since=" + since]))
    print("usage: commit_lint.py --message-file F | --range B..H | --commits-since DATE")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
