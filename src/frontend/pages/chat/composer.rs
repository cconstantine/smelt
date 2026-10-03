//! The message box and a new conversation's example asks.

/// Asks offered in an empty conversation (SME-41 D12): one each for code,
/// a repository and the web, the three things smelt is for.
pub(super) const EXAMPLE_ASKS: [&str; 3] = [
    "Write a Python script that prints the first 20 primes, then run it",
    "Clone https://github.com/pallets/itsdangerous and run its tests",
    "Find the latest stable Rust release and summarize what's new",
];
