//! Resolve a profile, open a connection, and load the top level of its tree.

use std::sync::Arc;

use async_trait::async_trait;
use sqlake_core::capability::Capabilities;
use sqlake_core::driver::Driver;
use sqlake_core::id::ProfileId;
use sqlake_core::node::{NodeRef, TreeNode};
use sqlake_core::profile::Profiles;

use crate::error::{AppError, AppResult};
use crate::session::SessionHandle;
use crate::usecase::UseCase;

#[derive(Debug)]
pub struct Connect {
    pub driver: Arc<dyn Driver>,
    pub profiles: Arc<dyn Profiles>,
}

#[derive(Debug, Clone)]
pub struct ConnectInput {
    pub profile: ProfileId,
    /// What the connection is called in the UI. Known from the profile before
    /// its secret is, so the connection has its real name while it is still
    /// waiting for a keyring.
    pub name: String,
}

#[derive(Debug)]
pub struct ConnectOutput {
    pub profile: ProfileId,
    pub name: String,
    pub session: SessionHandle,
    pub capabilities: Capabilities,
    /// The top level, fetched here so the tree is never briefly empty after a
    /// successful connection.
    pub roots: Vec<TreeNode>,
}

#[async_trait]
impl UseCase for Connect {
    type Input = ConnectInput;
    type Output = ConnectOutput;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output> {
        let profile = self.resolve(input.profile.clone()).await?;
        let session = SessionHandle::spawn(self.driver.connect(&profile).await?);
        let capabilities = session.capabilities();
        let roots = session.children(NodeRef::root()).await?;
        Ok(ConnectOutput {
            profile: input.profile,
            name: input.name,
            session,
            capabilities,
            roots,
        })
    }
}

impl Connect {
    /// Read the profile's secret without blocking the runtime.
    ///
    /// Resolution can talk to a keyring, and a keyring can put a dialog on the
    /// screen and wait for a fingerprint. On the runtime's own threads that
    /// would stall every other connection, every running query and the frame
    /// after this one.
    async fn resolve(&self, id: ProfileId) -> AppResult<sqlake_core::profile::ResolvedProfile> {
        let profiles = Arc::clone(&self.profiles);
        tokio::task::spawn_blocking(move || profiles.resolve(&id))
            .await
            .map_err(|_| AppError::Profile("resolving the profile did not finish".to_owned()))?
            .map_err(|err| AppError::Profile(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::profile::{ProfileError, ProfileSummary, ResolvedProfile};
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles, mock_profile};

    use super::*;

    fn input(id: &str) -> ConnectInput {
        ConnectInput {
            profile: ProfileId::parse(id).unwrap(),
            name: id.to_owned(),
        }
    }

    fn connect(profiles: Arc<dyn Profiles>) -> Connect {
        Connect {
            driver: Arc::new(MockDriver::new(Behaviour::instant())),
            profiles,
        }
    }

    #[tokio::test]
    async fn connecting_also_loads_the_top_level() {
        let out = connect(Arc::new(MockProfiles::default()))
            .execute(input("mock"))
            .await
            .expect("should connect");
        assert_eq!(out.profile.as_str(), "mock");
        assert!(!out.roots.is_empty());
    }

    #[tokio::test]
    async fn a_profile_that_does_not_resolve_never_reaches_the_driver() {
        // The driver would happily connect; the profile is what refuses. The
        // failure has to arrive as an error rather than as a live session for
        // a connection nobody configured.
        let err = connect(Arc::new(MockProfiles::default()))
            .execute(input("nope"))
            .await
            .expect_err("should not connect");
        assert!(err.user_message().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn a_failure_to_read_a_secret_is_reported_as_itself() {
        /// Stands in for a locked keyring, or a password command that failed.
        #[derive(Debug)]
        struct LockedKeyring;

        impl Profiles for LockedKeyring {
            fn list(&self) -> Vec<ProfileSummary> {
                MockProfiles::default().list()
            }

            fn resolve(&self, _: &ProfileId) -> Result<ResolvedProfile, ProfileError> {
                Err(ProfileError::new("the keyring is locked"))
            }
        }

        let err = connect(Arc::new(LockedKeyring))
            .execute(input("mock"))
            .await
            .expect_err("should not connect");
        assert!(err.user_message().contains("keyring is locked"), "{err}");
    }

    #[tokio::test]
    async fn a_profile_for_another_driver_is_refused_by_the_driver() {
        /// A profile that resolves fine and describes something else entirely.
        #[derive(Debug)]
        struct WrongKind;

        impl Profiles for WrongKind {
            fn list(&self) -> Vec<ProfileSummary> {
                MockProfiles::default().list()
            }

            fn resolve(&self, id: &ProfileId) -> Result<ResolvedProfile, ProfileError> {
                Ok(ResolvedProfile {
                    params: sqlake_core::profile::Params::Postgres(
                        sqlake_core::profile::PostgresParams {
                            host: "db.internal".to_owned(),
                            port: 5432,
                            database: "app".to_owned(),
                            user: "readonly".to_owned(),
                            sslmode: sqlake_core::profile::SslMode::DEFAULT,
                            password: None,
                        },
                    ),
                    ..mock_profile(id.as_str())
                })
            }
        }

        let err = connect(Arc::new(WrongKind))
            .execute(input("mock"))
            .await
            .expect_err("should not connect");
        assert!(err.user_message().contains("not a mock profile"), "{err}");
    }
}
