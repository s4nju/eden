// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::HashMap;
use std::io::Cursor;
use std::ops::AddAssign;
use std::sync::Arc;

use ascii::AsciiString;
use blobrepo::{BlobRepo, ChangesetHandle, ChangesetMetadata, ContentBlobInfo, CreateChangeset,
               HgBlobEntry};
use bookmarks::{Bookmark, Transaction};
use bytes::{Bytes, BytesMut};
use failure::{err_msg, Compat, FutureFailureErrorExt, StreamFailureErrorExt};
use futures::{Future, IntoFuture, Stream};
use futures::future::{self, err, ok, Shared};
use futures::stream;
use futures_ext::{BoxFuture, BoxStream, FutureExt, StreamExt};
use futures_stats::Timed;
use getbundle_response;
use mercurial::changeset::RevlogChangeset;
use mercurial::manifest::{Details, ManifestContent};
use mercurial_bundles::{create_bundle_stream, parts, Bundle2EncodeBuilder, Bundle2Item};
use mercurial_types::{HgChangesetId, HgManifestId, HgNodeHash, HgNodeKey, MPath, RepoPath,
                      NULL_HASH};
use metaconfig::PushrebaseParams;
use mononoke_types::ChangesetId;
use pushrebase;
use reachabilityindex::LeastCommonAncestorsHint;
use scuba_ext::{ScubaSampleBuilder, ScubaSampleBuilderExt};
use slog::Logger;
use stats::*;

use changegroup::{convert_to_revlog_changesets, convert_to_revlog_filelog, split_changegroup};
use errors::*;
use hooks::{ChangesetHookExecutionID, FileHookExecutionID, HookExecution, HookManager};
use upload_blobs::{upload_hg_blobs, UploadBlobsType, UploadableHgBlob};
use wirepackparser::{TreemanifestBundle2Parser, TreemanifestEntry};

type PartId = u32;
type Changesets = Vec<(HgNodeHash, RevlogChangeset)>;
type Filelogs = HashMap<HgNodeKey, Shared<BoxFuture<(HgBlobEntry, RepoPath), Compat<Error>>>>;
type ContentBlobs = HashMap<HgNodeKey, ContentBlobInfo>;
type Manifests = HashMap<HgNodeKey, <TreemanifestEntry as UploadableHgBlob>::Value>;
type UploadedChangesets = HashMap<HgNodeHash, ChangesetHandle>;

/// The resolve function takes a bundle2, interprets it's content as Changesets, Filelogs and
/// Manifests and uploades all of them to the provided BlobRepo in the correct order.
/// It returns a Future that contains the response that should be send back to the requester.
pub fn resolve(
    repo: Arc<BlobRepo>,
    logger: Logger,
    scuba_logger: ScubaSampleBuilder,
    pushrebase: PushrebaseParams,
    _heads: Vec<String>,
    bundle2: BoxStream<Bundle2Item, Error>,
    hook_manager: Arc<HookManager>,
    lca_hint: Arc<LeastCommonAncestorsHint + Send + Sync>,
) -> BoxFuture<Bytes, Error> {
    let resolver = Bundle2Resolver::new(repo, logger, scuba_logger, pushrebase, hook_manager);

    let bundle2 = resolver.resolve_start_and_replycaps(bundle2);

    resolver
        .maybe_resolve_commonheads(bundle2)
        .and_then(move |(commonheads, bundle2)| match commonheads {
            Some(commonheads) => resolve_pushrebase(commonheads, resolver, bundle2, lca_hint),
            None => resolve_push(resolver, bundle2),
        })
        .boxify()
}

fn resolve_push(
    resolver: Bundle2Resolver,
    bundle2: BoxStream<Bundle2Item, Error>,
) -> BoxFuture<Bytes, Error> {
    resolver
        .maybe_resolve_changegroup(bundle2)
        .and_then({
            let resolver = resolver.clone();
            move |(cg_push, bundle2)| {
                resolver
                    .resolve_multiple_parts(bundle2, Bundle2Resolver::maybe_resolve_pushkey)
                    .map(move |(pushkeys, bundle2)| {
                        let bookmark_push: Vec<_> = pushkeys
                            .into_iter()
                            .filter_map(|pushkey| match pushkey {
                                Pushkey::Phases => None,
                                Pushkey::BookmarkPush(bp) => Some(bp),
                            })
                            .collect();

                        STATS::bookmark_pushkeys_count.add_value(bookmark_push.len() as i64);

                        (cg_push, bookmark_push, bundle2)
                    })
            }
        })
        .and_then({
            let resolver = resolver.clone();
            move |(cg_push, bookmark_push, bundle2)| {
                if let Some(cg_push) = cg_push {
                    resolver
                        .resolve_b2xtreegroup2(bundle2)
                        .map(|(manifests, bundle2)| {
                            (Some((cg_push, manifests)), bookmark_push, bundle2)
                        })
                        .boxify()
                } else {
                    ok((None, bookmark_push, bundle2)).boxify()
                }
            }
        })
        .and_then({
            let resolver = resolver.clone();
            move |(cg_and_manifests, bookmark_push, bundle2)| {
                if let Some((cg_push, manifests)) = cg_and_manifests {
                    let changegroup_id = Some(cg_push.part_id);
                    resolver
                        .upload_changesets(cg_push, manifests)
                        .map(move |()| (changegroup_id, bookmark_push, bundle2))
                        .boxify()
                } else {
                    ok((None, bookmark_push, bundle2)).boxify()
                }
            }
        })
        .and_then({
            let resolver = resolver.clone();
            move |(changegroup_id, bookmark_push, bundle2)| {
                resolver
                    .maybe_resolve_infinitepush_bookmarks(bundle2)
                    .map(move |((), bundle2)| (changegroup_id, bookmark_push, bundle2))
            }
        })
        .and_then({
            let resolver = resolver.clone();
            move |(changegroup_id, bookmark_push, bundle2)| {
                resolver
                    .ensure_stream_finished(bundle2)
                    .map(move |()| (changegroup_id, bookmark_push))
            }
        })
        .and_then({
            let resolver = resolver.clone();
            move |(changegroup_id, bookmarks_push)| {
                let bookmarks_push_fut = bookmarks_push
                    .into_iter()
                    .map(|bp| BonsaiBookmarkPush::new(&resolver.repo, bp))
                    .collect::<Vec<_>>();
                future::join_all(bookmarks_push_fut)
                    .map(move |bookmakrs_push| (changegroup_id, bookmakrs_push))
            }
        })
        .and_then({
            let resolver = resolver.clone();
            move |(changegroup_id, bookmark_push)| {
                (move || {
                    let bookmark_ids: Vec<_> = bookmark_push.iter().map(|bp| bp.part_id).collect();

                    let mut txn = resolver.repo.update_bookmark_transaction();
                    for bp in bookmark_push {
                        try_boxfuture!(add_bookmark_to_transaction(&mut txn, bp));
                    }
                    txn.commit()
                        .and_then(|ok| {
                            if ok {
                                Ok(())
                            } else {
                                Err(format_err!("Bookmark transaction failed"))
                            }
                        })
                        .map(move |()| (changegroup_id, bookmark_ids))
                        .boxify()
                })()
                    .context("While updating Bookmarks")
                    .from_err()
            }
        })
        .and_then(move |(changegroup_id, bookmark_ids)| {
            resolver.prepare_push_response(changegroup_id, bookmark_ids)
        })
        .context("bundle2-resolver error")
        .from_err()
        .boxify()
}

