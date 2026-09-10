//! Logging in with Hive Keychain.
//!
//! The browser never sees a private key and the server never stores a password. The
//! whole handshake is:
//!
//! 1. the browser asks for a challenge for `@alice`,
//! 2. Keychain signs that exact string with alice's posting key,
//! 3. the server recovers the key from the signature and checks it against alice's
//!    posting authority **as the chain reports it**.
//!
//! Step 3 is the one that matters. Recovering a key from a signature proves only that
//! the signature is well formed -- a tampered signature simply recovers a *different*
//! key. It becomes a proof of identity only when the recovered key is checked against
//! an authority fetched from the chain, which is what happens here.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hivecomb::rpc::{NodeClient, UreqTransport};
use hivecomb::sign::{recover_message, Signature};
use rand::RngCore;

use crate::config::WebRole;

/// How long a challenge is good for. Long enough to find the Keychain popup, short
/// enough that a shoulder-surfed one is useless.
const CHALLENGE_TTL: Duration = Duration::from_secs(120);

/// Never more than this many challenges outstanding, so an unauthenticated endpoint
/// cannot be used to grow the process.
const MAX_CHALLENGES: usize = 256;

struct Challenge {
    account: String,
    expires: Instant,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub account: String,
    pub role: WebRole,
    pub expires_at: u64,
    expires: Instant,
}

pub struct Auth {
    challenges: Mutex<HashMap<String, Challenge>>,
    sessions: Mutex<HashMap<String, Session>>,
    session_ttl: Duration,
    client: NodeClient<UreqTransport>,
    access: BTreeMap<String, WebRole>,
}

impl Auth {
    pub fn new(
        nodes: Vec<String>,
        timeout: Duration,
        session_ttl: Duration,
        access: BTreeMap<String, WebRole>,
    ) -> Result<Self> {
        let client = NodeClient::new(UreqTransport, nodes)
            .context("building the Hive client used to verify logins")?
            .with_timeout(timeout);
        Ok(Self {
            challenges: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            session_ttl,
            client,
            access,
        })
    }

