#!/bin/bash
# v17.1 Custom run.sh for HF Spaces
# Uses Python line-by-line processing to preserve YAML formatting
# NO yaml.dump, NO sed -i

cd /usr/local/searxng

python3 << 'PYEOF'
import secrets

f = "searx/settings.yml"
lines = open(f).readlines()
output = []

# Engines to DISABLE (block datacenter IPs or cause noise)
DISABLE = {
    "google", "google images", "google news", "google videos",
    "google scholar", "google play apps", "google play movies",
    "bing", "bing images", "bing news", "bing videos",
    "mojeek", "qwant",
}

# Engines to ENABLE (datacenter-friendly)
ENABLE = {
    "duckduckgo", "duckduckgo lite", "brave",
    "wikipedia", "wikidata", "yahoo", "yahoo news",
    "startpage", "wiby", "mwmbl",
    "currency", "dictzone", "lingva", "archwiki",
}

current_engine = None
json_added = False

for line in lines:
    s = line.strip()

    # Track which engine block we are in
    if s.startswith("- name:"):
        current_engine = s.split(":", 1)[1].strip().strip("'\"").lower()

    # 1. Secret key
    if "ultrasecretkey" in line:
        line = line.replace("ultrasecretkey", secrets.token_hex(32))

    # 2. Port (first occurrence, server section)
    if s.startswith("port:") and "8080" in s:
        line = line.replace("8080", "7860")

    # 3. Bind address
    if "bind_address" in s and "127.0.0.1" in s:
        line = line.replace("127.0.0.1", "0.0.0.0")

    # 4. Limiter off
    if s.startswith("limiter:") and "true" in s:
        line = line.replace("true", "false")

    # 5. Image proxy off
    if s.startswith("image_proxy:") and "true" in s:
        line = line.replace("true", "false")

    # 6. Add JSON format after html
    if s == "- html" and not json_added:
        output.append(line)
        indent = line[:len(line) - len(line.lstrip())]
        output.append(f"{indent}- json\n")
        json_added = True
        continue

    # 7. Engine enable/disable
    if s.startswith("disabled") and ":" in s and current_engine:
        indent = line[:len(line) - len(line.lstrip())]
        if current_engine in DISABLE:
            line = f"{indent}disabled: true\n"
        elif current_engine in ENABLE:
            line = f"{indent}disabled: false\n"

    output.append(line)

open(f, "w").writelines(output)

enabled = sum(1 for l in output if "disabled: false" in l)
disabled = sum(1 for l in output if "disabled: true" in l)
print(f"Settings patched: {enabled} engines enabled, {disabled} disabled, json_format={json_added}")
PYEOF

echo "Starting Granian on port ${GRANIAN_PORT:-7860}..."
exec granian searx.webapp:app
