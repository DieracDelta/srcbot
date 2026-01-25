mod cache;
mod check_all;
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
use tracing::{error, info, warn};

use cache::{init_save_location, save_command_file};
use check_all::process_check_all;
use cli::{Args, Commands};
use fix_hash::process_fix_hash;
use full_eval::process_pr_full_eval;
use github::get_github_token;
use simple::process_pr;
use types::RemoteBuilderConfig;

#[tokio::main]
async fn main() -> Result<()> {
    // Capture the CLI command for logging/summary before parsing
    let cli_command = std::env::args().collect::<Vec<_>>().join(" ");

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

            // Validate and build remote builder config
            let remote_config = match (&verify_args.remote_builder, &verify_args.remote_system) {
                (Some(builder), Some(system)) => {
                    info!(
                        "Remote builder configured: {} for {}",
                        builder, system
                    );
                    Some(RemoteBuilderConfig {
                        ssh_target: builder.clone(),
                        system: system.clone(),
                        max_jobs: verify_args.remote_build_jobs,
                        gc_threshold: verify_args.remote_gc_threshold.clone(),
                        gc_keep_days: verify_args.remote_gc_keep_days,
                    })
                }
                (None, None) => None,
                _ => {
                    error!("--remote-builder and --remote-system must both be specified together");
                    std::process::exit(1);
                }
            };

            // Log the remote builder info if configured
            if let Some(ref config) = remote_config {
                info!("  Builders arg: {}", config.to_builders_arg());
                if let Some(ref threshold) = config.gc_threshold {
                    info!("  GC threshold: {}", threshold);
                }
                if let Some(days) = config.gc_keep_days {
                    info!("  GC keep days: {}", days);
                }
                // Note: Full multi-arch support is not yet implemented
                warn!("Note: Multi-arch build orchestration is in development. Remote builder config is saved but not fully utilized yet.");
            }

            // Log the CLI command
            info!("Command: {}", cli_command);

            let mut overall_success = true;
            for pr in &verify_args.prs {
                info!("==========================================");
                info!("Processing PR #{}", pr);
                info!("==========================================");

                // Save COMMAND.md to the log directory
                if let Err(e) = save_command_file(*pr, &cli_command) {
                    warn!("Failed to save command file: {}", e);
                }

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
                        verify_args.false_positive,
                        verify_args.base_commit.as_deref(),
                        verify_args.verify_full_drvs,
                        verify_args.nix_timeout,
                        verify_args.log_base_url.as_deref(),
                        remote_config.as_ref(),
                        &cli_command,
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
                        remote_config.as_ref(),
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
            // Build remote config if specified
            let remote_config = match (&fix_args.remote_builder, &fix_args.remote_system) {
                (Some(builder), Some(system)) => {
                    info!(
                        "Remote builder configured for fix-hash: {} for {}",
                        builder, system
                    );
                    Some(RemoteBuilderConfig {
                        ssh_target: builder.clone(),
                        system: system.clone(),
                        max_jobs: fix_args.remote_build_jobs,
                        gc_threshold: None,
                        gc_keep_days: None,
                    })
                }
                (None, None) => None,
                _ => {
                    error!("--remote-builder and --remote-system must both be specified together");
                    std::process::exit(1);
                }
            };

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
                fix_args.nix_timeout,
                remote_config.as_ref(),
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

        Commands::CheckAll(check_args) => {
            // Build remote config if specified
            let remote_config = match (&check_args.remote_builder, &check_args.remote_system) {
                (Some(builder), Some(system)) => {
                    info!(
                        "Remote builder configured for check-all: {} for {}",
                        builder, system
                    );
                    Some(RemoteBuilderConfig {
                        ssh_target: builder.clone(),
                        system: system.clone(),
                        max_jobs: check_args.remote_build_jobs,
                        gc_threshold: check_args.remote_gc_threshold.clone(),
                        gc_keep_days: check_args.remote_gc_keep_days,
                    })
                }
                (None, None) => None,
                _ => {
                    error!("--remote-builder and --remote-system must both be specified together");
                    std::process::exit(1);
                }
            };

            match process_check_all(&check_args, remote_config.as_ref()).await {
                Ok(success) => {
                    if success {
                        info!("All checks passed");
                        Ok(())
                    } else {
                        error!("Some checks failed");
                        std::process::exit(1)
                    }
                }
                Err(e) => {
                    error!("Error during check-all: {}", e);
                    std::process::exit(1)
                }
            }
        }
    }
}
