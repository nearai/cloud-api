#!/usr/bin/env python3
"""Audit or apply the versioned direct-OpenAI text pricing catalog.

The admin token is read from an environment variable and is never printed.
By default this command is read-only. New GPT-5.6 rows remain inactive unless
``--activate-gpt56`` is explicitly passed after capability probes succeed.
"""

from __future__ import annotations

import argparse
from decimal import Decimal, ROUND_HALF_UP
import json
import os
from pathlib import Path
import sys
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "config" / "openai_text_pricing.v1.json"
GPT56_PREFIX = "openai/gpt-5.6"
DEFAULT_USER_AGENT = "cloud-api-openai-pricing-catalog/1"


class CatalogError(RuntimeError):
    pass


def load_manifest(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as handle:
        manifest = json.load(handle)
    if manifest.get("manifestVersion") != 1:
        raise CatalogError("manifestVersion must be 1")
    models = manifest.get("models")
    if not isinstance(models, list) or len(models) != 18:
        raise CatalogError("manifest must contain exactly 18 canonical OpenAI models")
    ids = [model.get("modelId") for model in models]
    if any(not isinstance(model_id, str) for model_id in ids) or len(set(ids)) != len(ids):
        raise CatalogError("manifest modelId values must be unique strings")
    for entry in models:
        profile = entry.get("textPricing")
        if not isinstance(profile, dict):
            raise CatalogError(f"{entry['modelId']}: textPricing must be an object")
        validate_profile(entry["modelId"], profile)
        catalog = entry.get("catalog")
        if catalog is not None:
            expected_model = entry.get("upstreamModel")
            configured_model = catalog.get("providerConfig", {}).get("model_name")
            if configured_model != expected_model:
                raise CatalogError(
                    f"{entry['modelId']}: providerConfig.model_name must pin {expected_model}"
                )
    return manifest


def validate_profile(model_id: str, profile: dict[str, Any]) -> None:
    if profile.get("version") != 1:
        raise CatalogError(f"{model_id}: profile version must be 1")
    if profile.get("currency") != "USD" or profile.get("unit") != "million_tokens":
        raise CatalogError(f"{model_id}: profile must use USD per million_tokens")
    tiers = profile.get("tiers")
    if not isinstance(tiers, dict) or "default" not in tiers:
        raise CatalogError(f"{model_id}: default tier is required")
    if set(tiers) - {"default", "flex", "priority"}:
        raise CatalogError(f"{model_id}: profile contains an unknown tier")
    has_long = False
    for tier_name, tier in tiers.items():
        if not isinstance(tier, dict) or "short" not in tier:
            raise CatalogError(f"{model_id}: {tier_name}.short is required")
        for band_name in ("short", "long"):
            rates = tier.get(band_name)
            if rates is None:
                continue
            has_long = has_long or band_name == "long"
            required = {"uncachedInput", "cachedInput", "output"}
            allowed = required | {"cacheWrite"}
            if not required.issubset(rates) or set(rates) - allowed:
                raise CatalogError(f"{model_id}: {tier_name}.{band_name} rates are incomplete")
            if model_id.startswith(GPT56_PREFIX) and "cacheWrite" not in rates:
                raise CatalogError(
                    f"{model_id}: {tier_name}.{band_name}.cacheWrite is published and required"
                )
            for category, value in rates.items():
                if not isinstance(value, str):
                    raise CatalogError(
                        f"{model_id}: {tier_name}.{band_name}.{category} must be a decimal string"
                    )
                try:
                    parsed = Decimal(value)
                except Exception as error:  # Decimal raises several subclasses.
                    raise CatalogError(f"{model_id}: invalid rate {value!r}") from error
                if not parsed.is_finite() or parsed < 0 or parsed.as_tuple().exponent < -9:
                    raise CatalogError(f"{model_id}: invalid rate {value!r}")
    threshold = profile.get("longContextThreshold")
    if has_long and threshold != 272000:
        raise CatalogError(f"{model_id}: long-context profiles must use threshold 272000")
    if threshold is not None and "long" not in tiers["default"]:
        raise CatalogError(f"{model_id}: default.long is required when a threshold is set")


def request_json(
    method: str,
    url: str,
    token: str,
    user_agent: str,
    body: dict[str, Any] | None = None,
) -> dict[str, Any]:
    data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
    request = Request(
        url,
        method=method,
        data=data,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/json",
            "Content-Type": "application/json",
            "User-Agent": user_agent,
        },
    )
    try:
        with urlopen(request, timeout=60) as response:
            return json.load(response)
    except HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:1000]
        raise CatalogError(f"admin API returned HTTP {error.code}: {detail}") from error
    except (URLError, TimeoutError) as error:
        raise CatalogError(f"admin API request failed: {error}") from error


