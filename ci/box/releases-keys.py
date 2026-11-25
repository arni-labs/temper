#!/usr/bin/env python3
"""The R2 bucket temper's linux binaries go to, and its two keys, through the Cloudflare API with the control plane's
minter token (cloudflare_r2_minter: Workers R2 Storage Edit, Account API Tokens Edit):

  releases-keys.py bucket <minter file> <account>              makes temper-releases; one already there is kept
  releases-keys.py write <minter file> <account> <out file>    a key that may write temper-releases alone (for ci-temper)
  releases-keys.py read <minter file> <account> <out file>     a key that may only read it (for whatever installs a binary)

A key goes to <out file> (mode 600) as two lines, the S3 access key then its secret, and making one first takes off
the earlier key of that name, so a lost copy stops working. A bucket of its own, not a prefix of temper-v2-releases:
R2 can't scope a lasting key to a prefix, and the write key sits on a box that runs every pushed commit's code.
S3 access key = the token's id, secret = sha256 of its value (Cloudflare R2 API tokens). Prints no secret.
"""
import hashlib, json, os, sys, urllib.error, urllib.request

API, BUCKET = "https://api.cloudflare.com/client/v4", "temper-releases"
GROUPS = {"write": "Workers R2 Storage Bucket Item Write", "read": "Workers R2 Storage Bucket Item Read"}


def call(token, method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(API + path, data=data, method=method,
                                 headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.load(e)
        except ValueError:
            return e.code, {"errors": [{"message": "no JSON"}]}


def need(code, a, what):
    if code != 200:
        sys.exit(f"releases-keys: {what} answered {code} " + "; ".join(f"{e.get('code')}: {e.get('message')}" for e in a.get("errors") or []))
    return a["result"]


def main(mode, minter_file, account, *out):
    token = open(minter_file).read().strip()
    if mode == "bucket":
        code, a = call(token, "POST", f"/accounts/{account}/r2/buckets", {"name": BUCKET})
        if code != 200 and not any(e.get("code") == 10004 for e in a.get("errors") or []):
            need(code, a, f"making {BUCKET}")
        print(f"releases-keys: {BUCKET} {'made' if code == 200 else 'already there'}")
        return
    if mode not in GROUPS or len(out) != 1:
        sys.exit(__doc__)
    name, page = f"{BUCKET}-{mode}", 1
    while True:
        code, a = call(token, "GET", f"/accounts/{account}/tokens?per_page=50&page={page}")
        for t in need(code, a, "listing tokens"):
            if t.get("name") == name:
                code, b = call(token, "DELETE", f"/accounts/{account}/tokens/{t['id']}")
                code == 404 or need(code, b, "taking off the earlier key")
                print(f"releases-keys: took off the earlier {name}")
        if page >= (a.get("result_info") or {}).get("total_pages", 1):
            break
        page += 1
    code, a = call(token, "GET", f"/accounts/{account}/tokens/permission_groups")
    group = [g["id"] for g in need(code, a, "reading permission groups") if g.get("name") == GROUPS[mode]]
    if not group:
        sys.exit(f"releases-keys: no permission group {GROUPS[mode]}")
    resource = f"com.cloudflare.edge.r2.bucket.{account}_default_{BUCKET}"
    code, a = call(token, "POST", f"/accounts/{account}/tokens", {"name": name, "policies": [
        {"effect": "allow", "resources": {resource: "*"}, "permission_groups": [{"id": group[0]}]}]})
    made = need(code, a, f"making {name}")
    os.umask(0o077)
    with open(out[0], "w") as f:
        f.write(made["id"] + "\n" + hashlib.sha256(made["value"].encode()).hexdigest() + "\n")
    print(f"releases-keys: {name}, for {BUCKET} alone")


main(*sys.argv[1:])
