# Model safety

## What

A placeholder: the user plans to scope this as its own project. It will cover how smelt keeps the model from doing harmful things it wasn't asked to do. That includes destructive actions inside its own sandbox, like deleting files or discarding work.

## Why

The sandbox keeps the model's actions away from smelt's own server, but not away from the user's work inside the sandbox. Today nothing stops or checks a destructive step before it happens.

## Open questions

Everything, for now. This was split out of the system-prompt plan, which deliberately leaves out any "ask before destructive actions" rule so this project can decide how safety should work as a whole. `coding-session.md`'s "confirmation before destructive actions" item belongs here.
