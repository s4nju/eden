/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::cmdargs::{ADD_PUBLIC_PHASES, FETCH_PHASE, LIST_PUBLIC};
use anyhow::{bail, format_err, Error};
use clap::ArgMatches;
use cloned::cloned;
use fbinit::FacebookInit;
use futures::{
    compat::Future01CompatExt,
    future::{FutureExt as PreviewFutureExt, TryFutureExt},
};
use futures_ext::{try_boxfuture, FutureExt};
use futures_old::{stream, Future, IntoFuture, Stream};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use blobrepo::BlobRepo;
use cmdlib::args;
use context::CoreContext;
use mercurial_types::HgChangesetId;
use mononoke_types::ChangesetId;
use slog::{info, Logger};

use crate::error::SubcommandError;

pub async fn subcommand_phases<'a>(
    fb: FacebookInit,
    logger: Logger,
    matches: &'a ArgMatches<'_>,
    sub_m: &'a ArgMatches<'_>,
) -> Result<(), SubcommandError> {
    let repo = args::open_repo(fb, &logger, &matches);
    args::init_cachelib(fb, &matches, None);
    let ctx = CoreContext::new_with_logger(fb, logger.clone());

    match sub_m.subcommand() {
        (FETCH_PHASE, Some(sub_m)) => {
            let ty = sub_m
                .value_of("changeset-type")
                .map(|s| s)
                .unwrap_or("hg")
                .to_string();
            let hash = sub_m
                .value_of("hash")
                .map(|s| s.to_string())
                .ok_or(Error::msg("changeset hash is not specified"));

            subcommand_fetch_phase_impl(fb, repo, hash, ty)
                .boxed()
                .compat()
                .from_err()
                .boxify()
        }
        (ADD_PUBLIC_PHASES, Some(sub_m)) => {
            let path = String::from(sub_m.value_of("input-file").unwrap());
            let chunk_size = sub_m
                .value_of("chunk-size")
                .and_then(|chunk_size| chunk_size.parse::<usize>().ok())
                .unwrap_or(16384);

            repo.and_then(move |repo| add_public_phases(ctx, repo, logger, path, chunk_size))
                .from_err()
                .boxify()
        }
        (LIST_PUBLIC, Some(sub_m)) => {
            let ty = sub_m
                .value_of("changeset-type")
                .map(|s| s)
                .unwrap_or("hg")
                .to_string();

            subcommand_list_public_impl(ctx, ty, repo)
                .boxed()
                .compat()
                .from_err()
                .boxify()
        }
        _ => Err(SubcommandError::InvalidArgs).into_future().boxify(),
    }
    .compat()
    .await
}

fn add_public_phases(
    ctx: CoreContext,
    repo: BlobRepo,
    logger: Logger,
    path: impl AsRef<str>,
    chunk_size: usize,
) -> impl Future<Item = (), Error = Error> {
    let phases = repo.get_phases();
    let file = try_boxfuture!(File::open(path.as_ref()).map_err(Error::from));
    let hg_changesets = BufReader::new(file).lines().filter_map(|id_str| {
        id_str
            .map_err(Error::from)
            .and_then(|v| HgChangesetId::from_str(&v))
            .ok()
    });
    let entries_processed = Arc::new(AtomicUsize::new(0));
    info!(logger, "start processing hashes");
    stream::iter_ok(hg_changesets)
        .chunks(chunk_size)
        .and_then(move |chunk| {
            let count = chunk.len();
            repo.get_hg_bonsai_mapping(ctx.clone(), chunk)
                .and_then({
                    cloned!(ctx, phases);
                    move |changesets| {
                        phases
                            .get_sql_phases()
                            .add_public_raw(ctx, changesets.into_iter().map(|(_, cs)| cs).collect())
                    }
                })
                .and_then({
                    cloned!(entries_processed);
                    move |_| {
                        print!(
                            "\x1b[Khashes processed: {}\r",
                            entries_processed.fetch_add(count, Ordering::SeqCst) + count,
                        );
                        std::io::stdout().flush().expect("flush on stdout failed");
                        tokio_timer::sleep(Duration::from_secs(5)).map_err(Error::from)
                    }
                })
        })
        .for_each(|_| Ok(()))
        .boxify()
}

async fn subcommand_list_public_impl(
    ctx: CoreContext,
    ty: String,
    repo: impl Future<Item = BlobRepo, Error = Error>,
) -> Result<(), Error> {
    let repo = repo.compat().await?;
    let phases = repo.get_phases();
    let sql_phases = phases.get_sql_phases();

    let public = sql_phases.list_all_public(ctx.clone()).compat().await?;
    if ty == "bonsai" {
        for p in public {
            println!("{}", p);
        }
    } else {
        for chunk in public.chunks(1000) {
            let bonsais: Vec<_> = chunk.iter().cloned().collect();
            let hg_bonsais = repo
                .get_hg_bonsai_mapping(ctx.clone(), bonsais)
                .compat()
                .await?;
            let hg_css: Vec<HgChangesetId> = hg_bonsais
                .clone()
                .into_iter()
                .map(|(hg_cs_id, _)| hg_cs_id)
                .collect();

            for hg_cs in hg_css {
                println!("{}", hg_cs);
            }
        }
    }
    Ok(())
}

pub async fn subcommand_fetch_phase_impl<'a>(
    fb: FacebookInit,
    repo: impl Future<Item = BlobRepo, Error = Error>,
    hash: Result<String, Error>,
    ty: String,
) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo = repo.compat().await?;
    let hash = hash?;
    let phases = repo.get_phases();

    let bcs_id = if ty == "bonsai" {
        ChangesetId::from_str(&hash)?
    } else if ty == "hg" {
        let maybe_bonsai = repo
            .get_bonsai_from_hg(ctx.clone(), HgChangesetId::from_str(&hash)?)
            .compat()
            .await?;
        maybe_bonsai.ok_or(format_err!("bonsai not found for {}", hash))?
    } else {
        bail!("unknown hash type: {}", ty);
    };

    let public_phases = phases.get_public(ctx, vec![bcs_id], false).compat().await?;

    if public_phases.contains(&bcs_id) {
        println!("public");
    } else {
        println!("draft");
    }

    Ok(())
}
