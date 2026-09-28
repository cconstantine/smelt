//! Language servers for the model (SME-35): each runs in a pod of its own
//! next to the conversation's sandbox, and smelt talks LSP to it over a
//! `pods/exec` stream.

pub mod client;
