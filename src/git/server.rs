use super::{
    GitIdentity, ProjectInstructions, RepoStatus, RepoSummary, TrustRequest,
    SshKeySummary,
};
use crate::events::{self, ConversationEvent};
use crate::{db, sandbox};
use sqlx::PgPool;
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey, rand_core::OsRng};

#[cfg(test)]
mod db_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod trust_race_tests;

#[cfg(test)]
mod held_directory;

/// The race tests' holds and fixtures, for the trust server functions'
/// tests in `api::git` (SME-225), and, for the browser tier's late "Work
/// on a repo" failure (SME-263), its held directory and the count of that
/// hold's waiters.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) use super::db_tests::{cloned_repo, file};
    pub(crate) use super::trust_race_tests::{hold_row, waiters_or_finished, ONE_AT_A_TIME};
    #[cfg(feature = "browser-test")]
    pub(crate) use super::held_directory::{HeldDirectory, blocked_by, hold_directory};
}

mod keys;
mod instructions;
mod repos;
mod clone;
mod trust;

pub use keys::*;
pub use instructions::*;
pub use repos::*;
pub use clone::*;
pub use trust::*;
