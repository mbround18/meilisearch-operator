#!/usr/bin/env bash
set -euo pipefail

MS_URL="http://localhost:55022"
MASTER_KEY="0Z1EyuvJiXdDYSpQrfZ2IE3tLPb2zRijvDr1SizlpKuEKSaf6Kb9sXL6bv8HMogt"

# Get total count
META=$(curl -s -H "Authorization: Bearer $MASTER_KEY" \
     "${MS_URL}/keys?offset=0&limit=1")
TOTAL=$(echo "$META" | jq -r '.total')
echo "Total keys: $TOTAL"

# Decide chunk size
CHUNK=100
OFFSET=0

while [ "$OFFSET" -lt "$TOTAL" ]; do
  echo "Fetching keys offset=$OFFSET"
  KEYS=$(curl -s -H "Authorization: Bearer $MASTER_KEY" \
         "${MS_URL}/keys?offset=$OFFSET&limit=$CHUNK" | jq -r '.results[].uid')

  echo "$KEYS" | \
    xargs -P10 -I{} curl -s -H "Authorization: Bearer $MASTER_KEY" \
      -X DELETE "${MS_URL}/keys/{}" && echo "Deleted {}" || echo "Failed {}"

  OFFSET=$((OFFSET+CHUNK))
done

echo "Done."
