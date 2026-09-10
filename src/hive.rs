//! Signing and broadcasting.
//!
//! Signing happens offline: `hivecomb` needs the chain id, which is a compile-time
//! constant, and a recent block reference, which is cached. No node call sits between
//! deciding to attack and the signature existing.

use std::time::Duration;

use anyhow::{Context, Result};
use hivecomb::operations::{CustomJson, Operation};
use hivecomb::rpc::{HealthPolicy, NodeClient, UreqTransport};
use hivecomb::{Chain, PrivateKey, TaposCache, Transaction};
use rand::Rng;
use serde_json::{json, Value};
use tracing::debug;

/// Which authority a broadcast needs. Posting covers everything the game itself
/// reads; active is only for Hive-Engine token movements, which is why the two
/// features that need it are opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Posting,
    Active,
}

/// What a broadcast did. A dry run still signs, and still reports a real transaction
/// id, so a `--dry-run` log can be compared against a real one line for line. The
/// envelope itself goes to the debug log rather than being carried around.
#[derive(Debug, Clone)]
pub enum Sent {
    Broadcast { trx_id: String },
    DryRun { trx_id: String },
}

impl Sent {
    pub fn trx_id(&self) -> &str {
        match self {
            Sent::Broadcast { trx_id } | Sent::DryRun { trx_id } => trx_id,
        }
    }

    pub fn was_dry_run(&self) -> bool {
        matches!(self, Sent::DryRun { .. })
    }
}

pub struct Broadcaster {
    client: NodeClient<UreqTransport>,
    tapos: TaposCache,
    expiration_secs: u32,
    dry_run: bool,
}

impl Broadcaster {
    pub fn new(
        nodes: Vec<String>,
        timeout: Duration,
        expiration_secs: u32,
        tapos_max_age: Duration,
        dry_run: bool,
    ) -> Result<Self> {
        let client = NodeClient::new(UreqTransport, nodes)
            .context("building the Hive RPC client")?
            .with_timeout(timeout)
            // One extra pass over the node list before giving up, with a short
            // backoff. A game action missed this cycle is retried next cycle;
            // hammering a struggling node helps nobody.
            .with_retries(1, Duration::from_millis(500))
            .with_health_tracking(HealthPolicy::default());

        Ok(Self {
            client,
            tapos: TaposCache::with_max_age(tapos_max_age),
            expiration_secs,
            dry_run,
        })
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Confirm the nodes agree with the chain id this binary signs for. Cheap, once
    /// at startup, and it turns a silent stream of rejected transactions into one
    /// clear error.
    pub fn verify_chain(&self) -> Result<()> {
        self.client
            .verify_chain_id(Chain::Hive)
            .context("verifying the chain id against the configured nodes")?;
        Ok(())
    }

    /// Pull a fresh block reference into the cache. Called once per cycle rather than
    /// once per transaction: TaPoS stays valid for far longer than a cycle takes.
    pub fn refresh_tapos(&self) -> Result<()> {
        self.client
            .refresh_tapos(&self.tapos)
            .context("fetching a recent block reference")?;
        Ok(())
    }

    /// Sign and broadcast one `custom_json`.
    ///
    /// The game deduplicates on a `tx-hash` field, and its client adds one to every
    /// `terracore*` payload that does not already carry an `action`. Same rule here,
    /// so a payload built by this bot is byte-for-byte the shape the game expects.
    pub fn custom_json(
        &self,
        account: &str,
        key: &PrivateKey,
        auth: Auth,
        id: &str,
        payload: Value,
    ) -> Result<Sent> {
        let payload = add_tx_hash(id, payload);
        let json = serde_json::to_string(&payload).expect("a JSON value always re-serializes");

        let (required_auths, required_posting_auths) = match auth {
            Auth::Active => (vec![account.to_string()], vec![]),
            Auth::Posting => (vec![], vec![account.to_string()]),
        };

        let operation = Operation::CustomJson(CustomJson {
            required_auths,
            required_posting_auths,
            id: id.to_string(),
            json,
        });

        // `block_ref()` fails rather than handing back a stale reference, so a
        // transaction is never signed against a block the chain has moved past.
        let block_ref = match self.tapos.block_ref() {
            Ok(block_ref) => block_ref,
            Err(_) => self
                .client
                .refresh_tapos(&self.tapos)
                .context("refreshing an expired block reference")?,
        };

        let transaction = Transaction::new(block_ref, vec![operation], self.expiration_secs)
            .context("building the transaction")?;
        let signed = transaction
            .sign(&[key.clone()], Chain::Hive)
            .context("signing the transaction")?;
        let trx_id = signed.transaction.id().context("computing the transaction id")?;

        if self.dry_run {
            let envelope = signed.to_json().context("rendering the transaction")?;
            debug!(%trx_id, %id, envelope = %envelope, "dry run: not broadcasting");
            return Ok(Sent::DryRun { trx_id });
        }

        self.client
            .broadcast(&signed)
            .with_context(|| format!("broadcasting {id} for {account}"))?;
        Ok(Sent::Broadcast { trx_id })
    }

    /// A Hive-Engine token transfer, wrapped in the `ssc-mainnet-hive` custom_json
    /// the sidechain listens for. Always active authority: this moves tokens.
    ///
    /// `memo` is a `Value` and not a string on purpose -- the boss fight passes an
    /// object, and the sidechain carries it through either way.
    pub fn engine_transfer(
        &self,
        account: &str,
        key: &PrivateKey,
        symbol: &str,
        to: &str,
        quantity: &str,
        memo: Value,
    ) -> Result<Sent> {
        let payload = json!({
            "contractName": "tokens",
            "contractAction": "transfer",
            "contractPayload": {
                "symbol": symbol,
                "to": to,
                "quantity": quantity,
                "memo": memo,
            }
        });
        self.custom_json(account, key, Auth::Active, "ssc-mainnet-hive", payload)
    }
}

/// The game's idempotency nonce. The client builds it from two `Math.random()` calls
/// in base 36; the alphabet and length here match what it produces.
pub fn tx_hash() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

fn add_tx_hash(id: &str, mut payload: Value) -> Value {
    if !id.starts_with("terracore") {
        return payload;
    }
    if let Some(object) = payload.as_object_mut() {
        // A payload that already names an `action` carries its own nonce inside it;
        // the client skips `tx-hash` for exactly those.
        if !object.contains_key("action") {
            object.insert("tx-hash".into(), Value::String(tx_hash()));
        }
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terracore_payload_gets_a_nonce() {
        let out = add_tx_hash("terracore_battle", json!({ "target": "bob" }));
        assert_eq!(out["target"], "bob");
        assert_eq!(out["tx-hash"].as_str().unwrap().len(), 24);
    }

    #[test]
    fn a_payload_that_names_an_action_carries_its_own_nonce() {
        let out = add_tx_hash("terracore_equip", json!({ "action": "terracore_equip-abc" }));
        assert!(out.get("tx-hash").is_none());
    }

    #[test]
    fn a_sidechain_payload_is_left_alone() {
        let out = add_tx_hash("ssc-mainnet-hive", json!({ "contractName": "tokens" }));
        assert!(out.get("tx-hash").is_none());
    }

    #[test]
    fn nonces_differ_between_calls() {
        assert_ne!(tx_hash(), tx_hash());
    }
}
