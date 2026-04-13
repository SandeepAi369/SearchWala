#!/usr/bin/env bash
set -u

PROCESS_PATTERN="qrux"
THRESHOLD_MB="${1:-35}"
INTERVAL_SEC="${2:-1}"

if ! [[ "$THRESHOLD_MB" =~ ^[0-9]+$ ]]; then
  echo "Usage: $0 [threshold_mb=35] [interval_sec=1]"
  exit 1
fi

if ! [[ "$INTERVAL_SEC" =~ ^[0-9]+$ ]]; then
  echo "Usage: $0 [threshold_mb=35] [interval_sec=1]"
  exit 1
fi

echo "Monitoring RSS for process pattern '$PROCESS_PATTERN'"
echo "Threshold: ${THRESHOLD_MB}MB | Interval: ${INTERVAL_SEC}s"
echo "Press Ctrl+C to stop."

while true; do
  pids=$(pgrep -f "$PROCESS_PATTERN" || true)

  if [[ -z "$pids" ]]; then
    echo "[$(date '+%H:%M:%S')] waiting: no qrux process found"
    sleep "$INTERVAL_SEC"
    continue
  fi

  total_kb=0
  max_kb=0

  for pid in $pids; do
    rss_kb=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
    if [[ -n "$rss_kb" && "$rss_kb" =~ ^[0-9]+$ ]]; then
      total_kb=$((total_kb + rss_kb))
      if (( rss_kb > max_kb )); then
        max_kb=$rss_kb
      fi
    fi
  done

  total_mb=$((total_kb / 1024))
  max_mb=$((max_kb / 1024))

  if (( total_mb > THRESHOLD_MB || max_mb > THRESHOLD_MB )); then
    echo "[$(date '+%H:%M:%S')] ALERT: RSS exceeded ${THRESHOLD_MB}MB (total=${total_mb}MB, max_pid=${max_mb}MB)"
  else
    echo "[$(date '+%H:%M:%S')] OK: total=${total_mb}MB, max_pid=${max_mb}MB"
  fi

  sleep "$INTERVAL_SEC"
done
