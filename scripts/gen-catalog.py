#!/usr/bin/env python3
"""Regenerate crates/core/src/catalog.json from OpenRouter's public models API.

The table is hi's tier-3 metadata fallback (AD-0044): when models.json has no
entry and the session's provider is not OpenRouter, lookup falls back to this
snapshot. Refresh per release:

    curl -s https://openrouter.ai/api/v1/models -o /tmp/or-models.json
    python3 scripts/gen-catalog.py /tmp/or-models.json

Only fields hi can act on are kept (see catalog.rs); prose like description,
benchmarks, tokenizer, and tiered pricing overrides are dropped on purpose.
"""
import json
import sys


def gen(src_path: str, out_path: str) -> None:
    raw = json.load(open(src_path))["data"]
    models = {}
    for m in raw:
        arch = m.get("architecture") or {}
        inputs = arch.get("input_modalities") or []
        params = m.get("supported_parameters") or []
        pricing = m.get("pricing") or {}
        provider = m.get("top_provider") or {}
        reasoning = m.get("reasoning")
        models[m["id"]] = {
            "name": m.get("name"),
            "context": m.get("context_length"),
            "maxOutput": provider.get("max_completion_tokens"),
            "efforts": (reasoning or {}).get("supported_efforts"),
            "defaultEffort": (reasoning or {}).get("default_effort"),
            "reasoningMandatory": bool((reasoning or {}).get("mandatory")),
            "promptRate": _rate(pricing.get("prompt")),
            "completionRate": _rate(pricing.get("completion")),
            "cacheReadRate": _rate(pricing.get("input_cache_read")),
            "cacheWriteRate": _rate(pricing.get("input_cache_write")),
            "vision": "image" in inputs or None,
            "tools": "tools" in params or None,
        }
    out = {
        "source": "https://openrouter.ai/api/v1/models",
        "models": dict(sorted(models.items())),
    }
    with open(out_path, "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
    print(f"{len(models)} models -> {out_path}")


def _rate(v):
    return float(v) if v not in (None, "") else None


if __name__ == "__main__":
    gen(sys.argv[1], sys.argv[2])
