# -*- coding: utf-8 -*-
"""check_deps_graph.py — evorule dependency-graph machine guard.

Parses the ecosystem Cargo.toml files (workspace members + crate
[dependencies] sections) and validates every evorule-* dependency edge
against the constraint snapshot deps-graph.json.

Violation / expired migration exemption -> exit 1 (CI required check /
local pre-push hook entry point).

Modes:
  full scan (default)  scan every registered checkout; roots are taken from
                       graph.repos[].root entries and/or a knowledge-local
                       local-roots.json sitting next to the graph file (this
                       file never ships to CI copies)
  --self               scan the current checkout only; the repo identity is
                       taken from --repo, or inferred from .git/config remote
                       URL (CI checkouts), falling back to the cwd basename.
                       Designed for CI where the graph's local root paths do
                       not exist.

Usage:
  python check_deps_graph.py [--graph deps-graph.json] [--report out.md]
  python check_deps_graph.py --self [--repo <name>] [--graph ...] [--report ...]
"""
import argparse
import io
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_GRAPH = os.path.join(HERE, "deps-graph.json")


def load_graph(path):
    return json.load(io.open(path, encoding="utf-8"))


def read(path):
    return io.open(path, encoding="utf-8", errors="replace").read()


def workspace_members(root):
    """Return (members_with_path, is_workspace). Members keep original path
    segments (core/criticality style)."""
    p = os.path.join(root, "Cargo.toml")
    if not os.path.isfile(p):
        return [], False
    text = read(p)
    is_ws = "[workspace]" in text
    members = []
    if is_ws:
        seg = text.split("[workspace]", 1)[1]
        seg = seg.split("\n[", 1)[0]
        m = re.search(r"members\s*=\s*\[(.*?)\]", seg, re.S)
        if m:
            members = [r.replace("\\", "/").strip("/") for r in re.findall(r'"([^"]+)"', m.group(1))]
    return members, is_ws


def crate_evorule_deps(crate_toml_path):
    """Extract evorule-* dependency names from the [dependencies] section
    (dev-dependencies excluded)."""
    text = read(crate_toml_path)
    deps = set()
    m = re.search(r"^\[dependencies\]\s*$", text, re.M)
    if m:
        seg = text[m.end():]
        nxt = re.search(r"^\[", seg, re.M)
        if nxt:
            seg = seg[:nxt.start()]
        deps |= set(re.findall(r'^\s*(evorule-[\w-]+)\s*=', seg, re.M))
        deps |= set(re.findall(r'(evorule-[\w-]+)\s*=\s*\{[^}]*path\s*=', seg))
    return deps


