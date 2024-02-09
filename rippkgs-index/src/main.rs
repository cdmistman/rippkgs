#![feature(assert_matches)]
#![feature(unix_sigpipe)]

mod data;

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, Write},
    path::PathBuf,
    process::Command,
};

use clap::Parser;
use data::PackageInfo;
use eyre::Context;
use sqlx::{Connection, Executor};

#[derive(Debug, Parser)]
struct Opts {
    /// The location to write the saved index to.
    #[arg(short, long)]
    output: PathBuf,

    /// The flake URI of the nixpkgs to index.
    ///
    /// If this is provided, then the registry will optionally be cached at `--registry`.
    ///
    /// If this is empty, `--registry` must be provided.
    #[arg(short, long)]
    nixpkgs: Option<String>,

    /// The file for the cached registry.
    ///
    /// If `--nixpkgs` is provided, then this will cache the registry at the given path.
    ///
    /// If `--nixpkgs` is empty, then this file will be used in lieu of evaluating nixpkgs.
    #[arg(short, long)]
    registry: Option<PathBuf>,

    /// The value to pass as the config parameter to nixpkgs.
    ///
    /// Only used if `--nixpkgs` is provided.
    #[arg(short = 'c', long)]
    nixpkgs_config: Option<String>,
}

#[unix_sigpipe = "inherit"]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    let opts = Opts::parse();

    let registry = get_registry(&opts)?;

    match std::fs::remove_file(opts.output.as_path()) {
        Ok(()) => (),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
        Err(err) => Err(err).context("unable to create index db")?,
    }

    std::fs::File::options()
        .write(true)
        .create(true)
        .open(dbg!(opts.output.as_path()))
        .unwrap();

    let conn =
        &mut sqlx::SqliteConnection::connect(format!("sqlite:{}", opts.output.display()).as_str())
            .await
            .context("unable to connect to index database")?;

    conn.execute(sqlx::query_file_unchecked!("../queries/init.sql"))
        .await
        .context("unable to initialize database")?;

    for (attr, info) in registry.into_iter() {
        let store_path = match info.outputs.get("out") {
            Some(out) => out.display().to_string(),
            None => continue,
        };

        let name = info.pname.as_ref().unwrap_or(&attr).as_str();
        let version = info.version.as_ref().unwrap().as_str();
        let description = info
            .meta
            .as_ref()
            .map(|meta| meta.description.clone())
            .flatten();
        let long_description = info
            .meta
            .as_ref()
            .map(|meta| meta.long_description.clone())
            .flatten();

        let create_row_query = sqlx::query_file_as!(
            rippkgs_db::Package,
            "../queries/add-package.sql",
            attr,
            store_path,
            name,
            version,
            description,
            None::<String>,
            long_description,
        );

        conn.execute(create_row_query)
            .await
            .context("could not insert package into database")?;
    }

    Ok(())
}

fn get_registry(
    Opts {
        nixpkgs,
        registry,
        nixpkgs_config,
        ..
    }: &Opts,
) -> eyre::Result<HashMap<String, PackageInfo>> {
    let registry_reader: Box<dyn io::Read> = if let Some(nixpkgs) = nixpkgs {
        let nixpkgs_var = format!("nixpkgs={}", nixpkgs);

        let mut args = vec![
            "--json",
            "-f",
            "<nixpkgs>",
            "-I",
            nixpkgs_var.as_str(),
            "-qa",
            "--meta",
            "--out-path",
        ];

        if let Some(config) = nixpkgs_config.as_ref() {
            args.push("--arg");
            args.push("config");
            args.push(config.as_str());
        }

        let output = Command::new("nix-env")
            .args(args.iter())
            .output()
            .expect("failed to get nixpkgs packages");

        if !output.status.success() {
            panic!(
                "nix-env failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        if let Some(registry) = registry {
            File::options()
                .write(true)
                .truncate(true)
                .create(true)
                .open(registry)
                .context("couldn't open registry file")?
                .write(&output.stdout)
                .context("couldn't write registry file")?;
        }

        Box::new(VecDeque::from(output.stdout))
    } else if let Some(registry) = registry {
        let f = File::options()
            .read(true)
            .open(registry)
            .context("couldn't open registry file")?;

        Box::new(f)
    } else {
        return Err(eyre::eyre!("expected nixpkgs location or cached registry"));
    };

    serde_json::from_reader(registry_reader).context("unable to read registry JSON")
}
