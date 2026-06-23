use clap::Parser;

#[derive(Parser, Debug, Clone)]
pub struct Args {
    #[arg(long, required = true)]
    pub remote_url: String,

    #[arg(long, default_value = "main")]
    pub branch: String,

    #[arg(long, default_value_t = 8080)]
    pub port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,

    #[arg(long)]
    pub author_name: Option<String>,

    #[arg(long)]
    pub author_email: Option<String>,

    #[arg(long)]
    pub ssh_key: Option<String>,

    #[arg(long)]
    pub password: Option<String>,
}