fn resolve_pushrebase(
    commonheads: CommonHeads,
    resolver: Bundle2Resolver,
    bundle2: BoxStream<Bundle2Item, Error>,
    lca_hint: Arc<LeastCommonAncestorsHint + Send + Sync>,
) -> BoxFuture<Bytes, Error> {
    resolver
        .maybe_resolve_pushvars(bundle2)
        .and_then({
            cloned!(resolver);
            move |(maybe_pushvars, bundle2)| {
                resolver
                    .resolve_b2xtreegroup2(bundle2)
                    .map(move |(manifests, bundle2)| (manifests, maybe_pushvars, bundle2))
            }
        })
        .and_then({
            cloned!(resolver);
            move |(manifests, maybe_pushvars, bundle2)| {
                resolver
                    .maybe_resolve_changegroup(bundle2)
                    .map(move |(cg_push, bundle2)| (cg_push, manifests, maybe_pushvars, bundle2))
            }
        })
        .and_then(|(cg_push, manifests, maybe_pushvars, bundle2)| {
            cg_push
                .ok_or(err_msg("Empty pushrebase"))
                .into_future()
                .map(move |cg_push| (cg_push, manifests, maybe_pushvars, bundle2))
        })
        .and_then(|(cg_push, manifests, maybe_pushvars, bundle2)| {
            match cg_push.mparams.get("onto").cloned() {
                Some(onto_bookmark) => {
                    let v = Vec::from(onto_bookmark.as_ref());
                    let onto_bookmark = String::from_utf8(v)?;
                    let onto_bookmark = Bookmark::new(onto_bookmark)?;

                    Ok((onto_bookmark, cg_push, manifests, maybe_pushvars, bundle2))
                }
                None => Err(err_msg("onto is not specified")),
            }
        })
        .and_then({
            cloned!(resolver);
            move |(onto, cg_push, manifests, maybe_pushvars, bundle2)| {
                let changesets = cg_push.changesets.clone();
                resolver
                    .upload_changesets(cg_push, manifests)
                    .map(move |()| (changesets, onto, maybe_pushvars, bundle2))
            }
        })
        .and_then({
            cloned!(resolver);
            move |(changesets, onto, maybe_pushvars, bundle2)| {
                resolver
                    .resolve_multiple_parts(bundle2, Bundle2Resolver::maybe_resolve_pushkey)
                    .and_then({
                        cloned!(resolver);
                        move |(pushkeys, bundle2)| {
                            let bookmark_pushes: Vec<_> = pushkeys
                                .into_iter()
                                .filter_map(|pushkey| match pushkey {
                                    Pushkey::Phases => None,
                                    Pushkey::BookmarkPush(bp) => Some(bp),
                                })
                                .collect();

                            resolver
                                .ensure_stream_finished(bundle2)
                                .map(move |()| (changesets, bookmark_pushes, maybe_pushvars, onto))
                        }
                    })
            }
        })
        .and_then({
            cloned!(resolver);
            move |(changesets, bookmark_pushes, maybe_pushvars, onto)| {
                resolver
                    .run_hooks(changesets.clone(), maybe_pushvars, &onto)
                    .map_err(|err| match err {
                        RunHooksError::Failures((cs_hook_failures, file_hook_failures)) => {
                            let mut err_msgs = vec![];
                            for (exec_id, exec_info) in cs_hook_failures {
                                if let HookExecution::Rejected(info) = exec_info {
                                    err_msgs.push(format!(
                                        "{} for {}: {}",
                                        exec_id.hook_name, exec_id.cs_id, info.description
                                    ));
                                }
                            }
                            for (exec_id, exec_info) in file_hook_failures {
                                if let HookExecution::Rejected(info) = exec_info {
                                    err_msgs.push(format!(
                                        "{} for {}: {}",
                                        exec_id.hook_name, exec_id.cs_id, info.description
                                    ));
                                }
                            }
                            err_msg(format!("hooks failed:\n{}", err_msgs.join("\n")))
                        }
                        RunHooksError::Error(err) => err,
                    })
                    .and_then(move |()| {
                        resolver
                            .pushrebase(changesets.clone(), bookmark_pushes, &onto)
                            .map(|pushrebased_rev| (pushrebased_rev, onto))
                    })
            }
        })
        .and_then({
            cloned!(resolver);
            move |(pushrebased_rev, onto)| {
                resolver.prepare_pushrebase_response(commonheads, pushrebased_rev, onto, lca_hint)
            }
        })
        .boxify()
}

