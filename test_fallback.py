#!/usr/bin/env python3
import json
import sys
import time

import requests

URL = "http://localhost:8000/search"

payload = {
    "query": "latest rust async runtime benchmarks",
    "max_results": 6,
    "focus_mode": "lite",
    "llm": {
        "provider": "openai",
        "api_key": "INVALID_KEY_FOR_FALLBACK_TEST",
        "model": "gpt-4o-mini",
        "timeout_ms": 5000
    }
}


def main() -> int:
    started = time.time()
    try:
        response = requests.post(URL, json=payload, timeout=25)
    except Exception as exc:
        print(f"[FAIL] Request failed: {exc}")
        return 1

    elapsed_ms = int((time.time() - started) * 1000)
    print(f"HTTP {response.status_code} in {elapsed_ms}ms")

    try:
        data = response.json()
    except json.JSONDecodeError:
        print("[FAIL] Response is not valid JSON")
        print(response.text[:500])
        return 1

    ok_status = response.status_code == 200
    has_results = isinstance(data.get("search_results"), list)
    has_llm_error = bool(data.get("llm_error"))

    if ok_status and has_results and has_llm_error:
        print("[PASS] Fallback works: HTTP 200 + search_results + llm_error")
        print(f"search_results={len(data.get('search_results', []))}")
        print(f"llm_error={data.get('llm_error')}")
        return 0

    print("[FAIL] Unexpected fallback behavior")
    print(f"ok_status={ok_status}, has_results={has_results}, has_llm_error={has_llm_error}")
    print(json.dumps(data, indent=2)[:1200])
    return 1


if __name__ == "__main__":
    sys.exit(main())
