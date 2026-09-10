//! terracore-bot -- automation for the Terracore Hive game.
//!
//! Signing is offline via `hivecomb`; the only network calls are reading the game's
//! API and handing a finished transaction to a Hive node.

mod actions;
mod api;
mod config;
mod hive;
mod keys;
mod runner;
mod targeting;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use hivecomb::keys::Role;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::runner::Bot;

#[derive(Parser)]
#[command(name = "terracore-bot", version, about, long_about = None)]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,

    /// Decide and sign everything, broadcast nothing.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Repeat for more detail: -v for debug, -vv for trace.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run cycles forever.
    Run,
    /// Run one cycle and exit.
    Once,
    /// Show what the bot sees for each account. Reads only.
    Status,
    /// Show who the bot would attack right now, and why it refused the rest.
    Targets {
        /// Limit to one account.
        account: Option<String>,
        /// How many ranked targets to print.
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Parse the config and print the settings each account ends up with.
    Check,
    /// Manage the encrypted key store.
    #[command(subcommand)]
    Wallet(WalletCommand),
}

#[derive(Subcommand)]
enum WalletCommand {
    /// Create an empty encrypted wallet.
    Init,
    /// Add one private key, read without echo.
    Import(ImportArgs),
    /// List the accounts and roles held. Does not need the passphrase.
    List,
    /// Remove one key by its public key.
    Remove {
        /// The STM... public key to drop.
        public_key: String,
    },
}

#[derive(Args)]
struct ImportArgs {
    /// The Hive account the key belongs to.
    #[arg(short, long)]
    account: String,
    /// Which authority this key is. Posting is enough for attacking, claiming and
    /// missions; active is only needed for boss fights and upgrades.
    #[arg(short, long, value_enum, default_value_t = KeyRole::Posting)]
    role: KeyRole,
}

#[derive(Copy, Clone, ValueEnum)]
enum KeyRole {
    Posting,
    Active,
}

impl From<KeyRole> for Role {
    fn from(role: KeyRole) -> Self {
        match role {
            KeyRole::Posting => Role::Posting,
            KeyRole::Active => Role::Active,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match &cli.command {
        Command::Wallet(command) => wallet(command, &cli),
        Command::Check => check(&cli),
        // No wallet, no passphrase, no broadcasts: safe to run anywhere.
        Command::Status => runner::status(&load_config(&cli)?),
        Command::Targets { account, limit } => {
            runner::targets(&load_config(&cli)?, account.as_deref(), *limit)
        }
        Command::Once => {
            let config = load_config(&cli)?;
            let mut bot = Bot::new(config, stop_flag()?)?;
            bot.preflight()?;
            bot.cycle()
        }
        Command::Run => {
            let config = load_config(&cli)?;
            let mut bot = Bot::new(config, stop_flag()?)?;
            bot.preflight()?;
            bot.run()
        }
    }
}

fn load_config(cli: &Cli) -> Result<Config> {
    let mut config = Config::load(&cli.config)?;
    if cli.dry_run {
        config.general.dry_run = true;
    }
    if config.general.dry_run {
        tracing::warn!("dry run: transactions are signed but never broadcast");
    }
    Ok(config)
}

/// Ctrl-C sets a flag rather than killing the process, so a stop lands between
/// actions instead of halfway through an attack run.
fn stop_flag() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let handler = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        if handler.swap(true, Ordering::Relaxed) {
            // A second Ctrl-C means the operator is not waiting any longer.
            std::process::exit(130);
        }
        eprintln!("\nstopping after the current action -- Ctrl-C again to quit now");
    })
    .context("installing the Ctrl-C handler")?;
    Ok(stop)
}

fn check(cli: &Cli) -> Result<()> {
    let config = Config::load(&cli.config)?;
    println!("config   {}", config.path.display());
    println!("wallet   {}", config.wallet_path().display());
    println!("nodes    {}", config.hive.nodes.join(", "));
    println!("api      {}", config.terracore.api);
    println!(
        "cycle    every {}s, {}s between accounts{}",
        config.general.cycle_interval_secs,
        config.general.account_delay_secs,
        if config.general.dry_run { ", DRY RUN" } else { "" }
    );
    println!();
    for account in &config.accounts {
        println!(
            "@{}{}",
            account.name,
            if account.enabled { "" } else { "  (disabled)" }
        );
        let s = &account.settings;
        println!("  attack   {:?}", s.attack);
        println!("  claim    {:?}", s.claim);
        println!("  quest    {:?}", s.quest);
        println!("  boss     {:?}", s.boss);
        println!("  upgrade  {:?}", s.upgrade);
        println!();
    }
    Ok(())
}

fn wallet(command: &WalletCommand, cli: &Cli) -> Result<()> {
    // The wallet lives where the config says, but wallet commands must work before
    // any account is configured -- so a missing config falls back to the default.
    let (path, passphrase_env) = match Config::load(&cli.config) {
        Ok(config) => (config.wallet_path(), config.wallet.passphrase_env),
        Err(_) => {
            let defaults = config::WalletConfig::default();
            (
                config::expand_tilde(&defaults.path),
                defaults.passphrase_env,
            )
        }
    };

    match command {
        WalletCommand::Init => {
            keys::init(&path, &passphrase_env)?;
            println!("created {}", path.display());
            println!("Import a posting key next:");
            println!("  terracore-bot wallet import --account <name> --role posting");
        }
        WalletCommand::Import(args) => {
            let public = keys::import(&path, &passphrase_env, &args.account, args.role.into())?;
            println!("stored {public} for @{} ({:?})", args.account, Role::from(args.role).as_str());
        }
        WalletCommand::List => {
            let index = keys::list(&path)?;
            if index.is_empty() {
                println!("no keys in {}", path.display());
            }
            for (account, roles) in index {
                println!("@{account}  {}", roles.join(", "));
            }
        }
        WalletCommand::Remove { public_key } => {
            if keys::remove(&path, &passphrase_env, public_key)? {
                println!("removed {public_key}");
            } else {
                println!("{public_key} was not in the wallet");
            }
        }
    }
    Ok(())
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "terracore_bot=info,warn",
        1 => "terracore_bot=debug,info",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