    fn role_of(&self, account: &str) -> Option<WebRole> {
        self.access
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(account))
            .map(|(_, role)| *role)
    }

    /// Issue the string Keychain will be asked to sign.
    ///
    /// An account nobody granted access to is refused here rather than after the
    /// signature: there is no point making someone approve a popup that was never
    /// going to be accepted, and it is not a secret who may log in to their own bot.
    pub fn challenge(&self, account: &str) -> Result<String> {
        let account = account.trim().to_ascii_lowercase();
        if account.is_empty() {
            bail!("no account given");
        }
        if self.role_of(&account).is_none() {
            bail!("@{account} is not listed in [web.access]");
        }

        // Readable on purpose: this is what Keychain shows the person approving it,
        // and "sign this random hex" teaches people to approve anything.
        let message = format!(
            "terracore-bot login as @{account} at {} [{}]",
            crate::state::epoch_secs(),
            random_hex(16),
        );

        let mut challenges = self.lock_challenges();
        challenges.retain(|_, c| c.expires > Instant::now());
        if challenges.len() >= MAX_CHALLENGES {
            bail!("too many logins in flight; try again in a minute");
        }
        challenges.insert(
            message.clone(),
            Challenge {
                account,
                expires: Instant::now() + CHALLENGE_TTL,
            },
        );
        Ok(message)
    }

    /// Consume the challenge and recover the key that signed it. No network.
    ///
    /// Split out from [`Self::login`] so the half that needs no node can be tested
    /// without one -- and so it is obvious that this half proves nothing on its own.
    fn recover_signer(&self, message: &str, signature_hex: &str) -> Result<(String, hivecomb::PublicKey)> {
        let challenge = {
            let mut challenges = self.lock_challenges();
            // Taken, not read: a challenge is single use, so a replay of the same
            // signature finds nothing.
            challenges.remove(message)
        };
        let Some(challenge) = challenge else {
            bail!("that login challenge is unknown or already used -- start again");
        };
        if challenge.expires <= Instant::now() {
            bail!("that login challenge expired -- start again");
        }

        let signature = Signature::from_hex(signature_hex.trim())
            .context("Keychain returned something that is not a signature")?;
        let key = recover_message(message.as_bytes(), &signature)
            .context("that signature does not verify")?;
        Ok((challenge.account, key))
    }

    /// Verify a signed challenge and open a session. Returns the session token.
    pub fn login(&self, message: &str, signature_hex: &str) -> Result<(String, Session)> {
        let (account_name, key) = self.recover_signer(message, signature_hex)?;

        // The step that turns a well-formed signature into a proof of identity.
        let account = self
            .client
            .find_account(&account_name)
            .with_context(|| format!("looking up @{} on Hive", account_name))?
            .ok_or_else(|| anyhow::anyhow!("@{} does not exist on Hive", account_name))?;

        let check = account.posting.check(&[key]);
        if !check.satisfied {
            if !check.unresolved_accounts.is_empty() {
                // Being explicit beats a bare "denied": the person may well hold the
                // authority, just not through a key this check can see.
                bail!(
                    "that key does not meet @{}'s posting threshold on its own \
                     (weight {} of {}), and the rest is delegated to another account, \
                     which a login cannot follow. Sign with a key held directly in the \
                     posting authority.",
                    account_name,
                    check.weight,
                    check.threshold
                );
            }
            bail!(
                "that key is not in @{}'s posting authority",
                account_name
            );
        }

        let role = self
            .role_of(&account_name)
            .ok_or_else(|| anyhow::anyhow!("@{} is no longer permitted to log in", account_name))?;

        let token = random_hex(32);
        let session = Session {
            account: account_name,
            role,
            expires_at: crate::state::epoch_secs() + self.session_ttl.as_secs(),
            expires: Instant::now() + self.session_ttl,
        };
        self.lock_sessions().insert(token.clone(), session.clone());
        Ok((token, session))
    }

    /// The session a token names, if it is live. Expired entries are dropped as they
    /// are found rather than swept on a timer.
    pub fn session(&self, token: &str) -> Option<Session> {
        let mut sessions = self.lock_sessions();
        match sessions.get(token) {
            Some(session) if session.expires > Instant::now() => Some(session.clone()),
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }

    pub fn logout(&self, token: &str) {
        self.lock_sessions().remove(token);
    }

    /// Open a session without the chain handshake.
    ///
    /// Compiled only into the test binary: the shipped panel has exactly one way to
    /// create a session, and it goes through [`Self::login`].
    #[cfg(test)]
    pub(crate) fn mint_session(&self, account: &str, role: WebRole) -> String {
        let token = random_hex(32);
        self.lock_sessions().insert(
            token.clone(),
            Session {
                account: account.to_string(),
                role,
                expires_at: crate::state::epoch_secs() + self.session_ttl.as_secs(),
                expires: Instant::now() + self.session_ttl,
            },
        );
        token
    }

    fn lock_challenges(&self) -> std::sync::MutexGuard<'_, HashMap<String, Challenge>> {
        self.challenges.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Cryptographically random hex. `rand::thread_rng` is seeded from the OS and
/// reseeds itself; this is not the place for anything weaker.
fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hivecomb::sign::sign_message;
    use hivecomb::PrivateKey;

    fn auth() -> Auth {
        let mut access = BTreeMap::new();
        access.insert("alice".to_string(), WebRole::Admin);
        Auth::new(
            vec!["https://api.hive.blog".into()],
            Duration::from_secs(5),
            Duration::from_secs(3600),
            access,
        )
        .unwrap()
    }

    #[test]
    fn only_accounts_named_in_the_access_list_get_a_challenge() {
        let auth = auth();
        assert!(auth.challenge("alice").is_ok());
        // Case is not a security boundary; Hive account names are lowercase.
        assert!(auth.challenge("ALICE").is_ok());
        let err = auth.challenge("mallory").unwrap_err().to_string();
        assert!(err.contains("web.access"), "{err}");
    }

    /// Published in hivecomb's own example and holding no value.
    const THROWAWAY: &str = "5KQwrPbwdL6PhXujxW37FSSQZ1JiwsST4cqQzDeyXtP79zkvFD3";

    #[test]
    fn a_challenge_is_single_use() {
        let auth = auth();
        let message = auth.challenge("alice").unwrap();
        let key = PrivateKey::from_wif(THROWAWAY).unwrap();
        let signature = sign_message(message.as_bytes(), &key).unwrap().to_hex();

        let (account, recovered) = auth.recover_signer(&message, &signature).unwrap();
        assert_eq!(account, "alice");
        assert_eq!(recovered, key.public_key());

        // A replay of the very same signature finds no challenge to spend.
        let err = auth
            .recover_signer(&message, &signature)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown or already used"), "{err}");
    }

    #[test]
    fn a_signature_over_a_different_message_recovers_a_different_key() {
        let auth = auth();
        let message = auth.challenge("alice").unwrap();
        let key = PrivateKey::from_wif(THROWAWAY).unwrap();
        let signature = sign_message(b"some other message", &key).unwrap().to_hex();

        // This is the trap the chain check exists to close: signing the wrong thing
        // still recovers *a* valid key, just not the one that signed the challenge.
        let (_, recovered) = auth.recover_signer(&message, &signature).unwrap();
        assert_ne!(recovered, key.public_key());
    }

    #[test]
    fn a_challenge_that_was_never_issued_is_refused() {
        let auth = auth();
        let key = PrivateKey::from_wif(THROWAWAY).unwrap();
        let forged = "terracore-bot login as @alice at 0 [deadbeef]";
        let signature = sign_message(forged.as_bytes(), &key).unwrap().to_hex();
        let err = auth.recover_signer(forged, &signature).unwrap_err().to_string();
        assert!(err.contains("unknown or already used"), "{err}");
    }

    #[test]
    fn a_signature_that_is_not_a_signature_is_refused() {
        let auth = auth();
        let message = auth.challenge("alice").unwrap();
        let err = auth.recover_signer(&message, "not-hex").unwrap_err().to_string();
        assert!(err.contains("not a signature"), "{err}");
    }

    /// The check the whole panel rests on, against the live chain.
    ///
    /// A well-formed signature is not a login. This signs a genuine challenge with a
    /// genuine key that is simply not in the target account's posting authority, and
    /// requires the login to be refused for exactly that reason.
    ///
    ///     cargo test -- --ignored --nocapture
    #[test]
    #[ignore = "needs a Hive node"]
    fn a_key_outside_the_posting_authority_cannot_log_in() {
        let mut access = BTreeMap::new();
        access.insert("edsulivan".to_string(), WebRole::Viewer);
        let auth = Auth::new(
            vec!["https://api.hive.blog".into()],
            Duration::from_secs(15),
            Duration::from_secs(3600),
            access,
        )
        .unwrap();

        let message = auth.challenge("edsulivan").unwrap();
        let key = PrivateKey::from_wif(THROWAWAY).unwrap();
        let signature = sign_message(message.as_bytes(), &key).unwrap().to_hex();

        let err = auth.login(&message, &signature).unwrap_err().to_string();
        println!("refused: {err}");
        assert!(
            err.contains("posting authority"),
            "a throwaway key must be refused by the authority check, not by anything \
             else going wrong: {err}"
        );
    }

    #[test]
    fn an_unknown_token_names_no_session() {
        assert!(auth().session("deadbeef").is_none());
    }

    #[test]
    fn tokens_do_not_repeat() {
        assert_ne!(random_hex(32), random_hex(32));
        assert_eq!(random_hex(32).len(), 64);
    }
}