def fetch_catalog(base_url: str, token: str, user_agent: str) -> dict[str, dict[str, Any]]:
    query = urlencode({"include_inactive": "true", "limit": 500, "offset": 0})
    response = request_json(
        "GET",
        f"{base_url.rstrip('/')}/v1/admin/models?{query}",
        token,
        user_agent,
    )
    models = response.get("models")
    if not isinstance(models, list):
        raise CatalogError("admin model response did not contain a models array")
    return {model["modelId"]: model for model in models}


def projected_nano_per_token(rate: str) -> int:
    # USD/M-token * 1e9 nano-USD/USD / 1e6 tokens = * 1e3.
    return int((Decimal(rate) * Decimal(1000)).quantize(Decimal("1"), rounding=ROUND_HALF_UP))


def expected_projection(profile: dict[str, Any]) -> dict[str, int]:
    rates = profile["tiers"]["default"]["short"]
    return {
        "inputCostPerToken": projected_nano_per_token(rates["uncachedInput"]),
        "cacheReadCostPerToken": projected_nano_per_token(rates["cachedInput"]),
        "outputCostPerToken": projected_nano_per_token(rates["output"]),
    }


def compare_catalog_metadata(
    model_id: str,
    expected: dict[str, Any],
    actual: dict[str, Any],
) -> list[str]:
    """Compare PATCH-shaped catalog fields with the nested admin response."""
    differences: list[str] = []
    expected_metadata = {
        key: value
        for key, value in expected.items()
        if key not in {"inputModalities", "outputModalities"}
    }
    expected_metadata["architecture"] = {
        "inputModalities": expected["inputModalities"],
        "outputModalities": expected["outputModalities"],
    }

    for field, value in expected_metadata.items():
        actual_value = actual.get(field)
        if field == "providerConfig":
            # The admin response may contain a redacted global/provider key. It
            # is intentionally outside the checked-in manifest; all routing
            # dimensions must still match exactly.
            actual_value = {
                key: item
                for key, item in (actual_value or {}).items()
                if key != "api_key"
            }
        if actual_value != value:
            differences.append(f"{model_id}: metadata.{field} differs")
    return differences


def compare_entry(
    expected: dict[str, Any],
    actual: dict[str, Any] | None,
    activate_gpt56: bool,
) -> list[str]:
    model_id = expected["modelId"]
    if actual is None:
        return [f"{model_id}: missing"]
    differences: list[str] = []
    if actual.get("textPricing") != expected["textPricing"]:
        differences.append(f"{model_id}: textPricing differs")
    projection = expected_projection(expected["textPricing"])
    for field, amount in projection.items():
        actual_price = actual.get(field)
        actual_amount = actual_price.get("amount") if isinstance(actual_price, dict) else None
        if actual_amount != amount:
            differences.append(f"{model_id}: {field}.amount is {actual_amount}, expected {amount}")

    catalog = expected.get("catalog")
    if catalog:
        metadata = actual.get("metadata") or {}
        differences.extend(compare_catalog_metadata(model_id, catalog, metadata))
        expected_active = activate_gpt56
    else:
        expected_active = bool(expected["expectedActive"])
    if actual.get("isActive") is not expected_active:
        differences.append(
            f"{model_id}: isActive is {actual.get('isActive')}, expected {expected_active}"
        )
    return differences


