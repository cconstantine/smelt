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

# Whether a URL holds a raw tab, CR or LF anywhere in it. The WHATWG URL parser
# a driver reads with (the `url` crate, behind sqlx's `url.query_pairs()`)
# removes all three from its input before parsing, so it sees a URL the shell
# here does not: `?db<TAB>name=` is the key `dbname` to it, and a newline ends
# the query before the pair after it, since `IFS='&' read` stops there. That is
# how a `dbname=` hides from the guard below (SME-226 review 2, finding 1), and
# a `.env` delivers the characters — a literal tab inside double quotes, or a
# multi-line single-quoted value. Stripping them first would be the other way
# round, but nothing legitimate has one in a database URL, so the URL is refused
# instead of rewritten: whichever reading the guard keeps, some reader disagrees.
url_has_tab_or_newline() { # url
    case "$1" in
        *[$'\t\r\n']*) return 0 ;;
    esac
    return 1
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
