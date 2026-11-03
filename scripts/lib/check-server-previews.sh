# A check server's previews, and the address its pages link back to
# (SME-96). Sourced by scripts/check-server, never run; it reads start's
# `port` and `overrides` and the config helpers' override_value and
# env_file_value.
#
# Left to itself a check server took smelt's defaults: previews on
# 0.0.0.0:8181, which the dev server already holds (so smelt turned
# previews off), links built on :8181 (so they opened the dev server's
# listener), and SMELT_BASE_URL from compose's environment or the copied
# .env, both the dev server's (so a stopped sandbox's preview page and an
# MCP OAuth Connect sent the user to the dev server). Now each has a check
# server default of its own, derived from CHECK_PORT:
#
#   SMELT_PREVIEW_ADDR  0.0.0.0:<CHECK_PORT+200>   (8281 for 8081)
#   SMELT_PREVIEW_URL   http://{port}-{conversation}.preview.localhost:<that port>
#   SMELT_BASE_URL      http://localhost:<CHECK_PORT>
#
# +200, not the dev server's +101: 8182 is Grafana's host port (SME-190).
# Only a `-- NAME=VALUE` override changes them. The environment and .env
# hold the dev server's values in this setup, so they are ignored, with a
# notice naming each one (never its value).

PREVIEW_OFFSET=200

# Chooses the three settings into PREVIEW_ADDR, PREVIEW_URL and BASE_URL,
# each with a *_NOTE saying whose it is, and PREVIEW_PORT, the port the
# listener binds. A default is appended to `overrides`, so the server gets
# exactly what's printed; the caller's own override is already there. It
# exits 1 on a setting it refuses, before anything is checked out,
# created or started.
choose_preview_settings() {
    local value
    if value=$(override_value SMELT_PREVIEW_ADDR); then
        if [ -z "$value" ]; then
            echo "an empty -- SMELT_PREVIEW_ADDR= is refused: smelt reads it as unset, which is the dev server's 0.0.0.0:8181. Leave it out for check-server's default, 0.0.0.0:$((port + PREVIEW_OFFSET))" >&2
            exit 1
        fi
        PREVIEW_ADDR=$value PREVIEW_ADDR_NOTE=yours
    else
        if [ $((port + PREVIEW_OFFSET)) -gt 65535 ]; then
            echo "CHECK_PORT=$port leaves no port for its previews (CHECK_PORT+$PREVIEW_OFFSET is over 65535): use a lower CHECK_PORT, or pass -- SMELT_PREVIEW_ADDR=0.0.0.0:<a free port>" >&2
            exit 1
        fi
        PREVIEW_ADDR=0.0.0.0:$((port + PREVIEW_OFFSET)) PREVIEW_ADDR_NOTE="check-server's default"
        overrides+=("SMELT_PREVIEW_ADDR=$PREVIEW_ADDR")
    fi
    # A host (a name, an IPv4 address or a bracketed IPv6 one), a colon and
    # a port: `8281` or `::1` alone would pass a port check on the tail and
    # then fail smelt's bind, turning previews off (review 1, L2).
    if ! [[ "$PREVIEW_ADDR" =~ ^(\[[^]]*\]|[^:]+):([1-9][0-9]{0,4})$ ]] || [ "${BASH_REMATCH[2]}" -gt 65535 ]; then
        echo "SMELT_PREVIEW_ADDR=$PREVIEW_ADDR isn't a host and a port from 1 to 65535 (for example 0.0.0.0:8281 or [::]:8281)" >&2
        exit 1
    fi
    PREVIEW_PORT=${BASH_REMATCH[2]}
    if [ "$PREVIEW_PORT" = "$port" ]; then
        echo "SMELT_PREVIEW_ADDR=$PREVIEW_ADDR is the app's own port ($port): give previews another one" >&2
        exit 1
    fi

    if value=$(override_value SMELT_PREVIEW_URL); then
        if [ -z "$value" ]; then
            echo "an empty -- SMELT_PREVIEW_URL= is refused: smelt reads it as unset, which links to the dev server's previews on :8181. Leave it out for check-server's default" >&2
            exit 1
        fi
        PREVIEW_URL=$value PREVIEW_URL_NOTE=yours
        # smelt matches a preview request's whole Host, port included,
        # against the template, and with nothing forwarding in between the
        # browser connects to the listener's port: one that differs gives
        # links that don't open. Warned about, not refused, since a proxy
        # in front could make it right.
        local url_port
        if [[ "$value" =~ ^[A-Za-z]+://[^/]*:([0-9]+)(/|$) ]]; then
            url_port=${BASH_REMATCH[1]}
        elif [[ "$value" == https://* ]]; then
            url_port=443
        else
            url_port=80
        fi
        if [ "$url_port" != "$PREVIEW_PORT" ]; then
            echo "warning: preview links name port $url_port but previews listen on $PREVIEW_PORT: links won't open unless something forwards $url_port to $PREVIEW_PORT" >&2
        fi
    else
        PREVIEW_URL="http://{port}-{conversation}.preview.localhost:$PREVIEW_PORT"
        if [ "$PREVIEW_ADDR_NOTE" = yours ]; then
            PREVIEW_URL_NOTE="following your address"
        else
            PREVIEW_URL_NOTE="check-server's default"
        fi
        overrides+=("SMELT_PREVIEW_URL=$PREVIEW_URL")
    fi

    # An empty one is passed through as asked: smelt then derives the base
    # URL from each request's Host, which on a check server is its own.
    if value=$(override_value SMELT_BASE_URL); then
        BASE_URL=$value BASE_URL_NOTE=yours
    else
        BASE_URL=http://localhost:$port BASE_URL_NOTE="check-server's default"
        overrides+=("SMELT_BASE_URL=$BASE_URL")
    fi
}

# One line per setting the environment or .env would have set and this
# start ignores, naming where it came from but never the value. Run after
# choose_preview_settings, whose defaults are in `overrides` by then: so
# whose each value is comes from its note, not from override_value.
preview_ignored_notices() {
    local name value note
    for name in SMELT_PREVIEW_ADDR SMELT_PREVIEW_URL SMELT_BASE_URL; do
        case $name in
            SMELT_PREVIEW_ADDR) note=$PREVIEW_ADDR_NOTE ;;
            SMELT_PREVIEW_URL) note=$PREVIEW_URL_NOTE ;;
            *) note=$BASE_URL_NOTE ;;
        esac
        [ "$note" = yours ] && continue
        if [ -n "${!name:-}" ]; then
            echo "ignoring $name from the environment: it's the dev server's; pass -- $name=… to choose one"
        elif value=$(env_file_value "$name" 2>/dev/null) && [ -n "$value" ]; then
            echo "ignoring $name from .env: it's the dev server's; pass -- $name=… to choose one"
        fi
    done
}

# What the server was given, with whose each one is.
preview_summary() {
    local base="links back to $BASE_URL ($BASE_URL_NOTE)"
    [ -z "$BASE_URL" ] && base="SMELT_BASE_URL unset ($BASE_URL_NOTE): derived from each request"
    echo "previews on $PREVIEW_ADDR ($PREVIEW_ADDR_NOTE) as $PREVIEW_URL ($PREVIEW_URL_NOTE), $base"
}
