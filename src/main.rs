mod cache;
mod cli;
mod commands;
mod fix_hash;
mod full_eval;
mod full_eval_types;
mod git;
mod github;
mod nix;
mod simple;
mod summary;
mod types;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info};

use cache::init_save_location;
use cli::{Args, Commands};
use fix_hash::process_fix_hash;
use full_eval::process_pr_full_eval;
use github::get_github_token;
use simple::process_pr;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Initialize the global save location
    init_save_location(args.save_location);

    match args.command {
        Commands::Verify(mut verify_args) => {
            // Try to find token if not provided
            if verify_args.token.is_none() {
                verify_args.token = get_github_token().await;
            }

            let mut overall_success = true;
            for pr in &verify_args.prs {
                info!("==========================================");
                info!("Processing PR #{}", pr);
                info!("==========================================");

                let result = if verify_args.full_eval {
                    // Full evaluation mode: detect all changed packages
                    process_pr_full_eval(
                        *pr,
                        verify_args.token.as_ref(),
                        &verify_args.nixpkgs,
                        &verify_args.system,
                        verify_args.eval_workers,
                        verify_args.dry_run,
                        verify_args.gist,
                        verify_args.build_jobs,
                        verify_args.resume,
                        verify_args.full_rebuild,
                    )
                    .await
                } else {
                    // Default mode: parse attr from PR title
                    process_pr(
                        *pr,
                        verify_args.attr.as_ref(),
                        verify_args.token.as_ref(),
                        &verify_args.nixpkgs,
                        &verify_args.system,
                        verify_args.dry_run,
                        verify_args.full_rebuild,
                    )
                    .await
                };

                match result {
                    Ok(success) => {
                        if !success {
                            overall_success = false;
                            error!("Verification failed for PR #{}", pr);
                        } else {
                            info!("Verification passed for PR #{}", pr);
                        }
                    }
                    Err(e) => {
                        overall_success = false;
                        error!("Error processing PR #{}: {}", pr, e);
                    }
                }
                info!("------------------------------------------\n");
            }

            if overall_success {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }

        Commands::FixHash(fix_args) => {
            match process_fix_hash(
                &fix_args.nixpkgs,
                &fix_args.attribute,
                &fix_args.intermediate,
                &fix_args.system,
                fix_args.dont_diff,
                fix_args.log_location.as_ref(),
                &fix_args.nixpkgs_ref,
                fix_args.origin.as_ref(),
                fix_args.branch.as_ref(),
                &fix_args.log_base_url,
                fix_args.no_pr_text,
            )
            .await
            {
                Ok(success) => {
                    if success {
                        info!("Hash fix completed successfully");
                        Ok(())
                    } else {
                        error!("Hash fix failed");
                        std::process::exit(1)
                    }
                }
                Err(e) => {
                    error!("Error fixing hash: {}", e);
                    std::process::exit(1)
                }
            }
        }
    }
}
