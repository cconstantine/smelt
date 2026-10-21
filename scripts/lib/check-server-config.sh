# How check-server resolves one setting's value, and where headless Chrome's
# cache is. Split out of scripts/check-server, which had reached the repo's
# 40,000-byte ceiling (SME-186 review 1, finding 4). Sourced by it and never run
# on its own: these functions read check-server's `repo`, `worktree` and
# `overrides` at call time, and call scripts/lib/dotenv-value for the .env.
# NAME's value in the worktree's .env, exactly as dotenvy reads it:
# scripts/lib/dotenv-value is a port of that crate's parser, so the first line
# setting a name wins, `export` and whitespace around `=` are allowed, quotes,
# escapes and `$NAME` substitution are honoured, a ` #` comment ends an
# unquoted value, and a line that fails to parse stops the load there. The sed
# this replaces did some of it and missed the rest (SME-117 review 1: a
# `RUST_LOG=debug # note` passed on whole silenced smelt's log entirely;
# SME-117 review 2: `RUST_LOG= # note` is empty to dotenvy; SME-186 B40: it
# kept the *last* line, and a `$` in a dev .env's DATABASE_URL made this script
# and the server disagree on the URL). A missing file or name prints nothing.
# A missing python3 fails rather than reading nothing quietly.
env_file_value() {
    command -v python3 >/dev/null 2>&1 || {
        echo "reading $worktree/.env needs python3 (scripts/lib/dotenv-value)" >&2
        return 1
    }
    python3 -I "$repo/scripts/lib/dotenv-value" "$worktree/.env" "$1"
}

# The value of the last `-- NAME=` override given to start (in start's
# `overrides`), which may be empty; fails when there is none.
override_value() {
    local override value found=""
    for override in "${overrides[@]}"; do
        if [[ "$override" == "$1="* ]]; then
            value=${override#"$1="}
            found=1
        fi
    done
    [ -n "$found" ] || return 1
    echo "$value"
}

# Resolve NAME once into EFFECTIVE_SOURCE and EFFECTIVE_VALUE: the first of a
# `-- NAME=` override, the environment and the check worktree's .env that gives it
# a non-empty value ("override", "environment" or ".env"). Fails when none does: an
# empty value counts as unset, and then both are empty. The value comes out of the
# same read that decided where it came from, so asking both questions about one
# setting doesn't parse the file twice (SME-186 review 2, finding 2: effective_value
# asked effective_source, whose .env arm reads the file, and read it again for the
# answer it had already got — and start resolves several settings).
effective_resolve() {
    local value
    EFFECTIVE_SOURCE="" EFFECTIVE_VALUE=""
    if value=$(override_value "$1") && [ -n "$value" ]; then
        EFFECTIVE_SOURCE=override EFFECTIVE_VALUE=$value
    elif [ -n "${!1:-}" ]; then
        EFFECTIVE_SOURCE=environment EFFECTIVE_VALUE=${!1}
    elif value=$(env_file_value "$1") && [ -n "$value" ]; then
        EFFECTIVE_SOURCE=.env EFFECTIVE_VALUE=$value
    else
        return 1
    fi
}

# NAME's value: the first non-empty of a `-- NAME=` override, the environment and
# the check worktree's .env; empty when none is. Empty with status 0 then, not a
# failure: every caller assigns this inside `$( )`, where a failing status would
# end the script under `set -e` — which is why this doesn't pass effective_resolve's
# own status through.
effective_value() {
    effective_resolve "$1" || true
    echo "$EFFECTIVE_VALUE"
}

# Headless Chrome's cache directory: this checkout's, or else the main
# checkout's (a worktree has none of its own).
browser_check_cache() {
    # `dir` too, not just `main`: the for loop's variable is otherwise left set
    # in check-server's shell, where nothing reads it today and something would
    # tomorrow (SME-186 review 2, finding 3; G7 asks from a shell of its own).
    local main dir
    main=$(dirname "$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)")
    for dir in "$repo/.browser-check-cache" "$main/.browser-check-cache"; do
        if [ -d "$dir" ]; then
            (cd "$dir" && pwd -P)
            return 0
        fi
    done
    return 1
}