fn next_item(
    bundle2: BoxStream<Bundle2Item, Error>,
) -> BoxFuture<(Option<Bundle2Item>, BoxStream<Bundle2Item, Error>), Error> {
    bundle2.into_future().map_err(|(err, _)| err).boxify()
}

struct ChangegroupPush {
    part_id: PartId,
    changesets: Changesets,
    filelogs: Filelogs,
    content_blobs: ContentBlobs,
    mparams: HashMap<String, Bytes>,
}

struct CommonHeads {
    heads: Vec<HgChangesetId>,
}

enum Pushkey {
    BookmarkPush(BookmarkPush),
    Phases,
}

#[derive(Debug)]
struct BookmarkPush {
    part_id: PartId,
    name: Bookmark,
    old: Option<HgChangesetId>,
    new: Option<HgChangesetId>,
}

struct BonsaiBookmarkPush {
    part_id: PartId,
    name: Bookmark,
    old: Option<ChangesetId>,
    new: Option<ChangesetId>,
}

impl BonsaiBookmarkPush {
    fn new(
        repo: &Arc<BlobRepo>,
        bookmark_push: BookmarkPush,
    ) -> impl Future<Item = BonsaiBookmarkPush, Error = Error> + Send {
        fn bonsai_from_hg_opt(
            repo: &Arc<BlobRepo>,
            cs_id: Option<HgChangesetId>,
        ) -> impl Future<Item = Option<ChangesetId>, Error = Error> {
            match cs_id {
                None => future::ok(None).left_future(),
                Some(cs_id) => repo.get_bonsai_from_hg(&cs_id).right_future(),
            }
        }

        let BookmarkPush {
            part_id,
            name,
            old,
            new,
        } = bookmark_push;

        (bonsai_from_hg_opt(repo, old), bonsai_from_hg_opt(repo, new))
            .into_future()
            .map(move |(old, new)| BonsaiBookmarkPush {
                part_id,
                name,
                old,
                new,
            })
    }
}

/// Holds repo and logger for convienience access from it's methods
#[derive(Clone)]
struct Bundle2Resolver {
    repo: Arc<BlobRepo>,
    logger: Logger,
    scuba_logger: ScubaSampleBuilder,
    pushrebase: PushrebaseParams,
    hook_manager: Arc<HookManager>,
}

impl Bundle2Resolver {
    fn new(
        repo: Arc<BlobRepo>,
        logger: Logger,
        scuba_logger: ScubaSampleBuilder,
        pushrebase: PushrebaseParams,
        hook_manager: Arc<HookManager>,
    ) -> Self {
        Self {
            repo,
            logger,
            scuba_logger,
            pushrebase,
            hook_manager,
        }
    }