def audit(
    manifest: dict[str, Any],
    catalog: dict[str, dict[str, Any]],
    activate_gpt56: bool,
) -> list[str]:
    differences: list[str] = []
    for expected in manifest["models"]:
        differences.extend(compare_entry(expected, catalog.get(expected["modelId"]), activate_gpt56))
    return differences


def apply_payload(
    manifest: dict[str, Any],
    catalog: dict[str, dict[str, Any]],
    activate_gpt56: bool,
) -> dict[str, dict[str, Any]]:
    payload: dict[str, dict[str, Any]] = {}
    for entry in manifest["models"]:
        model_id = entry["modelId"]
        catalog_fields = entry.get("catalog")
        if model_id not in catalog and catalog_fields is None:
            raise CatalogError(
                f"refusing to create missing existing row {model_id}; restore it before applying"
            )
        update: dict[str, Any] = {
            "textPricing": entry["textPricing"],
            "changeReason": (
                f"Apply OpenAI text pricing manifest v{manifest['manifestVersion']} "
                f"verified {manifest['verifiedAt']}"
            ),
        }
        if catalog_fields:
            update.update(catalog_fields)
            update["isActive"] = activate_gpt56
        payload[model_id] = update
    return payload


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--base-url", help="Cloud API base URL, for example staging")
    parser.add_argument(
        "--admin-token-env",
        default="NEAR_AI_CLOUD_ADMIN_ACCESS_TOKEN",
        help="environment variable holding the admin token",
    )
    parser.add_argument(
        "--user-agent-env",
        help=(
            "optional environment variable holding the User-Agent bound to the admin "
            "token; defaults to the catalog command's own User-Agent"
        ),
    )
    parser.add_argument("--apply", action="store_true", help="apply differences via PATCH")
    parser.add_argument(
        "--activate-gpt56",
        action="store_true",
        help="activate GPT-5.6 rows; use only after upstream capability probes",
    )
    parser.add_argument(
        "--validate-only",
        action="store_true",
        help="validate the checked-in manifest without calling Cloud API",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        manifest = load_manifest(args.manifest)
        if args.validate_only:
            print(f"valid: {len(manifest['models'])} canonical OpenAI models")
            return 0
        if not args.base_url:
            raise CatalogError("--base-url is required unless --validate-only is used")
        token = os.environ.get(args.admin_token_env)
        if not token:
            raise CatalogError(f"admin token environment variable {args.admin_token_env} is unset")
        user_agent = DEFAULT_USER_AGENT
        if args.user_agent_env:
            user_agent = os.environ.get(args.user_agent_env, "")
            if not user_agent:
                raise CatalogError(
                    f"user-agent environment variable {args.user_agent_env} is unset"
                )
            if "\r" in user_agent or "\n" in user_agent:
                raise CatalogError("admin User-Agent must not contain a newline")
        catalog = fetch_catalog(args.base_url, token, user_agent)
        differences = audit(manifest, catalog, args.activate_gpt56)
        if not differences:
            print("audit clean: 18 canonical OpenAI rows match the manifest")
            return 0
        print("catalog differences:")
        for difference in differences:
            print(f"- {difference}")
        if not args.apply:
            return 1

        payload = apply_payload(manifest, catalog, args.activate_gpt56)
        request_json(
            "PATCH",
            f"{args.base_url.rstrip('/')}/v1/admin/models",
            token,
            user_agent,
            payload,
        )
        remaining = audit(
            manifest,
            fetch_catalog(args.base_url, token, user_agent),
            args.activate_gpt56,
        )
        if remaining:
            print("post-apply audit still differs:")
            for difference in remaining:
                print(f"- {difference}")
            return 1
        print("apply complete: post-apply audit is clean")
        return 0
    except (CatalogError, OSError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
