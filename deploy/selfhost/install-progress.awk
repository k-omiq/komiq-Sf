# Retain per-stage events and per-table high-water counts across log rotation.
BEGIN {
    n = split(marks, saved, ";")
    for (i = 1; i <= n; i++) {
        split(saved[i], pair, " ")
        if (pair[1] != "" && pair[2] != "") high[pair[1]] = pair[2]
    }
}
function advance(key, value) {
    # Counts are integer decimal strings. Compare without floating-point loss.
    sub(/^0+/, "", value)
    if (value == "") value = "0"
    if (!(key in high) || length(value) > length(high[key]) ||
        (length(value) == length(high[key]) && ("x" value) > ("x" high[key]))) {
        high[key] = value
        advanced = 1
    }
}
{
    sub(/^sqlite import: /, "")
    if ($1 ~ /^(copy|migrate|repair|import|verify|rename)$/ &&
        ($2 == "start" || $2 == "complete:")) advance("stage:" $1 ":" $2, 1)
    if ($1 == "precheck" && $3 == "start") advance("precheck:" $2 ":start", 1)
    if ($1 == "precheck" && $3 == "complete:" && $4 ~ /^[0-9]+$/) {
        advance("precheck:" $2 ":complete", 1)
        advance("precheck:" $2 ":rows", $4)
    }
    if ($1 == "table") {
        table = $2
        sub(/:$/, "", table)
        if ($3 ~ /^[0-9]+$/ && $4 == "rows") advance("table:" table ":rows", $3)
        if ($3 == "complete:" && $4 ~ /^[0-9]+$/) {
            advance("table:" table ":complete", 1)
            advance("table:" table ":rows", $4)
            if ($7 ~ /^[0-9]+$/ && $8 == "COPY") advance("table:" table ":bytes", $7)
        }
    }
    if ($1 == "verify" && $3 ~ /^SQLite=[0-9]+$/ && $4 ~ /^PG=[0-9]+$/) {
        advance("verify:" $2, 1)
        split($3, count, "="); advance("verify:" $2 ":sqlite", count[2])
        split($4, count, "="); advance("verify:" $2 ":pg", count[2])
    }
    if ($1 == "repair:" && $3 ~ /^[0-9]+$/ && $4 == "rows")
        advance("repair:" $2 ":" $5, $3)
    if ($1 == "verifier" && $2 == "PASS:") advance("verifier:pass", 1)
    if ($1 == "renamed" && $2 == "original") advance("rename:archived", 1)
    if ($1 == "complete:") advance("overall:complete", 1)
}
END {
    print advanced + 0
    printf "state;"
    for (key in high) printf "%s %s;", key, high[key]
}
