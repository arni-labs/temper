#!/usr/bin/env python3
"""Decide what a CI run tests, for .github/workflows/ci.yml.

Lane "full" (push, schedule, a plain dispatch) runs every check as before.
Lane "pr" (a pull request, or a dispatch given changed paths) tests only the
workspace packages a change can reach: the packages whose files changed plus
every package that depends on them (normal, dev and build dependencies all
count), worked out from `cargo metadata`. A change to anything outside a
package that a build or test could read (Cargo.lock, the toolchain, .github/,
test-fixtures/, os-apps/, skills/, scripts/, ...) tests every package; only the
paths in DOCS are known to be read by nothing.

Writes key=value lines to $GITHUB_OUTPUT (stdout when unset):
  lane      full | pr
  mode      full | affected | none
  packages  space-separated package names to test (empty for none)
  server    true when temper-server is among them (DST, GEPA modules)
  rust      false only when nothing but DOCS changed
  shards    JSON matrix of Tests shards for the pr lane
"""

import fnmatch
import json
import os
import subprocess
import sys

# Paths outside every package that no build, test or check reads.
DOCS = [
    "docs/*",
    "*.md",
    "*.pdf",
    "*.png",
    "*.html",
    "LICENSE-*",
    "assets/*",
    "ui/*",
    "packages/*",
    "plugins/*",
    ".agents/*",
    ".codex/*",
    ".cursor/*",
    ".vision/*",
]
SERVER = "temper-server"
# temper-server's tests run with the features the whole workspace turns on for it.
SERVER_FEATURES = "temper-server/observe,temper-server/sim"
# Packages whose tests take long enough to be worth two machines.
HEAVY = {"temper-platform", "temper-store-turso", "temper-cli", "temper-actor-runtime"}


def is_docs(path):
    if "/" not in path:
        return any(fnmatch.fnmatch(path, p) for p in DOCS if "/" not in p)
    return any(fnmatch.fnmatch(path, p) for p in DOCS if "/" in p)


def workspace():
    env = dict(os.environ, RUSTUP_TOOLCHAIN=os.environ.get("RUSTUP_TOOLCHAIN", "stable"))
    out = subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"], env=env)
    meta = json.loads(out)
    root = meta["workspace_root"].rstrip("/") + "/"
    dirs = {}
    for pkg in meta["packages"]:
        rel = os.path.dirname(pkg["manifest_path"])[len(root):]
        dirs[rel + "/"] = pkg["name"]
    names = set(dirs.values())
    dependents = {name: set() for name in names}
    for pkg in meta["packages"]:
        for dep in pkg["dependencies"]:
            if dep["name"] in names:
                dependents[dep["name"]].add(pkg["name"])
    return dirs, names, dependents


def owner(path, dirs):
    best = None
    for prefix, name in dirs.items():
        if path.startswith(prefix) and (best is None or len(prefix) > len(best[0])):
            best = (prefix, name)
    return best[1] if best else None


def closure(start, dependents):
    seen = set(start)
    todo = list(start)
    while todo:
        for nxt in dependents[todo.pop()]:
            if nxt not in seen:
                seen.add(nxt)
                todo.append(nxt)
    return seen


def changed_paths():
    given = os.environ.get("CHANGED_PATHS", "").split()
    if given:
        return given
    base, head = os.environ["BASE_SHA"], os.environ["HEAD_SHA"]
    out = subprocess.check_output(["git", "diff", "--name-only", f"{base}...{head}"], text=True)
    return [line for line in out.splitlines() if line]


def shards(packages):
    cells = []
    if SERVER in packages:
        for i in (1, 2):
            cells.append({"name": f"temper-server {i}/2", "args": f"-p {SERVER} --features {SERVER_FEATURES}",
                          "partition": f"hash:{i}/2", "gepa": True, "turso": False, "doc": ""})
    rest = sorted(packages - {SERVER})
    if rest:
        count = 2 if rest and HEAVY & set(rest) else 1
        args = " ".join(f"-p {name}" for name in rest)
        for i in range(1, count + 1):
            cells.append({"name": f"other packages {i}/{count}" if count > 1 else "other packages", "args": args,
                          "partition": f"hash:{i}/{count}", "gepa": False,
                          "turso": "temper-store-turso" in rest, "doc": ""})
    if cells:
        cells[-1]["doc"] = " ".join(f"-p {name}" for name in sorted(packages))
    return cells


def main():
    lane = "pr" if os.environ.get("EVENT") == "pull_request" or os.environ.get("CHANGED_PATHS", "").strip() else "full"
    dirs, names, dependents = workspace()
    out = {"lane": lane, "mode": "full", "packages": " ".join(sorted(names)), "server": "true", "rust": "true"}
    if lane == "pr":
        direct, outside, docs_only = set(), [], True
        for path in changed_paths():
            name = owner(path, dirs)
            if name:
                direct.add(name)
                docs_only = False
            elif not is_docs(path):
                outside.append(path)
                docs_only = False
        if outside:
            print(f"Testing everything: {len(outside)} changed path(s) outside a package, first {outside[0]}", file=sys.stderr)
            packages = set(names)
        else:
            packages = closure(direct, dependents)
            out["mode"] = "affected" if packages else "none"
        out["packages"] = " ".join(sorted(packages))
        out["server"] = "true" if SERVER in packages else "false"
        out["rust"] = "false" if docs_only else "true"
        out["shards"] = json.dumps({"include": shards(packages)}) if packages else json.dumps({"include": []})
        print(f"Direct: {sorted(direct)}; testing {len(packages)} package(s): {out['packages'] or 'none'}", file=sys.stderr)
    else:
        out["shards"] = json.dumps({"include": []})
    lines = "".join(f"{k}={v}\n" for k, v in out.items())
    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a") as fh:
            fh.write(lines)
    sys.stdout.write(lines)


if __name__ == "__main__":
    main()
