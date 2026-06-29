//! hushai-eval CLI. The hermetic contract: a stable JSON verdict on stdout + an exit code
//! (0 pass/improved, 1 regression/floor-breach, 2 inconclusive/infra).

use clap::{Parser, Subcommand};
use hushai_eval::RunOpts;

#[derive(Parser)]
#[command(name = "hushai-eval", about = "Hushai end-to-end regression harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Reset → inject → process → score the fixture suite and emit a verdict.
    Run {
        /// "fast" (inner-loop, only tier=fast fixtures) or "full" (everything).
        #[arg(long, default_value = "full")]
        tier: String,
        /// Score only this case_id.
        #[arg(long)]
        case: Option<String>,
        /// Which split(s) to run: "train", "holdout", or "all".
        #[arg(long, default_value = "train")]
        fixtures: String,
        /// Write the current metric vector as the new baseline (only on a passing case unless --force).
        #[arg(long = "update-baseline")]
        update_baseline: bool,
        /// Allow --update-baseline to overwrite even on a failing case.
        #[arg(long)]
        force: bool,
        /// Emit the full SuiteResult as JSON instead of the human report.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hushai_eval=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Run { tier, case, fixtures, update_baseline, force, json } => {
            let splits = match fixtures.as_str() {
                "all" => vec!["train".to_string(), "holdout".to_string()],
                "holdout" => vec!["holdout".to_string()],
                _ => vec!["train".to_string()],
            };
            let opts = RunOpts { tier, case, splits, update_baseline, force, json };
            match hushai_eval::run(opts).await {
                Ok(suite) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&suite).unwrap());
                    } else {
                        println!("{}", suite.human_report());
                    }
                    suite.exit_code
                }
                Err(e) => {
                    eprintln!("hushai-eval error: {e:#}");
                    2
                }
            }
        }
    };
    std::process::exit(code);
}