def iter_crate_tomls(root):
    """For workspace repos: Cargo.toml under each member path; for non-workspace
    repos: the root Cargo.toml."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        if "Cargo.toml" in filenames:
            rel = os.path.relpath(dirpath, root).replace("\\", "/")
            if rel.startswith(("target", ".git", "node_modules")) or "/target" in rel:
                continue
            out.append(os.path.join(dirpath, "Cargo.toml"))
        # prune: skip target/.git/deep dependency trees
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git", "node_modules")][:50]
    return out


def infer_repo_name():
    """Repo identity for --self mode: infer from the checkout's .git/config
    remote URL; fall back to the cwd basename (an explicit --repo flag, when
    given, wins before this helper is consulted)."""
    git_path = os.path.join(os.getcwd(), ".git")
    config_candidates = []
    if os.path.isdir(git_path):
        config_candidates.append(os.path.join(git_path, "config"))
    elif os.path.isfile(git_path):
        # worktree: .git is a file pointing at the worktree gitdir; the
        # shared config lives in the common dir
        try:
            gitdir = read(git_path).split("gitdir:", 1)[1].strip()
            common = os.path.join(gitdir, "commondir")
            cfg = os.path.join(gitdir, "config")
            if os.path.isfile(common):
                cfg = os.path.normpath(os.path.join(gitdir, read(common).strip(), "config"))
            config_candidates.append(cfg)
        except Exception:
            pass
    for cfg in config_candidates:
        if os.path.isfile(cfg):
            for line in read(cfg).splitlines():
                m = re.search(r"url\s*=\s*\S*[:/](evorule-[\w-]+?)(?:\.git)?/?\s*$", line)
                if m:
                    return m.group(1)
    return os.path.basename(os.path.abspath(os.getcwd()))


def edge_matches_exemption(graph, repo, rel, dep):
    """Return the exemption item whose structured `match` block covers this
    edge, or None. Only items carrying a `match` object are machine-checked;
    prose-only exemptions stay process-managed."""
    for e in graph.get("migration_exemptions", {}).get("items", []):
        m = e.get("match")
        if not m:
            continue
        if m.get("repo") != repo:
            continue
        if m.get("path_prefix") and not rel.startswith(m["path_prefix"]):
            continue
        if dep not in m.get("deps", []):
            continue
        return e
    return None


def edge_violations(graph, repos):
    """Main check: scan the given repos ({name: {"layer", "root"}}),
    return (violations, exemption_hits, scanned)."""
    violations, exemption_hits, scanned = [], [], []
    forbidden = graph["forbidden_edges"]

    for repo, meta in sorted(repos.items()):
        root = meta["root"]
        members, is_ws = workspace_members(root)
        scanned.append("%s (workspace=%s, members=%d)" % (repo, is_ws, len(members)))
        # knowledge governance repo: skip dependency-edge scanning (historical
        # experiment crates are not part of the enforcement surface)
        if meta["layer"] == "knowledge":
            scanned.append("  (knowledge layer: dependency scan skipped)")
            continue
        # server locked members (members carry the parent path)
        if repo == "evorule-server":
            lock = graph["server_locked_members"]
            for m in members:
                leaf = m.split("/")[-1]
                parent = m.split("/")[0] if "/" in m else ""
                if parent == "core" and leaf in lock["core_forbidden_return"]:
                    active = [e for e in graph["migration_exemptions"]["items"]
                              if e["id"] in ("EX-M2-criticality", "EX-S2-rule-domain") and leaf in e["scope"]]
                    rec = "evorule-server member core/%s" % leaf
                    if active:
                        exemption_hits.append({"item": rec, "exemption": active[0]["id"], "expires_with": active[0]["expires_with"]})
                    else:
                        violations.append("%s: forbidden return (locked member, no active exemption)" % rec)
        # dependency edges
        for toml in iter_crate_tomls(root):
            rel = os.path.relpath(toml, root).replace("\\", "/")
            deps = crate_evorule_deps(toml)
            # IRON RULE exceptions: canonical repo's own internal members are
            # legal; the server host consuming canonical is legal
            in_canonical_self = (repo == "evorule")
            is_server_host = (repo == "evorule-server" and not rel.startswith("plugins/"))
            for dep in sorted(deps):
                edge = "%s:%s -> %s" % (repo, rel, dep)
                scanned.append("  edge " + edge)
                ok = False
                for rule in forbidden:
                    hit_from = False
                    for tok in rule["from"]:
                        if tok.startswith("*except:"):
                            ex = [e.strip() for e in tok[len("*except:"):].split(",")]
                            hit_from = repo not in ex and not (repo == "evorule" and "evorule(自身内部件)" in ex)
                            if hit_from:
                                break
                        elif re.fullmatch(r"[\w-]+", tok):
                            hit_from = (repo == tok)
                    if not hit_from:
                        continue
                    to = rule["to"]
                    if dep in to or any(re.fullmatch(r.replace("evorule-*", r"evorule-[\w-]+"), dep) for r in to):
                        # IRON RULE exemptions: canonical repo's own internal
                        # members; server host main and core members (the
                        # plugins/ subtree is NOT exempt)
                        if in_canonical_self:
                            continue
                        if "IRON RULE" in rule["rule"] and is_server_host:
                            continue
                        # migration-period exemption (structured match):
                        # visible as an active exemption hit instead of a
                        # violation; each item is removed at its batch closure
                        exempt = edge_matches_exemption(graph, repo, rel, dep)
                        if exempt:
                            exemption_hits.append({
                                "item": edge,
                                "exemption": exempt["id"],
                                "expires_with": exempt["expires_with"],
                            })
                            ok = True
                            continue
                        violations.append("%s — %s" % (edge, rule["rule"]))
                        ok = True
                if not ok:
                    scanned.append("    (allowed-or-new: record for graph review)")
    return violations, exemption_hits, scanned


def main():
    ap = argparse.ArgumentParser(description="evorule dependency-graph machine guard")
    ap.add_argument("--graph", help="path to deps-graph.json (default: alongside this script)")
    ap.add_argument("--self", action="store_true",
                    help="scan the current checkout only (CI mode)")
    ap.add_argument("--repo", help="repo name for --self (default: inferred from .git/config or cwd)")
    ap.add_argument("--report", help="also write the full run output to this file")
    args = ap.parse_args()

    graph = load_graph(args.graph or DEFAULT_GRAPH)

    if args.self:
        repo = args.repo or infer_repo_name()
        meta = graph["repos"].get(repo)
        if not meta:
            print("VIOLATION: repo '%s' is not registered in the constraint snapshot (registered: %s)"
                  % (repo, ", ".join(sorted(graph["repos"]))))
            sys.exit(1)
        # --self scans the checkout at cwd: the graph's local root paths do
        # not exist inside a CI checkout.
        repos = {repo: {"layer": meta["layer"], "root": os.getcwd()}}
        mode = "self (%s)" % repo
    else:
        graph_dir = os.path.dirname(os.path.abspath(args.graph or DEFAULT_GRAPH))
        roots_file = os.path.join(graph_dir, "local-roots.json")
        local_roots = {}
        if os.path.isfile(roots_file):
            local_roots = json.load(io.open(roots_file, encoding="utf-8")).get("repos", {})
        repos = {}
        for k, v in graph["repos"].items():
            root = v.get("root") or local_roots.get(k)
            if root and os.path.isdir(root):
                repos[k] = {"layer": v["layer"], "root": root}
        mode = "full scan (%d repos)" % len(repos)

    violations, exemption_hits, scanned = edge_violations(graph, repos)
    lines = ["=== mode: %s ===" % mode, "=== scanned ==="]
    lines.extend(scanned)
    lines.extend(["", "=== migration exemptions (active) ==="])
    if exemption_hits:
        lines.extend("%s  [%s expires_with %s]" % (e["item"], e["exemption"], e["expires_with"]) for e in exemption_hits)
    else:
        lines.append("(none)")
    lines.extend(["", "=== violations ==="])
    if violations:
        lines.extend("VIOLATION: " + v for v in violations)
        lines.extend(["", "RESULT: FAIL (%d)" % len(violations)])
        out = "\n".join(lines)
        print(out)
        if args.report:
            io.open(args.report, "w", encoding="utf-8").write(out + "\n")
        sys.exit(1)
    lines.extend(["(none)", "", "RESULT: PASS"])
    out = "\n".join(lines)
    print(out)
    if args.report:
        io.open(args.report, "w", encoding="utf-8").write(out + "\n")
    sys.exit(0)


if __name__ == "__main__":
    main()
