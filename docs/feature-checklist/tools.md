# Feature checklist: Tools and browsing
Part of the [Feature checklist](index.md).

The model's tools: todos, search, fetch, the browser.

| Ticket | Feature | How to check in the browser |
| -- | -- | -- |
| SME-8 | Tool use | Ask "what's on the todo list?": the transcript shows one row, "Checked the todo list", with its result a click away, and the model answers from it. Work that runs on in the background is a terminal command (the SME-13 row); there's no tasks panel since SME-54 |
| SME-19 | glob, grep | Both find a file written earlier |
| SME-20 | Todo list | The todo panel updates live as the model calls `todowrite` |
| SME-21 | webfetch, http_request | Fetch a real page; GET a JSON API. Since SME-155 no address is refused: a private address (e.g. `http://10.0.0.1/`) fails to connect with a clear message, and `localhost` is reachable |
| SME-22 | Browsing sessions | Open a session: the panel streams; click a link and use the address bar in the panel; the model reads the same page; the chat stays usable at a laptop width (1400px) |
| SME-24 | Web search | The model searches and cites a source |
| SME-69 | Chrome found outside the build checkout | Start a check server from a worktree (`scripts/check-server start`, which now passes `BROWSER_CHECK_CACHE` instead of symlinking): ask the model to `webfetch` `https://example.com`: it returns the page's text, not "chrome-headless-shell not found". Not web-checkable: a binary run with `BROWSER_CHECK_CACHE` unset and no `.browser-check-cache` in its build checkout says where it looked and to set the variable |
| SME-119 | One failed browse or fetch doesn't break the next | Not web-checkable: neither failure has a trigger in the UI (no browsing call can panic under the session lock today, and the shared browser's runtime fails only when the OS refuses threads). The SME-22 and SME-21 rows still pass. Automated: `browsing` and `webfetch` unit tests and the browser tier's `scenario_open_after_a_poisoned_session_lock` |
| SME-184 | The address bar picks http for a local address typed without a scheme | In a conversation with a sandbox, run `python3 -m http.server 5173 --bind 127.0.0.1 &` in a terminal and open a browsing session. In the Live Browser's address bar type `localhost:5173` and Enter: the bar reads `http://localhost:5173/`, no error shows, and the panel shows "Directory listing for /". Type `example.com`: the bar reads `https://example.com/`. Delete the conversation afterwards. Automated: `browsing::server::tests::test_normalize_address_gives_local_and_private_addresses_http` and `…_gives_public_names_https` |
