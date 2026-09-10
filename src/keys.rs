//! Key material: an encrypted wallet in, signing keys out.
//!
//! The wallet is `hivecomb`'s: scrypt-derived key, AES-GCM per entry, and a
//! `PrivateKey` that refuses to render itself through `Debug` or `Display`. Nothing
//! here logs a key, and nothing here writes one anywhere but into that file.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{bail, Context, Result};
use hivecomb::keys::Role;
use hivecomb::wallet::Wallet;
use hivecomb::PrivateKey;
use zeroize::Zeroizing;

/// The keys one account has available. Posting is required; active is optional, and
/// its absence is what disables the features that spend tokens.
pub struct AccountKeys {
    pub posting: PrivateKey,
    pub active: Option<PrivateKey>,
}

pub struct KeyStore {
    wallet: Wallet,
}

impl KeyStore {
    /// Open and unlock. The passphrase comes from `passphrase_env` if that names a
    /// set variable, otherwise from a prompt -- which needs a terminal, so an
    /// unattended deployment must set the variable.
    pub fn open(path: &Path, passphrase_env: &str) -> Result<Self> {
        if !path.exists() {
            bail!(
                "no wallet at {} -- create one with `terracore-bot wallet init`",
                path.display()
            );
        }
        let mut wallet = Wallet::open(path)
            .with_context(|| format!("opening the wallet at {}", path.display()))?;

        let passphrase = read_passphrase(passphrase_env, "Wallet passphrase: ", false)?;
        wallet
            .unlock(&passphrase)
            .context("unlocking the wallet (wrong passphrase?)")?;
        Ok(Self { wallet })
    }

    /// The keys for one account, or an error naming what is missing. A posting key is
    /// the minimum: without it the account cannot do anything at all.
    pub fn for_account(&self, account: &str) -> Result<AccountKeys> {
        let posting = self.wallet.key_for_role(account, Role::Posting).map_err(|_| {
            anyhow::anyhow!(
                "no posting key for `{account}` in the wallet -- add one with \
                 `terracore-bot wallet import --account {account} --role posting`"
            )
        })?;
        // Absent is the normal case and not an error: it means "this account does not
        // do the things that spend tokens".
        let active = self.wallet.key_for_role(account, Role::Active).ok();
        Ok(AccountKeys { posting, active })
    }
}

/// Create a new wallet file.
pub fn init(path: &Path, passphrase_env: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let passphrase = read_passphrase(passphrase_env, "New wallet passphrase: ", true)?;
    Wallet::create(path, &passphrase)
        .with_context(|| format!("creating a wallet at {}", path.display()))?;
    restrict_permissions(path)?;
    Ok(())
}

/// Add one WIF to the wallet, tagged with the account and role it belongs to.
///
/// The WIF is read from the terminal without echo, or from stdin when piped, so it
/// never has to appear in a shell history or a process listing.
pub fn import(path: &Path, passphrase_env: &str, account: &str, role: Role) -> Result<String> {
    let mut wallet =
        Wallet::open(path).with_context(|| format!("opening the wallet at {}", path.display()))?;
    let passphrase = read_passphrase(passphrase_env, "Wallet passphrase: ", false)?;
    wallet.unlock(&passphrase).context("unlocking the wallet")?;

    // Wiped on drop. hivecomb's `PrivateKey` zeroizes its own copy; this is the
    // copy that would otherwise be left behind in a freed allocation.
    let wif = Zeroizing::new(if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{role_str} WIF for @{account}: ", role_str = role.as_str()))
            .context("reading the key")?
    } else {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .context("reading the key from stdin")?;
        line
    });

    let key = PrivateKey::from_wif(wif.trim()).context("that is not a valid WIF private key")?;

    // The wallet is keyed by public key, so re-adding a key that is already held
    // replaces its account/role tag rather than storing a second entry. Silently
    // losing the earlier tag is how an account ends up with an active key and no
    // posting key, so say it out loud.
    let already = wallet.index().into_iter().find(|(existing_account, roles)| {
        (existing_account != account || !roles.iter().any(|r| r == role.as_str()))
            && wallet
                .key_for_role(existing_account, Role::Posting)
                .or_else(|_| wallet.key_for_role(existing_account, Role::Active))
                .map(|k| k.public_key() == key.public_key())
                .unwrap_or(false)
    });
    if let Some((existing_account, roles)) = already {
        eprintln!(
            "warning: this key is already stored for @{existing_account} ({}); that tag \
             is being replaced, because a wallet holds one entry per key.",
            roles.join(", ")
        );
    }

    let public = wallet
        .add_key(&key, Some(account), Some(role))
        .context("storing the key")?;
    restrict_permissions(path)?;
    Ok(public.to_prefixed("STM"))
}

pub fn remove(path: &Path, passphrase_env: &str, public_key: &str) -> Result<bool> {
    let mut wallet =
        Wallet::open(path).with_context(|| format!("opening the wallet at {}", path.display()))?;
    let passphrase = read_passphrase(passphrase_env, "Wallet passphrase: ", false)?;
    wallet.unlock(&passphrase).context("unlocking the wallet")?;
    let public = hivecomb::PublicKey::from_prefixed(public_key, "STM")
        .context("that is not a valid STM public key")?;
    wallet.remove_key(&public).context("removing the key")
}

pub fn list(path: &Path) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    // Deliberately no unlock: which accounts and roles are present is metadata, and
    // answering "do I have an active key for bob" should not require a passphrase.
    let wallet =
        Wallet::open(path).with_context(|| format!("opening the wallet at {}", path.display()))?;
    Ok(wallet.index())
}

fn read_passphrase(env_var: &str, prompt: &str, confirm: bool) -> Result<Zeroizing<String>> {
    if !env_var.is_empty() {
        if let Ok(value) = std::env::var(env_var) {
            if !value.is_empty() {
                return Ok(Zeroizing::new(value));
            }
        }
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "no terminal to prompt on -- set ${env_var} to the wallet passphrase for \
             unattended runs"
        );
    }
    let passphrase = Zeroizing::new(
        rpassword::prompt_password(prompt).context("reading the passphrase")?,
    );
    if confirm {
        let again = Zeroizing::new(
            rpassword::prompt_password("Repeat: ").context("reading the passphrase")?,
        );
        if *again != *passphrase {
            bail!("the two passphrases do not match");
        }
    }
    if passphrase.is_empty() {
        bail!("an empty passphrase encrypts nothing");
    }
    Ok(passphrase)
}

/// Owner-only. The contents are encrypted, but a key store other users can read is
/// still a key store other users can copy and attack offline.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("reading permissions of {}", path.display()))?
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("restricting permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
