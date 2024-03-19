use std::path::PathBuf;
use std::time::Instant;

use eyre::Context;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use rusqlite::functions::Context as FunctionContext;
use rusqlite::{functions::FunctionFlags, Connection};

use rippkgs::Package;

pub fn search(
    query_str: &str,
    db: &Connection,
    num_results: u32,
    filter_built: bool,
) -> eyre::Result<Vec<Package>> {
    db.create_scalar_function(
        "fuzzy_score",
        2,
        FunctionFlags::SQLITE_UTF8,
        scalar_fuzzy_score,
    )
    .context("unable to install `fuzzy_score` function")?;

    let mut query = db
        .prepare(
            r#"
SELECT *
FROM packages
ORDER BY fuzzy_score(name, ?1) DESC
            "#,
        )
        .context("unable to prepare search query")?;

    let start = Instant::now();

    let res = query
        .query_map(rusqlite::params![query_str], |r| Package::try_from(r))
        .map(|res| {
            res.filter(|package_res| {
                let Ok(package) = package_res else {
                    // carry on the error
                    return true;
                };

                let Some(store_path) = package.store_path.as_ref() else {
                    // only None when the package is stdenv (not installable) or part of
                    // bootstrapping (should use other attrs). We always filter these out because
                    // they're almost always irrelevant.
                    return false;
                };

                if !filter_built {
                    // we don't care about filtering out results based on presence of the store
                    // path.
                    return true;
                }

                PathBuf::from("/nix/store/").join(store_path).exists()
            })
            .take(num_results as _)
            .collect::<Result<Vec<_>, _>>()
            .context("error parsing results")
        })
        .context("unable to execute query")?;

    let elapsed = start.elapsed();
    eprintln!("got results in {} ms", elapsed.as_millis());

    res
}

fn scalar_fuzzy_score(ctx: &FunctionContext) -> rusqlite::Result<i64> {
    lazy_static::lazy_static! {
      static ref MATCHER: SkimMatcherV2 = SkimMatcherV2::default();
    }

    let choice = ctx.get::<String>(0)?;
    let pattern = ctx.get::<String>(1)?;

    Ok(MATCHER.fuzzy_match(&choice, &pattern).unwrap_or(0))
}
