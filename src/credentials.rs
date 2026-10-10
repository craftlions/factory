//! Secrets harnesses sign in with, entered in the dashboard's settings and
//! kept in the database. A session's guest gets them as environment variables,
//! not on its disk, and the dashboard never reads them back.

use crate::db;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

const MAX_SECRET: usize = 4096;

/// One way a harness can sign in.
#[derive(Serialize)]
pub struct Kind {
    pub id: &'static str,
    pub name: &'static str,
    /// Where to get one.
    pub hint: &'static str,
    /// The environment variable the harness reads it from.
    #[serde(skip)]
    pub variable: &'static str,
}

#[derive(Serialize)]
pub struct Harness {
    pub id: &'static str,
    pub name: &'static str,
    pub kinds: &'static [Kind],
}

/// Harnesses whose credentials the settings manage.
pub const HARNESSES: &[Harness] = &[Harness {
    id: "claude",
    name: "Claude Code",
    kinds: &[
        Kind {
            id: "api_key",
            name: "Anthropic API key",
            hint: "Create one in the Claude Console. Usage is billed to its organization.",
            variable: "ANTHROPIC_API_KEY",
        },
        Kind {
            id: "oauth_token",
            name: "Claude subscription token",
            hint: "Run `claude setup-token` where you are signed in to Claude. The token lasts a year.",
            variable: "CLAUDE_CODE_OAUTH_TOKEN",
        },
    ],
}];

pub fn harness(id: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|harness| harness.id == id)
}

fn kind(harness: &'static Harness, id: &str) -> Option<&'static Kind> {
    harness.kinds.iter().find(|kind| kind.id == id)
}

/// The variables that pass the harness's credential into a guest. `Ok(None)`
/// for a harness whose credentials the settings do not manage.
pub async fn guest_env(
    pool: &SqlitePool,
    harness_id: &str,
) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(harness) = harness(harness_id) else {
        return Ok(None);
    };
    let stored = db::credential(pool, harness_id)
        .await
        .map_err(|error| error.to_string())?;
    let found = stored.and_then(|stored| Some((kind(harness, &stored.kind)?, stored.secret)));
    match found {
        Some((kind, secret)) => Ok(Some(vec![(kind.variable.to_owned(), secret)])),
        None => Err(format!(
            "{} has no credentials yet. Add them under Settings.",
            harness.name
        )),
    }
}

#[derive(Deserialize)]
pub struct Entry {
    pub kind: String,
    pub secret: String,
}

#[derive(Serialize)]
pub struct Stored {
    pub kind: String,
    /// Enough of the secret to recognise it.
    pub hint: String,
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub harness: &'static Harness,
    pub stored: Option<Stored>,
}

fn hint(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 12 {
        return "set".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("…{tail}")
}

pub async fn statuses(pool: &SqlitePool) -> sqlx::Result<Vec<Status>> {
    let stored = db::credentials(pool).await?;
    Ok(HARNESSES
        .iter()
        .map(|harness| Status {
            harness,
            stored: stored
                .iter()
                .find(|stored| stored.harness == harness.id)
                .map(|stored| Stored {
                    kind: stored.kind.clone(),
                    hint: hint(&stored.secret),
                    updated_at: stored.updated_at,
                }),
        })
        .collect())
}

/// Checks an entry for the harness and returns the secret to store.
pub fn validate<'a>(harness_id: &str, entry: &'a Entry) -> Result<&'a str, String> {
    let harness =
        harness(harness_id).ok_or_else(|| format!("{harness_id} takes no credentials"))?;
    if kind(harness, &entry.kind).is_none() {
        return Err(format!(
            "{} cannot sign in with {}",
            harness.name, entry.kind
        ));
    }
    let secret = entry.secret.trim();
    if secret.is_empty() {
        return Err("the secret must not be empty".into());
    }
    if secret.len() > MAX_SECRET || secret.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the secret must be one line without spaces".into());
    }
    Ok(secret)
}
