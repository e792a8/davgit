mod cli;
mod fs;
mod git_bridge;

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use dav_server::DavHandler;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tracing::info;

use crate::cli::Args;
use crate::fs::GitDavFs;
use crate::git_bridge::GitRepo;

fn resolve_author(args: &Args) -> (String, String) {
    let name = args.author_name.clone().unwrap_or_else(|| {
        std::env::var("GIT_AUTHOR_NAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "davgit".to_string())
    });
    let email = args.author_email.clone().unwrap_or_else(|| {
        std::env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| format!("{}@localhost", name))
    });
    (name, email)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let (author_name, author_email) = resolve_author(&args);

    info!(
        "connecting to remote: {} (branch: {})",
        args.remote_url, args.branch
    );

    let git_repo = GitRepo::init_and_fetch(
        &args.remote_url,
        &args.branch,
        args.ssh_key.as_deref(),
        args.password.as_deref(),
        &author_name,
        &author_email,
    )
    .context("failed to initialize and fetch from remote")?;

    let tree = if let Some(tree_id) = git_repo.resolve_head_tree()? {
        info!("loading tree from remote...");
        git_repo.read_tree_to_memory(tree_id)?
    } else {
        info!("empty repository - starting with empty directory");
        std::collections::HashMap::new()
    };

    info!("loaded {} files from remote", tree.len());

    let fs = GitDavFs::new(tree, git_repo);
    let handler = DavHandler::builder()
        .filesystem(Box::new(fs))
        .build_handler();

    let addr: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .context("invalid bind address")?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind to address")?;

    info!("davgit listening on http://{}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req: hyper::Request<Incoming>| {
                        let h = handler.clone();
                        async move { Ok::<_, hyper::Error>(h.handle(req).await) }
                    }),
                )
                .await
            {
                tracing::error!("connection error from {}: {}", peer, err);
            }
        });
    }
}
