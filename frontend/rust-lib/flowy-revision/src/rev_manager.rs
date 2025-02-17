use crate::disk::RevisionState;
use crate::{RevisionPersistence, RevisionSnapshotDiskCache, RevisionSnapshotManager, WSDataProviderDataSource};
use bytes::Bytes;
use flowy_error::{FlowyError, FlowyResult};
use flowy_sync::{
    entities::revision::{RepeatedRevision, Revision, RevisionRange},
    util::{pair_rev_id_from_revisions, RevIdCounter},
};
use lib_infra::future::FutureResult;
use std::sync::Arc;

pub trait RevisionCloudService: Send + Sync {
    /// Read the object's revision from remote
    /// Returns a list of revisions that used to build the object
    /// # Arguments
    ///
    /// * `user_id`: the id of the user
    /// * `object_id`: the id of the object
    ///
    fn fetch_object(&self, user_id: &str, object_id: &str) -> FutureResult<Vec<Revision>, FlowyError>;
}

pub trait RevisionObjectDeserializer: Send + Sync {
    type Output;
    /// Deserialize the list of revisions into an concrete object type.
    ///
    /// # Arguments
    ///
    /// * `object_id`: the id of the object
    /// * `revisions`: a list of revisions that represent the object
    ///
    fn deserialize_revisions(object_id: &str, revisions: Vec<Revision>) -> FlowyResult<Self::Output>;
}

pub trait RevisionObjectSerializer: Send + Sync {
    /// Serialize a list of revisions into one in `Bytes` format
    ///
    /// * `revisions`: a list of revisions will be serialized to `Bytes`
    ///
    fn combine_revisions(revisions: Vec<Revision>) -> FlowyResult<Bytes>;
}

/// `RevisionCompress` is used to compress multiple revisions into one revision
///
pub trait RevisionCompress: Send + Sync {
    fn compress_revisions(
        &self,
        user_id: &str,
        object_id: &str,
        mut revisions: Vec<Revision>,
    ) -> FlowyResult<Revision> {
        if revisions.is_empty() {
            return Err(FlowyError::internal().context("Can't compact the empty revisions"));
        }

        if revisions.len() == 1 {
            return Ok(revisions.pop().unwrap());
        }

        let first_revision = revisions.first().unwrap();
        let last_revision = revisions.last().unwrap();

        let (base_rev_id, rev_id) = first_revision.pair_rev_id();
        let md5 = last_revision.md5.clone();
        let bytes = self.combine_revisions(revisions)?;
        Ok(Revision::new(object_id, base_rev_id, rev_id, bytes, user_id, md5))
    }

    fn combine_revisions(&self, revisions: Vec<Revision>) -> FlowyResult<Bytes>;
}

pub struct RevisionManager<Connection> {
    pub object_id: String,
    user_id: String,
    rev_id_counter: RevIdCounter,
    rev_persistence: Arc<RevisionPersistence<Connection>>,
    #[allow(dead_code)]
    rev_snapshot: Arc<RevisionSnapshotManager>,
    rev_compress: Arc<dyn RevisionCompress>,
    #[cfg(feature = "flowy_unit_test")]
    rev_ack_notifier: tokio::sync::broadcast::Sender<i64>,
}

