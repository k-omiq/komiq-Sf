#!/usr/bin/env bash
# Probe each source home page once from this host and report whether Cloudflare
# (or another wall) answers instead of the site.
# Input on stdin: "name<TAB>url" lines. Output: TSV
#   name url status server cf_mitigated wall title
set -uo pipefail
UA='Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36'

probe() {
  local name=$1 url=$2 tmp status server mit wall title
  tmp=$(mktemp -d)
  status=$(curl -s -m 20 -L --max-redirs 5 -A "$UA" -H 'Accept: text/html' \
    -H 'Accept-Language: en-US,en;q=0.9' -D "$tmp/h" -o "$tmp/b" -w '%{http_code}' "$url" 2>/dev/null)
  [ -n "$status" ] || status=000
  touch "$tmp/h" "$tmp/b"
  server=$(grep -i '^server:' "$tmp/h" | tail -1 | cut -d' ' -f2- | tr -d '\r\t' | head -c 30)
  mit=$(grep -i '^cf-mitigated:' "$tmp/h" | tail -1 | cut -d' ' -f2- | tr -d '\r\t')
  title=$(head -c 200000 "$tmp/b" | tr '\n\t' '  ' | grep -oiE '<title[^>]*>[^<]{0,80}' | head -1 | sed -E 's/<title[^>]*>//I')
  wall=none
  if [ -n "$mit" ] || grep -qiE 'challenges\.cloudflare\.com|cf-chl-|_cf_chl_opt' "$tmp/b"; then
    wall=cloudflare
  elif grep -qiE 'ddos-guard' "$tmp/h" "$tmp/b"; then
    wall=ddos-guard
  elif grep -qiE 'Attention Required|you have been blocked|cf-error-details' "$tmp/b"; then
    wall=cf-block
  elif [ "$status" != 200 ] && grep -qiE 'hcaptcha\.com|google\.com/recaptcha|captcha' "$tmp/b"; then
    wall=captcha
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$url" "$status" "$server" "$mit" "$wall" "$title"
  rm -rf "$tmp"
}
export -f probe
export UA

while IFS=$'\t' read -r n u; do
  [ -n "$u" ] && printf '%s\t%s\0' "$n" "$u"
done | xargs -0 -P 8 -n 1 bash -c 'IFS=$'"'"'\t'"'"' read -r n u <<<"$1"; probe "$n" "$u"' _
