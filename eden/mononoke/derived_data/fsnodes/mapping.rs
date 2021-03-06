/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};

use anyhow::{Error, Result};
use async_trait::async_trait;
use blobrepo::BlobRepo;
use blobstore::{Blobstore, BlobstoreGetData};
use bytes::Bytes;
use context::CoreContext;
use derived_data::{
    impl_bonsai_derived_mapping, BlobstoreRootIdMapping, BonsaiDerivable, BonsaiDerivedMapping,
    DerivedDataTypesConfig,
};
use futures::stream::{self, StreamExt, TryStreamExt};
use mononoke_types::{
    BlobstoreBytes, BonsaiChangeset, ChangesetId, ContentId, FileType, FsnodeId, MPath,
};
use repo_blobstore::RepoBlobstore;

use crate::batch::derive_fsnode_in_batch;
use crate::derive::derive_fsnode;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RootFsnodeId(FsnodeId);

impl RootFsnodeId {
    pub fn fsnode_id(&self) -> &FsnodeId {
        &self.0
    }
    pub fn into_fsnode_id(self) -> FsnodeId {
        self.0
    }
}

impl TryFrom<BlobstoreBytes> for RootFsnodeId {
    type Error = Error;

    fn try_from(blob_bytes: BlobstoreBytes) -> Result<Self> {
        FsnodeId::from_bytes(&blob_bytes.into_bytes()).map(RootFsnodeId)
    }
}

impl TryFrom<BlobstoreGetData> for RootFsnodeId {
    type Error = Error;

    fn try_from(blob_get_data: BlobstoreGetData) -> Result<Self> {
        blob_get_data.into_bytes().try_into()
    }
}

impl From<RootFsnodeId> for BlobstoreBytes {
    fn from(root_fsnode_id: RootFsnodeId) -> Self {
        BlobstoreBytes::from_bytes(Bytes::copy_from_slice(root_fsnode_id.0.blake2().as_ref()))
    }
}

#[async_trait]
impl BonsaiDerivable for RootFsnodeId {
    const NAME: &'static str = "fsnodes";

    type Options = ();

    async fn derive_from_parents_impl(
        ctx: CoreContext,
        repo: BlobRepo,
        bonsai: BonsaiChangeset,
        parents: Vec<Self>,
        _options: &Self::Options,
    ) -> Result<Self, Error> {
        let fsnode_id = derive_fsnode(
            &ctx,
            &repo,
            parents
                .into_iter()
                .map(|root_fsnode_id| root_fsnode_id.into_fsnode_id())
                .collect(),
            get_file_changes(&bonsai),
        )
        .await?;
        Ok(RootFsnodeId(fsnode_id))
    }

    async fn batch_derive_impl<BatchMapping>(
        ctx: &CoreContext,
        repo: &BlobRepo,
        csids: Vec<ChangesetId>,
        mapping: &BatchMapping,
        gap_size: Option<usize>,
    ) -> Result<HashMap<ChangesetId, Self>, Error>
    where
        BatchMapping: BonsaiDerivedMapping<Value = Self> + Send + Sync + Clone + 'static,
    {
        let derived = derive_fsnode_in_batch(ctx, repo, mapping, csids.clone(), gap_size).await?;

        stream::iter(derived.into_iter().map(|(cs_id, derived)| async move {
            let derived = RootFsnodeId(derived);
            mapping
                .put(ctx.clone(), cs_id.clone(), derived.clone())
                .await?;
            Ok((cs_id, derived))
        }))
        .buffered(100)
        .try_collect::<HashMap<_, _>>()
        .await
    }
}

#[derive(Clone)]
pub struct RootFsnodeMapping {
    blobstore: RepoBlobstore,
}

#[async_trait]
impl BlobstoreRootIdMapping for RootFsnodeMapping {
    type Value = RootFsnodeId;

    fn new(repo: &BlobRepo, _config: &DerivedDataTypesConfig) -> Result<Self> {
        Ok(Self {
            blobstore: repo.get_blobstore(),
        })
    }

    fn blobstore(&self) -> &dyn Blobstore {
        &self.blobstore
    }

    fn prefix(&self) -> &'static str {
        "derived_root_fsnode."
    }

    fn options(&self) {}
}

impl_bonsai_derived_mapping!(RootFsnodeMapping, BlobstoreRootIdMapping, RootFsnodeId);

