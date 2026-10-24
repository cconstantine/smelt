# scripts/check-server's scratch-database helpers: the dev URL's three
# sources, the scratch database's name and URL (SME-226), its own lock
# (SME-129), the sandbox-object cleanup and the drop. Split out of
# check-server, which had reached the size ceiling — the same route as the
# three files sourced beside it (SME-186 review 1, finding 4; SME-265 is
# the change that needs the room). Sourced by check-server, never run:
# these functions read check-server's own variables ($repo, $worktree,
# $lock_dir and its state-file functions) at call time, exactly as before.

# The dev DATABASE_URL: the environment's, else the check worktree's .env, else
# this checkout's, each read as dotenvy reads it (SME-186 B40 — the server
# itself gets its URL through dotenvy, so the two have to read the line the same
# way). This checkout's own .env is the last of the three (SME-226): a fresh
# check worktree has none, and start copies this file into it only *after* the
# scratch URL is built, so without this fallback the default start — which
# builds a scratch database — fails in every new worktree. It is exactly the
# file that copy makes, for the same server, and the value is never printed.
dev_database_url() {
    local url=${DATABASE_URL:-}
    if [ -z "$url" ] && [ -f "$worktree/.env" ]; then
        url=$(env_file_value DATABASE_URL)
    fi
    if [ -z "$url" ] && [ -f "$repo/.env" ]; then
        url=$(repo_env_file_value DATABASE_URL)
    fi
    echo "$url"
}

# A database URL's database name: what the scratch lock is keyed by.
db_name() {
    local url=$1
    url=${url%%\?*}
    echo "${url##*/}"
}

# The scratch database's name: the dev URL's database renamed after the
# worktree. A fixed name, so a leftover from a failed run is easy to find and
# the next run reuses it. It needs nothing *of* that URL (SME-226), which is
# what lets the scratch lock be taken before a DATABASE_URL is resolved at all.
scratch_db_name() {
    echo "smelt_scratch_$(basename "$worktree" | tr -c 'a-zA-Z0-9\n' '_')"
}

# The dev DATABASE_URL with its database renamed after the worktree; the name is
# scratch_db_name's, so the two can never disagree about what the scratch
# database is called.
scratch_db_url() {
    local url
    url=$(dev_database_url)
    [ -n "$url" ] || { echo "a scratch database needs DATABASE_URL (environment, .env or the repo's .env)" >&2; return 1; }
    # A raw tab, CR or LF is stripped by a driver's URL parser before it reads the
    # query, so one hides a `dbname=` from the guards below (SME-226 review 2,
    # finding 1; see url_has_tab_or_newline). Refused rather than stripped first:
    # no real database URL has one in it, and guessing which reading to believe is
    # how the drop ends up naming another database.
    if url_has_tab_or_newline "$url"; then
        echo "the dev DATABASE_URL contains a raw tab, CR or LF, which a driver strips before reading its query, where it can hide a dbname=: remove the character from the URL, or say CHECK_DEV_DB=1 to serve that URL as it is" >&2
        return 1
    fi
    local query=""
    case "$url" in
        *\?*) query="?${url#*\?}"; url=${url%%\?*} ;;
    esac
    # A query that names a database fights the renamed path segment over which
    # database the driver opens, and which of the two wins was never measured
    # (SME-226), so it is refused rather than guessed at: a scratch URL is only
    # worth having if it says this worktree's database and no other. Before this
    # `?dbname=other` rode along, and the recorded `sqlx database drop` and
    # `create` asked for that other one by name.
    # Which key is compared is the *decoded* one (SME-226 review 1, finding 2):
    # a driver decodes a query key before matching it against the names it knows,
    # so `?db%6Eame=other` says `dbname=other` to it exactly as the plain spelling
    # does. The literal `?dbname=` match this guard had let that one through, and
    # the two sqlx calls below asked for that other database by name.
    if query_names_dbname "$query"; then
        echo "the dev DATABASE_URL's query sets dbname=, which would override the scratch database's name: remove it from the URL, or say CHECK_DEV_DB=1 to serve that URL as it is" >&2
        return 1
    fi
    echo "${url%/*}/$(scratch_db_name)$query"
}

# Which database this start serves (SME-226): a scratch database of the
# worktree's own, unless the caller opted into the dev one with CHECK_DEV_DB=1.
# CHECK_SCRATCH_DB=1 names this default and stays accepted, so the docs, saved
# runbooks and Feature-checklist rows that set it keep working; the two flags
# together were refused at the top of start, so whatever isn't CHECK_DEV_DB=1 is
# scratch. Before this ticket the default was the dev database, and any value of
# CHECK_SCRATCH_DB but `1` — `0` among them — served it in silence.
serves_scratch_db() { [ "${CHECK_DEV_DB:-}" != 1 ]; }

# The lock ordering a start and a stop of the same scratch database
# (SME-129): keyed by the database's name, so it works before the check
# worktree exists and whatever CHECK_PORT each side uses. Descriptor 9: the
# cleanup tool a stop runs inherits it, and start waits for the tool to end,
# while the dx subshell below closes it, so the server never holds it.
take_scratch_lock() { # scratch database name
    exec 9>>"$lock_dir/smelt-check-scratch-$1.lock"
    if ! flock -n 9; then
        echo "waiting for the scratch lock: a check-server start or stop in $worktree holds it" >&2
        flock 9
    fi
}

# Deletes the sandbox objects (pods, then claims) of the instance the scratch
# database `$1` names (SME-129), before the database is dropped: its
# smelt_instance row is the last record of the instance its server labelled
# them with, and once it is dropped no server ever owns them again. Succeeds
# when there is nothing to know (no database, or one never migrated) and when
# `$1` is empty. The tool refuses anything that isn't a smelt_scratch_*
# database, and deletes nothing without the exact instance label.
cleanup_scratch() { # scratch database url (may be empty)
    local url=$1
    [ -n "$url" ] || return 0
    local cleanup
    if [ -n "${SANDBOX_INSTANCE_CLEANUP:-}" ]; then
        cleanup=("$SANDBOX_INSTANCE_CLEANUP")
    else
        cleanup=(cargo run -q --bin sandbox_instance_cleanup --features server --)
    fi
    local kubeconfig
    kubeconfig=$(cat "$(kubeconfig_file)" 2>/dev/null || true)
    local -a kube=()
    [ -n "$kubeconfig" ] && kube=(KUBECONFIG="$kubeconfig")
    (
        cd "$repo"
        env "${kube[@]}" "${cleanup[@]}" \
            --database-url "$url" --dev-database-url "$(dev_database_url)" </dev/null
    )
}

drop_scratch_db() {
    local file
    file=$(scratch_db_file)
    [ -s "$file" ] || return 0
    sqlx database drop -y --database-url "$(cat "$file")" >/dev/null
    : >"$file"
    echo "dropped the scratch database"
}

# Cleans up the scratch database's objects and drops it, then empties the
# KUBECONFIG file. Everything is kept when the cleanup can't finish, so a
# later stop retries.
finish_stop() { # scratch database url (may be empty)
    if ! cleanup_scratch "$1"; then
        echo "the scratch database's sandbox objects weren't all deleted; scripts/check-server stop again (it retries) — the database is kept so they can still be found" >&2
        return 1
    fi
    drop_scratch_db
    # Only a start writes it, so a stop with none leaves it alone. An `if`,
    # not `&&`: as the function's last command, a missing file would make it
    # fail, and stop exit 1 after a cleanup and drop that succeeded.
    if [ -e "$(kubeconfig_file)" ]; then
        : >"$(kubeconfig_file)"
    fi
}
