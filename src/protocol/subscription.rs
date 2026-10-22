//! Subscriptions to a stream (docs/protocol.md §5.1): which scopes a stream
//! follows and what version of each a client already holds.
use super::documents::{ErrorCode, ProtocolError, Scope};
pub(crate) use super::engine::Have;

/// One scope a stream follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscribed {
    pub scope: Scope,
    /// The version of the replica the client holds; none without `have`.
    pub have: Option<Have>,
    /// The working directory to load an unheld session with.
    pub cwd: Option<String>,
}

/// The scopes of `GET /api/v2/stream?scope=…&have=…&cwd=…` (protocol §5.1).
pub(crate) fn parse_stream_query(query: &str) -> Result<Vec<Subscribed>, ProtocolError> {
    let invalid = |message: String| ProtocolError::new(ErrorCode::InvalidRequest, message);
    let mut scopes: Vec<Subscribed> = Vec::new();
    let mut haves = Vec::new();
    let mut cwds = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "scope" => {
                let scope = Scope::parse(&value)
                    .ok_or_else(|| invalid(format!("not a scope: {value:?}")))?;
                if scopes.iter().any(|followed| followed.scope == scope) {
                    return Err(invalid(format!("scope named twice: {value}")));
                }
                scopes.push(Subscribed {
                    scope,
                    have: None,
                    cwd: None,
                });
            }
            "have" => haves.push(value.into_owned()),
            "cwd" => cwds.push(value.into_owned()),
            _ => return Err(invalid(format!("unknown stream parameter: {key}"))),
        }
    }
    if scopes.is_empty() {
        return Err(invalid("a stream follows at least one scope".into()));
    }
    for have in haves {
        // `<scope>:<epoch>:<rev>`; a session ID may itself hold a colon, an
        // epoch (base64url) and a rev (a number) never do.
        let mut parts = have.rsplitn(3, ':');
        let (Some(rev), Some(epoch), Some(scope)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid(format!("have is not <scope>:<epoch>:<rev>: {have:?}")));
        };
        let rev: u64 = rev
            .parse()
            .map_err(|_| invalid(format!("have names no rev: {have:?}")))?;
        if epoch.is_empty() {
            return Err(invalid(format!("have names no epoch: {have:?}")));
        }
        let followed = scopes
            .iter_mut()
            .find(|followed| followed.scope.to_string() == scope)
            .ok_or_else(|| invalid(format!("have of a scope not followed: {have:?}")))?;
        if followed
            .have
            .replace(Have {
                epoch: epoch.to_string(),
                rev,
            })
            .is_some()
        {
            return Err(invalid(format!("have named twice for {scope}")));
        }
    }
    for cwd in cwds {
        // `<scope>:<path>`: the longest followed session scope it starts with.
        let target = scopes
            .iter_mut()
            .filter(|followed| matches!(followed.scope, Scope::Session(_)))
            .map(|followed| (followed.scope.to_string(), followed))
            .filter(|(scope, _)| {
                cwd.strip_prefix(scope.as_str())
                    .is_some_and(|rest| rest.starts_with(':'))
            })
            .max_by_key(|(scope, _)| scope.len());
        let Some((scope, followed)) = target else {
            return Err(invalid(format!(
                "cwd of a session scope not followed: {cwd:?}"
            )));
        };
        let path = &cwd[scope.len() + 1..];
        if path.is_empty() {
            return Err(invalid(format!("cwd names no directory: {cwd:?}")));
        }
        if followed.cwd.replace(path.to_string()).is_some() {
            return Err(invalid(format!("cwd named twice for {scope}")));
        }
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests;