pub(crate) fn get_file_changes(
    bcs: &BonsaiChangeset,
) -> Vec<(MPath, Option<(ContentId, FileType)>)> {
    bcs.file_changes()
        .map(|(mpath, file_change)| {
            (
                mpath.clone(),
                file_change.map(|file_change| (file_change.content_id(), file_change.file_type())),
            )
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;
    use blobrepo_hg::BlobRepoHg;
    use blobstore::Loadable;
    use bookmarks::BookmarkName;
    use borrowed::borrowed;
    use derived_data::BonsaiDerived;
    use derived_data_test_utils::iterate_all_manifest_entries;
    use fbinit::FacebookInit;
    use fixtures::{
        branch_even, branch_uneven, branch_wide, linear, many_diamonds, many_files_dirs,
        merge_even, merge_uneven, unshared_merge_even, unshared_merge_uneven,
    };
    use futures::compat::Stream01CompatExt;
    use futures::future::Future;
    use futures::stream::Stream;
    use futures::try_join;
    use manifest::Entry;
    use mercurial_types::{HgChangesetId, HgManifestId};
    use revset::AncestorsNodeStream;
    use tokio_compat::runtime::Runtime;

    async fn fetch_manifest_by_cs_id(
        ctx: &CoreContext,
        repo: &BlobRepo,
        hg_cs_id: HgChangesetId,
    ) -> Result<HgManifestId> {
        Ok(hg_cs_id.load(ctx, repo.blobstore()).await?.manifestid())
    }

    async fn verify_fsnode(
        ctx: &CoreContext,
        repo: &BlobRepo,
        bcs_id: ChangesetId,
        hg_cs_id: HgChangesetId,
    ) -> Result<()> {
        let root_fsnode_id = RootFsnodeId::derive(ctx, repo, bcs_id)
            .await?
            .into_fsnode_id();

        let fsnode_entries = iterate_all_manifest_entries(ctx, repo, Entry::Tree(root_fsnode_id))
            .map_ok(|(path, _)| path)
            .try_collect::<Vec<_>>();

        let root_mf_id = fetch_manifest_by_cs_id(ctx, repo, hg_cs_id).await?;

        let filenode_entries = iterate_all_manifest_entries(ctx, repo, Entry::Tree(root_mf_id))
            .map_ok(|(path, _)| path)
            .try_collect::<Vec<_>>();

        let (mut fsnode_entries, mut filenode_entries) =
            try_join!(fsnode_entries, filenode_entries)?;
        fsnode_entries.sort();
        filenode_entries.sort();
        assert_eq!(fsnode_entries, filenode_entries);
        Ok(())
    }

    async fn all_commits<'a>(
        ctx: &'a CoreContext,
        repo: &'a BlobRepo,
    ) -> Result<impl Stream<Item = Result<(ChangesetId, HgChangesetId)>> + 'a> {
        let master_book = BookmarkName::new("master").unwrap();
        let bcs_id = repo
            .get_bonsai_bookmark(ctx.clone(), &master_book)
            .await?
            .unwrap();

        Ok(
            AncestorsNodeStream::new(ctx.clone(), &repo.get_changeset_fetcher(), bcs_id.clone())
                .compat()
                .and_then(move |new_bcs_id| async move {
                    let hg_cs_id = repo
                        .get_hg_from_bonsai_changeset(ctx.clone(), new_bcs_id)
                        .await?;
                    Ok((new_bcs_id, hg_cs_id))
                }),
        )
    }

    fn verify_repo<F>(fb: FacebookInit, repo: F, runtime: &mut Runtime)
    where
        F: Future<Output = BlobRepo>,
    {
        let ctx = CoreContext::test_mock(fb);
        let repo = runtime.block_on_std(repo);
        borrowed!(ctx, repo);

        runtime
            .block_on_std(async move {
                all_commits(ctx, repo)
                    .await
                    .unwrap()
                    .try_for_each(move |(bcs_id, hg_cs_id)| async move {
                        verify_fsnode(ctx, repo, bcs_id, hg_cs_id).await
                    })
                    .await
            })
            .unwrap();
    }

    #[fbinit::test]
    fn test_derive_data(fb: FacebookInit) {
        let mut runtime = Runtime::new().unwrap();
        verify_repo(fb, linear::getrepo(fb), &mut runtime);
        verify_repo(fb, branch_even::getrepo(fb), &mut runtime);
        verify_repo(fb, branch_uneven::getrepo(fb), &mut runtime);
        verify_repo(fb, branch_wide::getrepo(fb), &mut runtime);
        verify_repo(fb, many_diamonds::getrepo(fb), &mut runtime);
        verify_repo(fb, many_files_dirs::getrepo(fb), &mut runtime);
        verify_repo(fb, merge_even::getrepo(fb), &mut runtime);
        verify_repo(fb, merge_uneven::getrepo(fb), &mut runtime);
        verify_repo(fb, unshared_merge_even::getrepo(fb), &mut runtime);
        verify_repo(fb, unshared_merge_uneven::getrepo(fb), &mut runtime);
    }
}
