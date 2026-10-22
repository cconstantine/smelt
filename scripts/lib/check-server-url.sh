#!/usr/bin/env bash
# URL helpers for scripts/check-server, in a file of their own because that one
# is at the size ceiling (SME-186 review 1, finding 4 — same note as the two
# libs it sources above). Sourced, never run.

# A percent-decoded URL query *key*. A driver decodes a query key before
# matching it against the names it knows, so a guard that compares the raw text
# is walked past by `db%6Eame`, which is `dbname` to it (SME-226 review 1,
# finding 2). `%XX` is all a key that means something else needs: a lone `%`, or
# one that isn't followed by two hex digits, is left as it is, as a driver
# leaves it.
urldecode_key() { # key
    local raw=$1
    case "$raw" in
        *%[0-9a-fA-F][0-9a-fA-F]*) printf '%b' "${raw//%/\\x}" ;;
        *) printf '%s' "$raw" ;;
    esac
}

# Whether a URL's query — the `?…` part, `?` included, possibly empty — sets
# the database name: any pair whose decoded key is `dbname`, in any position.
query_names_dbname() { # query
    local pair
    local -a pairs=()
    IFS='&' read -r -a pairs <<<"${1#\?}"
    for pair in "${pairs[@]}"; do
        [ "$(urldecode_key "${pair%%=*}")" = dbname ] && return 0
    done
    return 1
}
