mod data;

use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use clap::{Args, Parser, Subcommand};
use data::{PackageInfo, Registry};
use eyre::{Context, Result};
use rippkgs::Package;
use rusqlite::OpenFlags;

#[derive(Debug, Parser)]
#[clap(about = "Generate an index for use with the rippkgs cli")]
struct Opts {
    #[clap(subcommand)]
    cmd: Subcmd,
}

#[derive(Debug, Subcommand)]
enum Subcmd {
    /// Generate an index from a registry JSON file
    Registry(ImportRegistry),
    /// Generate an index from a nixpkgs expression
    Nixpkgs(IndexNixpkgs),
}

#[derive(Debug, Args)]
struct ImportRegistry {
    /// The registry to import. This should be generated by calling `genRegistry` from the flake
    /// library.
    registry: PathBuf,

    /// The location to write the saved index to.
    #[clap(short, long, default_value = "rippkgs-index.sqlite")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct IndexNixpkgs {
    /// Optional location to save the generated registry to.
    #[clap(short = 'r', long)]
    save_registry: Option<PathBuf>,

    /// Optional expression to use for the `config` argument to `import <nixpkgs>`.
    #[clap(short = 'a', long, default_value = "{}")]
    nixpkgs_arg: String,

    /// The location of Nixpkgs on-disk to index. If omitted, will import `<nixpkgs>` without
    /// passing the `-I` flag to nix.
    nixpkgs: Option<PathBuf>,

    /// The location to write the saved index to.
    #[clap(short, long, default_value = "rippkgs-index.sqlite")]
    output: PathBuf,
}

fn main() -> Result<()> {
    let opts = Opts::parse();

    let output = match &opts.cmd {
        Subcmd::Registry(opts) => opts.output.as_path(),
        Subcmd::Nixpkgs(opts) => opts.output.as_path(),
    };

    match std::fs::remove_file(output) {
        Ok(()) => (),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
        Err(err) => Err(err).context("removing previous index db")?,
    }

    let registry = match &opts.cmd {
        Subcmd::Registry(opts) => import_registry(opts).context("importing registry")?,
        Subcmd::Nixpkgs(opts) => index_nixpkgs(opts).context("indexing nixpkgs")?,
    };

    write_index(output, registry).context("writing index")?;

    Ok(())
}

fn write_index(index: &Path, registry: Registry) -> Result<()> {
    let mut conn = rusqlite::Connection::open_with_flags(
        index,
        OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("connecting to index database")?;

    conn.execute(Package::create_table(), [])
        .context("creating table in database")?;

    let start = Instant::now();
    let tx = conn.transaction().context("starting transaction")?;

    {
        let mut create_row_query = tx
            .prepare(
                r#"
    INSERT INTO packages (attribute, name, version, storePath, description, long_description)
    VALUES (?, ?, ?, ?, ?, ?)
                "#,
            )
            .context("preparing INSERT query")?;

        registry
            .into_iter()
            .map(|(attr, info)| info.into_rippkgs_package(attr))
            .try_for_each(
                |Package {
                     attribute,
                     name,
                     version,
                     store_path,
                     description,
                     long_description,
                     score: _score, // score not included in the database
                 }| {
                    create_row_query
                        .execute(rusqlite::params![
                            attribute,
                            name,
                            version,
                            store_path,
                            description,
                            long_description
                        ])
                        .context("inserting package into database")
                        .map(|_| ())
                },
            )?;
    }

    tx.commit().context("committing database")?;

    println!(
        "wrote index in {:.4} seconds",
        start.elapsed().as_secs_f64()
    );

    Ok(())
}

fn index_nixpkgs(
    IndexNixpkgs {
        save_registry,
        nixpkgs_arg,
        nixpkgs,
        ..
    }: &IndexNixpkgs,
) -> Result<Registry> {
    let apply_arg = format!(
        r#"
genRegistry:

let pkgs = import <nixpkgs> {nixpkgs_arg};
    genRegistry' = genRegistry pkgs;
in genRegistry' pkgs
        "#,
    );

    let mut args = vec![
        "eval",
        "--impure",
        "--json",
        "--expr",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/lib/genRegistry.nix")),
        "--apply",
        apply_arg.as_str(),
    ];

    let mut nixpkgs_include_arg = String::new();
    if let Some(nixpkgs) = nixpkgs.as_ref() {
        nixpkgs_include_arg.push_str(format!("nixpkgs={}", nixpkgs.display()).as_str());
        args.push("-I");
        args.push(nixpkgs_include_arg.as_str());
    }

    let start = Instant::now();

    let output = Command::new("nix")
        .args(args.iter())
        .output()
        .with_context(|| format!("getting nixpkgs packages from {}", "nixpkgs"))?;

    println!(
        "evaluated registry in {:.4} seconds",
        start.elapsed().as_secs_f64()
    );

    if !output.status.success() {
        panic!(
            "`nix eval` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    if let Some(registry) = save_registry {
        File::options()
            .write(true)
            .truncate(true)
            .create(true)
            .open(registry)
            .context("opening registry file")?
            .write(&output.stdout)
            .context("writing registry file")?;
    }

    let start = Instant::now();
    let res = serde_json::from_slice(&output.stdout).context("reading registry JSON");
    println!(
        "parsed registry in {:.4} seconds",
        start.elapsed().as_secs_f64()
    );

    res
}

fn import_registry(ImportRegistry { registry, .. }: &ImportRegistry) -> Result<Registry> {
    let f = File::options()
        .read(true)
        .open(registry)
        .context("opening registry file")?;

    let start = Instant::now();
    let res = serde_json::from_reader::<_, HashMap<String, PackageInfo>>(f)
        .context("reading registry JSON")?;

    println!(
        "parsed registry in {:.4} seconds",
        start.elapsed().as_secs_f64()
    );

    Ok(res)
}