    /// Parse Start and Replycaps and ignore their content
    fn resolve_start_and_replycaps(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxStream<Bundle2Item, Error> {
        next_item(bundle2)
            .and_then(|(start, bundle2)| match start {
                Some(Bundle2Item::Start(_)) => next_item(bundle2),
                _ => err(format_err!("Expected Bundle2 Start")).boxify(),
            })
            .and_then(|(replycaps, bundle2)| match replycaps {
                Some(Bundle2Item::Replycaps(_, part)) => part.map(|_| bundle2).boxify(),
                _ => err(format_err!("Expected Bundle2 Replycaps")).boxify(),
            })
            .flatten_stream()
            .boxify()
    }

    // Parse b2x:commonheads
    // This part sent by pushrebase so that server can find out what commits to send back to the
    // client. This part is used as a marker that this push is pushrebase.
    fn maybe_resolve_commonheads(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<(Option<CommonHeads>, BoxStream<Bundle2Item, Error>), Error> {
        next_item(bundle2)
            .and_then(|(commonheads, bundle2)| match commonheads {
                Some(Bundle2Item::B2xCommonHeads(_header, heads)) => heads
                    .collect()
                    .map(|heads| {
                        let heads = CommonHeads { heads };
                        (Some(heads), bundle2)
                    })
                    .boxify(),
                Some(part) => ok((None, stream::once(Ok(part)).chain(bundle2).boxify())).boxify(),
                _ => err(format_err!("Unexpected Bundle2 stream end")).boxify(),
            })
            .boxify()
    }

    /// Parse pushvars
    /// It is used to store hook arguments.
    fn maybe_resolve_pushvars(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<
        (
            Option<HashMap<String, Bytes>>,
            BoxStream<Bundle2Item, Error>,
        ),
        Error,
    > {
        next_item(bundle2)
            .and_then(move |(newpart, bundle2)| match newpart {
                Some(Bundle2Item::Pushvars(header, emptypart)) => {
                    let pushvars = header.aparams().clone();
                    // ignored for now, will be used for hooks
                    emptypart.map(move |_| (Some(pushvars), bundle2)).boxify()
                }
                Some(part) => ok((None, stream::once(Ok(part)).chain(bundle2).boxify())).boxify(),
                None => ok((None, bundle2)).boxify(),
            })
            .context("While resolving Pushvars")
            .from_err()
            .boxify()
    }

    /// Parse changegroup.
    /// The ChangegroupId will be used in the last step for preparing response
    /// The Changesets should be parsed as RevlogChangesets and used for uploading changesets
    /// The Filelogs should be scheduled for uploading to BlobRepo and the Future resolving in
    /// their upload should be used for uploading changesets
    fn maybe_resolve_changegroup(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<(Option<ChangegroupPush>, BoxStream<Bundle2Item, Error>), Error> {
        let repo = self.repo.clone();

        next_item(bundle2)
            .and_then(move |(changegroup, bundle2)| match changegroup {
                // XXX: we may be interested in checking that this is a correct changegroup part
                // type
                Some(Bundle2Item::Changegroup(header, parts))
                | Some(Bundle2Item::B2xInfinitepush(header, parts))
                | Some(Bundle2Item::B2xRebase(header, parts)) => {
                    let part_id = header.part_id();
                    let (c, f) = split_changegroup(parts);
                    convert_to_revlog_changesets(c)
                        .collect()
                        .and_then(|changesets| {
                            upload_hg_blobs(
                                repo.clone(),
                                convert_to_revlog_filelog(repo, f),
                                UploadBlobsType::EnsureNoDuplicates,
                            ).map(move |upload_map| {
                                let mut filelogs = HashMap::new();
                                let mut content_blobs = HashMap::new();
                                for (node_key, (cbinfo, file_upload)) in upload_map {
                                    filelogs.insert(node_key.clone(), file_upload);
                                    content_blobs.insert(node_key, cbinfo);
                                }
                                (changesets, filelogs, content_blobs)
                            })
                                .context("While uploading File Blobs")
                                .from_err()
                        })
                        .map(move |(changesets, filelogs, content_blobs)| {
                            let cg_push = ChangegroupPush {
                                part_id,
                                changesets,
                                filelogs,
                                content_blobs,
                                mparams: header.mparams().clone(),
                            };
                            (Some(cg_push), bundle2)
                        })
                        .boxify()
                }
                Some(part) => ok((None, stream::once(Ok(part)).chain(bundle2).boxify())).boxify(),
                _ => err(format_err!("Unexpected Bundle2 stream end")).boxify(),
            })
            .context("While resolving Changegroup")
            .from_err()
            .boxify()
    }

    /// Parses pushkey part if it exists
    /// Returns an error if the pushkey namespace is unknown
    fn maybe_resolve_pushkey(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<(Option<Pushkey>, BoxStream<Bundle2Item, Error>), Error> {
        next_item(bundle2)
            .and_then(move |(newpart, bundle2)| match newpart {
                Some(Bundle2Item::Pushkey(header, emptypart)) => {
                    let namespace = try_boxfuture!(
                        header
                            .mparams()
                            .get("namespace")
                            .ok_or(format_err!("pushkey: `namespace` parameter is not set"))
                    );

                    let pushkey = match &namespace[..] {
                        b"phases" => Pushkey::Phases,
                        b"bookmarks" => {
                            let part_id = header.part_id();
                            let mparams = header.mparams();
                            let name = try_boxfuture!(get_ascii_param(mparams, "key"));
                            let name = Bookmark::new_ascii(name);
                            let old = try_boxfuture!(get_optional_changeset_param(mparams, "old"));
                            let new = try_boxfuture!(get_optional_changeset_param(mparams, "new"));

                            Pushkey::BookmarkPush(BookmarkPush {
                                part_id,
                                name,
                                old,
                                new,
                            })
                        }
                        _ => {
                            return err(format_err!(
                                "pushkey: unexpected namespace: {:?}",
                                namespace
                            )).boxify()
                        }
                    };

                    emptypart.map(move |_| (Some(pushkey), bundle2)).boxify()
                }
                Some(part) => ok((None, stream::once(Ok(part)).chain(bundle2).boxify())).boxify(),
                None => ok((None, bundle2)).boxify(),
            })
            .context("While resolving Pushkey")
            .from_err()
            .boxify()
    }

    /// Parse b2xtreegroup2.
    /// The Manifests should be scheduled for uploading to BlobRepo and the Future resolving in
    /// their upload as well as their parsed content should be used for uploading changesets.
    fn resolve_b2xtreegroup2(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<(Manifests, BoxStream<Bundle2Item, Error>), Error> {
        let repo = self.repo.clone();

        next_item(bundle2)
            .and_then(move |(b2xtreegroup2, bundle2)| match b2xtreegroup2 {
                Some(Bundle2Item::B2xTreegroup2(_, parts))
                | Some(Bundle2Item::B2xRebasePack(_, parts)) => {
                    upload_hg_blobs(
                        repo,
                        TreemanifestBundle2Parser::new(parts),
                        UploadBlobsType::IgnoreDuplicates,
                    ).context("While uploading Manifest Blobs")
                        .from_err()
                        .map(move |manifests| (manifests, bundle2))
                        .boxify()
                }
                _ => err(format_err!("Expected Bundle2 B2xTreegroup2")).boxify(),
            })
            .context("While resolving B2xTreegroup2")
            .from_err()
            .boxify()
    }

    /// Parse b2xinfinitepushscratchbookmarks.
    /// This part is ignored, so just parse it and forget it
    fn maybe_resolve_infinitepush_bookmarks(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<((), BoxStream<Bundle2Item, Error>), Error> {
        next_item(bundle2)
            .and_then(
                move |(infinitepushbookmarks, bundle2)| match infinitepushbookmarks {
                    Some(Bundle2Item::B2xInfinitepushBookmarks(_, bookmarks)) => {
                        bookmarks.collect().map(|_| ((), bundle2)).boxify()
                    }
                    None => Ok(((), bundle2)).into_future().boxify(),
                    _ => err(format_err!(
                        "Expected B2xInfinitepushBookmarks or end of the stream"
                    )).boxify(),
                },
            )
            .context("While resolving B2xInfinitepushBookmarks")
            .from_err()
            .boxify()
    }

    /// Takes parsed Changesets and scheduled for upload Filelogs and Manifests. The content of
    /// Manifests is used to figure out DAG of dependencies between a given Changeset and the
    /// Manifests and Filelogs it adds.
    /// The Changesets are scheduled for uploading and a Future is returned, whose completion means
    /// that the changesets were uploaded
    fn upload_changesets(
        &self,
        cg_push: ChangegroupPush,
        manifests: Manifests,
    ) -> BoxFuture<(), Error> {
        let changesets = cg_push.changesets;
        let filelogs = cg_push.filelogs;
        let content_blobs = cg_push.content_blobs;

        self.scuba_logger
            .clone()
            .add("changeset_count", changesets.len())
            .add("manifests_count", manifests.len())
            .add("filelogs_count", filelogs.len())
            .log_with_msg("Size of unbundle", None);

        STATS::changesets_count.add_value(changesets.len() as i64);
        STATS::manifests_count.add_value(manifests.len() as i64);
        STATS::filelogs_count.add_value(filelogs.len() as i64);
        STATS::content_blobs_count.add_value(content_blobs.len() as i64);

        fn upload_changeset(
            repo: Arc<BlobRepo>,
            scuba_logger: ScubaSampleBuilder,
            node: HgNodeHash,
            revlog_cs: RevlogChangeset,
            mut uploaded_changesets: UploadedChangesets,
            filelogs: &Filelogs,
            manifests: &Manifests,
            content_blobs: &ContentBlobs,
        ) -> BoxFuture<UploadedChangesets, Error> {
            let (p1, p2) = {
                (
                    get_parent(&repo, &uploaded_changesets, revlog_cs.p1),
                    get_parent(&repo, &uploaded_changesets, revlog_cs.p2),
                )
            };
            let NewBlobs {
                root_manifest,
                sub_entries,
                // XXX use these content blobs in the future
                content_blobs: _content_blobs,
            } = try_boxfuture!(NewBlobs::new(
                *revlog_cs.manifestid(),
                &manifests,
                &filelogs,
                &content_blobs,
            ));

            p1.join(p2)
                .with_context(move |_| format!("While fetching parents for Changeset {}", node))
                .from_err()
                .and_then(move |(p1, p2)| {
                    let cs_metadata = ChangesetMetadata {
                        user: String::from_utf8(revlog_cs.user().into())?,
                        time: revlog_cs.time().clone(),
                        extra: revlog_cs.extra().clone(),
                        comments: String::from_utf8(revlog_cs.comments().into())?,
                    };
                    let create_changeset = CreateChangeset {
                        expected_nodeid: Some(node),
                        expected_files: Some(Vec::from(revlog_cs.files())),
                        p1,
                        p2,
                        root_manifest,
                        sub_entries,
                        // XXX pass content blobs to CreateChangeset here
                        cs_metadata,
                        must_check_case_conflicts: true,
                    };
                    let scheduled_uploading = create_changeset.create(&repo, scuba_logger);

                    uploaded_changesets.insert(node, scheduled_uploading);
                    Ok(uploaded_changesets)
                })
                .boxify()
        }

        let repo = self.repo.clone();

        let changesets_hashes: Vec<_> = changesets.iter().map(|(hash, _)| *hash).collect();

        trace!(self.logger, "changesets: {:?}", changesets);
        trace!(self.logger, "filelogs: {:?}", filelogs.keys());
        trace!(self.logger, "manifests: {:?}", manifests.keys());
        trace!(self.logger, "content blobs: {:?}", content_blobs.keys());

        let scuba_logger = self.scuba_logger.clone();
        stream::iter_ok(changesets)
            .fold(
                HashMap::new(),
                move |uploaded_changesets, (node, revlog_cs)| {
                    upload_changeset(
                        repo.clone(),
                        scuba_logger.clone(),
                        node.clone(),
                        revlog_cs,
                        uploaded_changesets,
                        &filelogs,
                        &manifests,
                        &content_blobs,
                    )
                },
            )
            .and_then(|uploaded_changesets| {
                stream::futures_unordered(
                    uploaded_changesets
                        .into_iter()
                        .map(|(_, cs)| cs.get_completed_changeset()),
                ).map_err(Error::from)
                    .for_each(|_| Ok(()))
            })
            .chain_err(ErrorKind::WhileUploadingData(changesets_hashes))
            .from_err()
            .boxify()
    }

    /// Ensures that the next item in stream is None
    fn ensure_stream_finished(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
    ) -> BoxFuture<(), Error> {
        next_item(bundle2)
            .and_then(|(none, _)| {
                ensure_msg!(none.is_none(), "Expected end of Bundle2");
                Ok(())
            })
            .boxify()
    }

    /// Takes a changegroup id and prepares a Bytes response containing Bundle2 with reply to
    /// changegroup part saying that the push was successful
    fn prepare_push_response(
        &self,
        changegroup_id: Option<PartId>,
        bookmark_ids: Vec<PartId>,
    ) -> BoxFuture<Bytes, Error> {
        let writer = Cursor::new(Vec::new());
        let mut bundle = Bundle2EncodeBuilder::new(writer);
        // Mercurial currently hangs while trying to read compressed bundles over the wire:
        // https://bz.mercurial-scm.org/show_bug.cgi?id=5646
        // TODO: possibly enable compression support once this is fixed.
        bundle.set_compressor_type(None);
        if let Some(changegroup_id) = changegroup_id {
            bundle.add_part(try_boxfuture!(parts::replychangegroup_part(
                parts::ChangegroupApplyResult::Success { heads_num_diff: 0 },
                changegroup_id,
            )));
        }
        for part_id in bookmark_ids {
            bundle.add_part(try_boxfuture!(parts::replypushkey_part(true, part_id)));
        }
        bundle
            .build()
            .map(|cursor| Bytes::from(cursor.into_inner()))
            .context("While preparing response")
            .from_err()
            .boxify()
    }

    fn prepare_pushrebase_response(
        &self,
        commonheads: CommonHeads,
        pushrebased_rev: ChangesetId,
        onto: Bookmark,
        lca_hint: Arc<LeastCommonAncestorsHint + Send + Sync>,
    ) -> impl Future<Item = Bytes, Error = Error> {
        // Send to the client both pushrebased commit and current "onto" bookmark. Normally they
        // should be the same, however they might be different if bookmark
        // suddenly moved before current pushrebase finished.
        let repo: BlobRepo = (*self.repo).clone();
        let common = commonheads.heads;
        let maybe_onto_head = repo.get_bookmark(&onto);

        let pushrebased_rev = repo.get_hg_from_bonsai_changeset(pushrebased_rev);

        let mut scuba_logger = self.scuba_logger.clone();
        maybe_onto_head
            .join(pushrebased_rev)
            .and_then(move |(maybe_onto_head, pushrebased_rev)| {
                let mut heads = vec![];
                if let Some(onto_head) = maybe_onto_head {
                    heads.push(onto_head);
                }
                heads.push(pushrebased_rev);
                getbundle_response::create_getbundle_response(repo, common, heads, lca_hint)
            })
            .and_then(|cg_part_builder| {
                let compression = None;
                create_bundle_stream(vec![cg_part_builder], compression)
                    .collect()
                    .map(|chunks| {
                        let mut total_capacity = 0;
                        for c in chunks.iter() {
                            total_capacity += c.len();
                        }

                        // TODO(stash): make push and pushrebase response streamable - T34090105
                        let mut res = BytesMut::with_capacity(total_capacity);
                        for c in chunks {
                            res.extend_from_slice(&c);
                        }
                        res.freeze()
                    })
                    .context("While preparing response")
                    .from_err()
            })
            .timed({
                move |stats, result| {
                    if result.is_ok() {
                        scuba_logger
                            .add_future_stats(&stats)
                            .log_with_msg("Pushrebase: prepared the response", None);
                    }
                    Ok(())
                }
            })
    }

    /// A method that can use any of the above maybe_resolve_* methods to return
    /// a Vec of (potentailly multiple) Part rather than an Option of Part.
    /// The original use case is to parse multiple pushkey Parts since bundle2 gets
    /// one pushkey part per bookmark.
    fn resolve_multiple_parts<T, Func>(
        &self,
        bundle2: BoxStream<Bundle2Item, Error>,
        mut maybe_resolve: Func,
    ) -> BoxFuture<(Vec<T>, BoxStream<Bundle2Item, Error>), Error>
    where
        Func: FnMut(&Self, BoxStream<Bundle2Item, Error>)
            -> BoxFuture<(Option<T>, BoxStream<Bundle2Item, Error>), Error>
            + Send
            + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        future::loop_fn((Vec::new(), bundle2), move |(mut result, bundle2)| {
            maybe_resolve(&this, bundle2).map(move |(maybe_element, bundle2)| match maybe_element {
                None => future::Loop::Break((result, bundle2)),
                Some(element) => {
                    result.push(element);
                    future::Loop::Continue((result, bundle2))
                }
            })
        }).boxify()
    }

    fn pushrebase(
        &self,
        changesets: Changesets,
        bookmark_pushes: Vec<BookmarkPush>,
        onto_bookmark: &Bookmark,
    ) -> impl Future<Item = ChangesetId, Error = Error> {
        let changesets: Vec<_> = changesets
            .into_iter()
            .map(|(node, _)| HgChangesetId::new(node))
            .collect();

        let incorrect_bookmark_pushes: Vec<_> = bookmark_pushes
            .iter()
            .filter(|bp| &bp.name != onto_bookmark)
            .collect();

        if !incorrect_bookmark_pushes.is_empty() {
            try_boxfuture!(Err(err_msg(format!(
                "allowed only pushes of {} bookmark: {:?}",
                onto_bookmark, bookmark_pushes
            ))))
        }

        pushrebase::do_pushrebase(
            self.repo.clone(),
            self.pushrebase.clone(),
            onto_bookmark.clone(),
            changesets,
        ).map_err(|err| err_msg(format!("pushrebase failed {:?}", err)))
            .timed({
                let mut scuba_logger = self.scuba_logger.clone();
                move |stats, result| {
                    if let Ok(res) = result {
                        scuba_logger
                            .add_future_stats(&stats)
                            .add("pushrebase_retry_num", res.retry_num)
                            .log_with_msg("Pushrebase finished", None);
                    }
                    Ok(())
                }
            })
            .map(|res| res.head)
            .boxify()
    }

    fn run_hooks(
        &self,
        changesets: Changesets,
        pushvars: Option<HashMap<String, Bytes>>,
        onto_bookmark: &Bookmark,
    ) -> BoxFuture<(), RunHooksError> {
        let mut futs = stream::FuturesUnordered::new();
        for (cs_id, _) in changesets {
            let hg_cs_id = HgChangesetId::new(cs_id.clone());
            futs.push(
                self.hook_manager
                    .run_changeset_hooks_for_bookmark(
                        hg_cs_id.clone(),
                        onto_bookmark,
                        pushvars.clone(),
                    )
                    .join(self.hook_manager.run_file_hooks_for_bookmark(
                        hg_cs_id,
                        onto_bookmark,
                        pushvars.clone(),
                    )),
            )
        }
        futs.collect()
            .from_err()
            .and_then(|res| {
                let (cs_hook_results, file_hook_results): (Vec<_>, Vec<_>) =
                    res.into_iter().unzip();
                let cs_hook_failures: Vec<
                    (ChangesetHookExecutionID, HookExecution),
                > = cs_hook_results
                    .into_iter()
                    .flatten()
                    .filter(|(_, exec)| match exec {
                        HookExecution::Accepted => false,
                        HookExecution::Rejected(_) => true,
                    })
                    .collect();
                let file_hook_failures: Vec<(FileHookExecutionID, HookExecution)> =
                    file_hook_results
                        .into_iter()
                        .flatten()
                        .filter(|(_, exec)| match exec {
                            HookExecution::Accepted => false,
                            HookExecution::Rejected(_) => true,
                        })
                        .collect();
                if cs_hook_failures.len() > 0 || file_hook_failures.len() > 0 {
                    Err(RunHooksError::Failures((
                        cs_hook_failures,
                        file_hook_failures,
                    )))
                } else {
                    Ok(())
                }
            })
            .boxify()
    }
}

#[derive(Debug)]
pub enum RunHooksError {
    Failures(
        (
            Vec<(ChangesetHookExecutionID, HookExecution)>,
            Vec<(FileHookExecutionID, HookExecution)>,
        ),
    ),
    Error(Error),
}

impl From<Error> for RunHooksError {
    fn from(error: Error) -> Self {
        RunHooksError::Error(error)
    }
}

fn add_bookmark_to_transaction(
    txn: &mut Box<Transaction>,
    bookmark_push: BonsaiBookmarkPush,
) -> Result<()> {
    match (bookmark_push.new, bookmark_push.old) {
        (Some(new), Some(old)) => txn.update(&bookmark_push.name, &new, &old),
        (Some(new), None) => txn.create(&bookmark_push.name, &new),
        (None, Some(old)) => txn.delete(&bookmark_push.name, &old),
        _ => Ok(()),
    }
}

/// Retrieves the parent from uploaded changesets, if it is missing then fetches it from BlobRepo
fn get_parent(
    repo: &BlobRepo,
    map: &UploadedChangesets,
    p: Option<HgNodeHash>,
) -> impl Future<Item = Option<ChangesetHandle>, Error = Error> {
    let res = match p {
        None => None,
        Some(p) => match map.get(&p) {
            None => Some(ChangesetHandle::ready_cs_handle(
                Arc::new(repo.clone()),
                HgChangesetId::new(p),
            )),
            Some(cs) => Some(cs.clone()),
        },
    };
    ok(res)
}

type HgBlobFuture = BoxFuture<(HgBlobEntry, RepoPath), Error>;
type HgBlobStream = BoxStream<(HgBlobEntry, RepoPath), Error>;

/// In order to generate the DAG of dependencies between Root Manifest and other Manifests and
/// Filelogs we need to walk that DAG.
/// This represents the manifests and file nodes introduced by a particular changeset.
struct NewBlobs {
    // root_manifest can be None f.e. when commit removes all the content of the repo
    root_manifest: BoxFuture<Option<(HgBlobEntry, RepoPath)>, Error>,
    // sub_entries has both submanifest and filenode entries.
    sub_entries: HgBlobStream,
    // This is returned as a Vec rather than a Stream so that the path and metadata are
    // available before the content blob is uploaded. This will allow creating and uploading
    // changeset blobs without being blocked on content blob uploading being complete.
    content_blobs: Vec<ContentBlobInfo>,
}

struct WalkHelperCounters {
    manifests_count: usize,
    filelogs_count: usize,
    content_blobs_count: usize,
}

impl AddAssign for WalkHelperCounters {
    fn add_assign(&mut self, other: WalkHelperCounters) {
        *self = Self {
            manifests_count: self.manifests_count + other.manifests_count,
            filelogs_count: self.filelogs_count + other.filelogs_count,
            content_blobs_count: self.content_blobs_count + other.content_blobs_count,
        };
    }
}

impl NewBlobs {
    fn new(
        manifest_root_id: HgManifestId,
        manifests: &Manifests,
        filelogs: &Filelogs,
        content_blobs: &ContentBlobs,
    ) -> Result<Self> {
        if manifest_root_id.into_nodehash() == NULL_HASH {
            // If manifest root id is NULL_HASH then there is no content in this changest
            return Ok(Self {
                root_manifest: ok(None).boxify(),
                sub_entries: stream::empty().boxify(),
                content_blobs: Vec::new(),
            });
        }

        let root_key = HgNodeKey {
            path: RepoPath::root(),
            hash: manifest_root_id.clone().into_nodehash(),
        };

        let &(ref manifest_content, ref p1, ref p2, ref manifest_root) = manifests
            .get(&root_key)
            .ok_or_else(|| format_err!("Missing root tree manifest"))?;

        let (entries, content_blobs, counters) = Self::walk_helper(
            &RepoPath::root(),
            &manifest_content,
            get_manifest_parent_content(manifests, RepoPath::root(), p1.clone()),
            get_manifest_parent_content(manifests, RepoPath::root(), p2.clone()),
            manifests,
            filelogs,
            content_blobs,
        )?;

        STATS::per_changeset_manifests_count.add_value(counters.manifests_count as i64);
        STATS::per_changeset_filelogs_count.add_value(counters.filelogs_count as i64);
        STATS::per_changeset_content_blobs_count.add_value(counters.content_blobs_count as i64);

        Ok(Self {
            root_manifest: manifest_root
                .clone()
                .map(|it| Some((*it).clone()))
                .from_err()
                .boxify(),
            sub_entries: stream::futures_unordered(entries)
                .with_context(move |_| {
                    format!(
                        "While walking dependencies of Root Manifest with id {:?}",
                        manifest_root_id
                    )
                })
                .from_err()
                .boxify(),
            content_blobs,
        })
    }

    fn walk_helper(
        path_taken: &RepoPath,
        manifest_content: &ManifestContent,
        p1: Option<&ManifestContent>,
        p2: Option<&ManifestContent>,
        manifests: &Manifests,
        filelogs: &Filelogs,
        content_blobs: &ContentBlobs,
    ) -> Result<(Vec<HgBlobFuture>, Vec<ContentBlobInfo>, WalkHelperCounters)> {
        if path_taken.len() > 4096 {
            bail_msg!(
                "Exceeded max manifest path during walking with path: {:?}",
                path_taken
            );
        }

        let mut entries: Vec<HgBlobFuture> = Vec::new();
        let mut cbinfos: Vec<ContentBlobInfo> = Vec::new();
        let mut counters = WalkHelperCounters {
            manifests_count: 0,
            filelogs_count: 0,
            content_blobs_count: 0,
        };

        for (name, details) in manifest_content.files.iter() {
            if is_entry_present_in_parent(p1, name, details)
                || is_entry_present_in_parent(p2, name, details)
            {
                // If one of the parents contains exactly the same version of entry then either that
                // file or manifest subtree is not new
                continue;
            }

            let nodehash = details.entryid().clone().into_nodehash();
            let next_path = MPath::join_opt(path_taken.mpath(), name);
            let next_path = match next_path {
                Some(path) => path,
                None => bail_msg!("internal error: joined root path with root manifest"),
            };

            if details.is_tree() {
                let key = HgNodeKey {
                    path: RepoPath::DirectoryPath(next_path),
                    hash: nodehash,
                };

                if let Some(&(ref manifest_content, ref p1, ref p2, ref blobfuture)) =
                    manifests.get(&key)
                {
                    counters.manifests_count += 1;
                    entries.push(
                        blobfuture
                            .clone()
                            .map(|it| (*it).clone())
                            .from_err()
                            .boxify(),
                    );
                    let (mut walked_entries, mut walked_cbinfos, sub_counters) =
                        Self::walk_helper(
                            &key.path,
                            manifest_content,
                            get_manifest_parent_content(manifests, key.path.clone(), p1.clone()),
                            get_manifest_parent_content(manifests, key.path.clone(), p2.clone()),
                            manifests,
                            filelogs,
                            content_blobs,
                        )?;
                    entries.append(&mut walked_entries);
                    cbinfos.append(&mut walked_cbinfos);
                    counters += sub_counters;
                }
            } else {
                let key = HgNodeKey {
                    path: RepoPath::FilePath(next_path),
                    hash: nodehash,
                };
                if let Some(blobfuture) = filelogs.get(&key) {
                    counters.filelogs_count += 1;
                    counters.content_blobs_count += 1;
                    entries.push(
                        blobfuture
                            .clone()
                            .map(|it| (*it).clone())
                            .from_err()
                            .boxify(),
                    );
                    match content_blobs.get(&key) {
                        Some(cbinfo) => cbinfos.push(cbinfo.clone()),
                        None => {
                            bail_msg!("internal error: content blob future missing for filenode")
                        }
                    }
                }
            }
        }

        Ok((entries, cbinfos, counters))
    }
}

fn get_manifest_parent_content(
    manifests: &Manifests,
    path: RepoPath,
    p: Option<HgNodeHash>,
) -> Option<&ManifestContent> {
    p.and_then(|p| manifests.get(&HgNodeKey { path, hash: p }))
        .map(|&(ref content, ..)| content)
}

fn is_entry_present_in_parent(
    p: Option<&ManifestContent>,
    name: &MPath,
    details: &Details,
) -> bool {
    match p.and_then(|p| p.files.get(name)) {
        None => false,
        Some(parent_details) => parent_details == details,
    }
}

fn get_ascii_param(params: &HashMap<String, Bytes>, param: &str) -> Result<AsciiString> {
    let val = params
        .get(param)
        .ok_or(format_err!("`{}` parameter is not set", param))?;
    AsciiString::from_ascii(val.to_vec())
        .map_err(|err| format_err!("`{}` parameter is not ascii: {}", param, err))
}

fn get_optional_changeset_param(
    params: &HashMap<String, Bytes>,
    param: &str,
) -> Result<Option<HgChangesetId>> {
    let val = get_ascii_param(params, param)?;

    if val.is_empty() {
        Ok(None)
    } else {
        Ok(Some(HgChangesetId::from_ascii_str(&val)?))
    }
}
