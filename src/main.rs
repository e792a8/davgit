mod cli;
mod handler;
mod git;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tracing::info;

use crate::cli::Args;
use crate::git::GitRepo;

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

    let git = Arc::new(git_repo);

    let addr: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .context("invalid bind address")?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind to address")?;

    info!("davgit listening on http://{}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        let git = git.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req: hyper::Request<Incoming>| {
                        let git = git.clone();
                        async move { Ok::<_, hyper::Error>(handler::handle_request(req, git).await) }
                    }),
                )
                .await
            {
                tracing::error!("connection error from {}: {}", peer, err);
            }
        });
    }
}