impl<Connection: 'static> RevisionManager<Connection> {
    pub fn new<SP, C>(
        user_id: &str,
        object_id: &str,
        rev_persistence: RevisionPersistence<Connection>,
        rev_compress: C,
        snapshot_persistence: SP,
    ) -> Self
    where
        SP: 'static + RevisionSnapshotDiskCache,
        C: 'static + RevisionCompress,
    {
        let rev_id_counter = RevIdCounter::new(0);
        let rev_compress = Arc::new(rev_compress);
        let rev_persistence = Arc::new(rev_persistence);
        let rev_snapshot = Arc::new(RevisionSnapshotManager::new(user_id, object_id, snapshot_persistence));
        #[cfg(feature = "flowy_unit_test")]
        let (revision_ack_notifier, _) = tokio::sync::broadcast::channel(1);

        Self {
            object_id: object_id.to_string(),
            user_id: user_id.to_owned(),
            rev_id_counter,
            rev_persistence,
            rev_snapshot,
            rev_compress,
            #[cfg(feature = "flowy_unit_test")]
            rev_ack_notifier: revision_ack_notifier,
        }
    }

    #[tracing::instrument(level = "debug", skip_all, fields(object_id) err)]
    pub async fn load<B>(&mut self, cloud: Option<Arc<dyn RevisionCloudService>>) -> FlowyResult<B::Output>
    where
        B: RevisionObjectDeserializer,
    {
        let (revisions, rev_id) = RevisionLoader {
            object_id: self.object_id.clone(),
            user_id: self.user_id.clone(),
            cloud,
            rev_persistence: self.rev_persistence.clone(),
        }
        .load()
        .await?;
        self.rev_id_counter.set(rev_id);
        tracing::Span::current().record("object_id", &self.object_id.as_str());
        B::deserialize_revisions(&self.object_id, revisions)
    }

    pub async fn load_revisions(&self) -> FlowyResult<Vec<Revision>> {
        let revisions = RevisionLoader {
            object_id: self.object_id.clone(),
            user_id: self.user_id.clone(),
            cloud: None,
            rev_persistence: self.rev_persistence.clone(),
        }
        .load_revisions()
        .await?;
        Ok(revisions)
    }

    #[tracing::instrument(level = "debug", skip(self, revisions), err)]
    pub async fn reset_object(&self, revisions: RepeatedRevision) -> FlowyResult<()> {
        let rev_id = pair_rev_id_from_revisions(&revisions).1;
        let _ = self.rev_persistence.reset(revisions.into_inner()).await?;
        self.rev_id_counter.set(rev_id);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, revision), err)]
    pub async fn add_remote_revision(&self, revision: &Revision) -> Result<(), FlowyError> {
        if revision.bytes.is_empty() {
            return Err(FlowyError::internal().context("Remote revisions is empty"));
        }

        let _ = self.rev_persistence.add_ack_revision(revision).await?;
        // self.rev_history.add_revision(revision).await;
        self.rev_id_counter.set(revision.rev_id);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all, err)]
    pub async fn add_local_revision(&self, revision: &Revision) -> Result<(), FlowyError> {
        if revision.bytes.is_empty() {
            return Err(FlowyError::internal().context("Local revisions is empty"));
        }
        let rev_id = self
            .rev_persistence
            .add_sync_revision(revision, &self.rev_compress)
            .await?;
        // self.rev_history.add_revision(revision).await;
        self.rev_id_counter.set(rev_id);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self), err)]
    pub async fn ack_revision(&self, rev_id: i64) -> Result<(), FlowyError> {
        if self.rev_persistence.ack_revision(rev_id).await.is_ok() {
            #[cfg(feature = "flowy_unit_test")]
            let _ = self.rev_ack_notifier.send(rev_id);
        }
        Ok(())
    }

    pub fn rev_id(&self) -> i64 {
        self.rev_id_counter.value()
    }

    pub fn next_rev_id_pair(&self) -> (i64, i64) {
        let cur = self.rev_id_counter.value();
        let next = self.rev_id_counter.next();
        (cur, next)
    }

    pub async fn get_revisions_in_range(&self, range: RevisionRange) -> Result<Vec<Revision>, FlowyError> {
        let revisions = self.rev_persistence.revisions_in_range(&range).await?;
        Ok(revisions)
    }

    pub async fn next_sync_revision(&self) -> FlowyResult<Option<Revision>> {
        self.rev_persistence.next_sync_revision().await
    }

    pub async fn get_revision(&self, rev_id: i64) -> Option<Revision> {
        self.rev_persistence.get(rev_id).await.map(|record| record.revision)
    }
}

impl<Connection: 'static> WSDataProviderDataSource for Arc<RevisionManager<Connection>> {
    fn next_revision(&self) -> FutureResult<Option<Revision>, FlowyError> {
        let rev_manager = self.clone();
        FutureResult::new(async move { rev_manager.next_sync_revision().await })
    }

    fn ack_revision(&self, rev_id: i64) -> FutureResult<(), FlowyError> {
        let rev_manager = self.clone();
        FutureResult::new(async move { (*rev_manager).ack_revision(rev_id).await })
    }

    fn current_rev_id(&self) -> i64 {
        self.rev_id()
    }
}

#[cfg(feature = "flowy_unit_test")]
impl<Connection> RevisionManager<Connection> {
    pub async fn revision_cache(&self) -> Arc<RevisionPersistence<Connection>> {
        self.rev_persistence.clone()
    }
    pub fn ack_notify(&self) -> tokio::sync::broadcast::Receiver<i64> {
        self.rev_ack_notifier.subscribe()
    }
}

pub struct RevisionLoader<Connection> {
    pub object_id: String,
    pub user_id: String,
    pub cloud: Option<Arc<dyn RevisionCloudService>>,
    pub rev_persistence: Arc<RevisionPersistence<Connection>>,
}

impl<Connection: 'static> RevisionLoader<Connection> {
    pub async fn load(&self) -> Result<(Vec<Revision>, i64), FlowyError> {
        let records = self.rev_persistence.batch_get(&self.object_id)?;
        let revisions: Vec<Revision>;
        let mut rev_id = 0;
        if records.is_empty() && self.cloud.is_some() {
            let remote_revisions = self
                .cloud
                .as_ref()
                .unwrap()
                .fetch_object(&self.user_id, &self.object_id)
                .await?;
            for revision in &remote_revisions {
                rev_id = revision.rev_id;
                let _ = self.rev_persistence.add_ack_revision(revision).await?;
            }
            revisions = remote_revisions;
        } else {
            for record in &records {
                rev_id = record.revision.rev_id;
                if record.state == RevisionState::Sync {
                    // Sync the records if their state is RevisionState::Sync.
                    let _ = self.rev_persistence.sync_revision(&record.revision).await?;
                }
            }
            revisions = records.into_iter().map(|record| record.revision).collect::<_>();
        }

        if let Some(revision) = revisions.last() {
            debug_assert_eq!(rev_id, revision.rev_id);
        }

        Ok((revisions, rev_id))
    }

    pub async fn load_revisions(&self) -> Result<Vec<Revision>, FlowyError> {
        let records = self.rev_persistence.batch_get(&self.object_id)?;
        let revisions = records.into_iter().map(|record| record.revision).collect::<_>();
        Ok(revisions)
    }
}
