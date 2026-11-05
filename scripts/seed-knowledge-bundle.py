#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
"""Deterministic knowledge-bundle builder + ed25519 signer for engine seeding.

Replicates the server-side bundle hash/signature chain byte-for-byte:

- content hash  = "blake3:" + blake3(serde_json::to_vec(canonical_bundle))
  where canonical_bundle = bundle with audit.content_hash cleared and
  publish_signature removed. Struct fields serialize in declaration order,
  free-form JSON values (payloads) use serde_json BTreeMap ordering, i.e.
  recursively sorted keys, compact separators, UTF-8, no ASCII escaping.
- signature     = ed25519 over the content-hash string bytes (ASCII).
  key_id = 64-hex ed25519 public key; sig = 128-hex.

Requirements: blake3, cryptography (both available in the deploy environment).

Usage:
  python seed-knowledge-bundle.py \
    --dataset-id obs --schema-id https://evorule.org/schemas/obs/run/v1.json \
    --sandbox-ref sandbox:1 --key-file publisher-key.json --out bundle.json
"""

import argparse
import json
import os
import sys

import blake3
from cryptography.hazmat.primitives.asymmetric.ed25519 import (
    Ed25519PrivateKey,
    Ed25519PublicKey,
)

BUNDLE_SCHEMA_VERSION = "1.0"


def sorted_value(v):
    """Recursively sort dict keys to mirror serde_json Value (BTreeMap) ordering."""
    if isinstance(v, dict):
        return {k: sorted_value(v[k]) for k in sorted(v)}
    if isinstance(v, list):
        return [sorted_value(x) for x in v]
    return v


ENTRY_FIELD_ORDER = [
    "entry_id", "entry_kind", "rule_body", "schema_ref", "provenance",
    "domain", "tags", "dependencies", "knowledge_kind", "trust_level",
    "license_ref", "execution_contract",
]


def order_entry(e):
    """Struct fields in serde declaration order; only Value payloads (rule_body)
    use serde_json BTreeMap ordering (recursively sorted keys)."""
    out = {}
    for k in ENTRY_FIELD_ORDER:
        if k in e:
            out[k] = sorted_value(e[k]) if k == "rule_body" else e[k]
    for k in e:
        if k not in out:
            out[k] = e[k]
    return out


def canonical_bytes(obj):
    """serde_json::to_vec equivalent: compact, UTF-8, no key sorting at struct level."""
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def content_hash(bundle):
    canonical = dict(bundle)
    audit = dict(canonical["audit"])
    audit["content_hash"] = ""
    canonical["audit"] = audit
    canonical.pop("publish_signature", None)
    digest = blake3.blake3(canonical_bytes(canonical)).hexdigest()
    return "blake3:" + digest


def load_or_create_key(path):
    if os.path.exists(path):
        with open(path, "r", encoding="utf-8") as f:
            doc = json.load(f)
        key = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(doc["private_key_hex"]))
    else:
        key = Ed25519PrivateKey.generate()
        doc = {
            "private_key_hex": key.private_bytes_raw().hex(),
            "public_key_hex": key.public_key().public_bytes_raw().hex(),
        }
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(doc, f, indent=2)
    pub_hex = key.public_key().public_bytes_raw().hex()
    return key, pub_hex


def build_bundle(args):
    if args.entries_file:
        with open(args.entries_file, "r", encoding="utf-8") as f:
            entries = json.load(f)
        if isinstance(entries, dict):
            entries = [entries]
    else:
        entries = [
            {
                "entry_id": "entry-obs-seed-001",
                "entry_kind": "knowledge",
                "rule_body": {
                    "dataset": args.dataset_id,
                    "description": "Engine runtime observation seed entry",
                    "record_type": "engine_run_info",
                    "source": "seed-knowledge-bundle",
                },
                "schema_ref": args.schema_id,
                "provenance": {"source": "seed:engine-runtime"},
                "domain": "general",
                "knowledge_kind": "fact",
            }
        ]
    entries = [order_entry(e) for e in entries]

    skeleton = {
        "bundle_schema_version": BUNDLE_SCHEMA_VERSION,
        "bundle_id": args.bundle_id or "bundle-{ds}-seed-v1".format(ds=args.dataset_id),
        "dataset": {
            "dataset_id": args.dataset_id,
            "name": args.dataset_name or "seed:{ds}".format(ds=args.dataset_id),
            "tenant_id": args.tenant_id,
            "instance_id": args.tenant_id,
            "versioning": {"current": "v1", "chain": ["v1"]},
            "version_selection": {"mode": "pinned", "pinned_version": "v1"},
        },
        "entries": entries,
        "tests": {"subset": [args.sandbox_ref], "verdict": "pass"},
        "audit": {
            "exported_at": args.exported_at,
            "exported_by": args.exported_by,
            "source_version": "v1",
            "content_hash": "",
            "hash_algo": "blake3",
        },
    }
    skeleton["audit"]["content_hash"] = content_hash(skeleton)
    return skeleton


def sign_bundle(bundle, key, key_id):
    payload = bundle["audit"]["content_hash"].encode("utf-8")
    sig = key.sign(payload)
    bundle["publish_signature"] = {"key_id": key_id, "sig": sig.hex()}
    return bundle


def verify_self(bundle):
    """Round-trip sanity: recompute hash and verify signature before emit."""
    recorded = bundle["audit"]["content_hash"]
    if content_hash(bundle) != recorded:
        raise SystemExit("self-check failed: content hash mismatch")
    block = bundle.get("publish_signature")
    if not block:
        raise SystemExit("self-check failed: signature missing")
    pub = Ed25519PublicKey.from_public_bytes(bytes.fromhex(block["key_id"]))
    pub.verify(bytes.fromhex(block["sig"]), recorded.encode("utf-8"))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--dataset-id", required=True)
    p.add_argument("--dataset-name", default=None)
    p.add_argument("--schema-id", required=True)
    p.add_argument("--entries-file", default=None,
                   help="JSON array of knowledge entries (rule_body = payload)")
    p.add_argument("--bundle-id", default=None)
    p.add_argument("--sandbox-ref", required=True, help="e.g. sandbox:1")
    p.add_argument("--tenant-id", default="seed-deploy")
    p.add_argument("--exported-at", required=True, help="ISO-8601 UTC timestamp")
    p.add_argument("--exported-by", default="seed-deploy")
    p.add_argument("--key-file", required=True)
    p.add_argument("--out", required=True)
    args = p.parse_args()

    key, pub_hex = load_or_create_key(args.key_file)
    bundle = build_bundle(args)
    sign_bundle(bundle, key, pub_hex)
    verify_self(bundle)

    out_dir = os.path.dirname(os.path.abspath(args.out))
    os.makedirs(out_dir, exist_ok=True)
    with open(args.out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(bundle, f, ensure_ascii=False, indent=2)
        f.write("\n")
    print(json.dumps({
        "bundle_id": bundle["bundle_id"],
        "dataset_id": bundle["dataset"]["dataset_id"],
        "entry_count": len(bundle["entries"]),
        "content_hash": bundle["audit"]["content_hash"],
        "key_id": pub_hex,
    }))
    return 0


if __name__ == "__main__":
    sys.exit(main())
