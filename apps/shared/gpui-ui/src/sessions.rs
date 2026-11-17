use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionGroup {
    pub workspace: String,
    pub sessions: Vec<Value>,
}

pub fn workspace_label(session: &Value) -> String {
    let workspace = session["workspace"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| session["cwd"].as_str())
        .unwrap_or(".");
    let trimmed = workspace.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(trimmed)
        .to_owned()
}

pub fn group_sessions(sessions: &[Value]) -> Vec<SessionGroup> {
    let mut groups = BTreeMap::<String, Vec<Value>>::new();
    for session in sessions {
        let key = workspace_label(session);
        groups.entry(key).or_default().push(session.clone());
    }
    let mut groups: Vec<SessionGroup> = groups
        .into_iter()
        .map(|(workspace, mut sessions)| {
            sessions.sort_by_key(|session| {
                std::cmp::Reverse(session["modified"].as_u64().unwrap_or(0))
            });
            let workspace = sessions.first().map(workspace_label).unwrap_or(workspace);
            SessionGroup {
                workspace,
                sessions,
            }
        })
        .collect();
    groups.sort_by_key(|group| {
        std::cmp::Reverse(
            group
                .sessions
                .first()
                .and_then(|session| session["modified"].as_u64())
                .unwrap_or(0),
        )
    });
    groups
}

pub fn relative_time_label(modified: u64, now: SystemTime) -> String {
    let modified = UNIX_EPOCH + Duration::from_millis(modified);
    let seconds = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    if seconds < 60 {
        "now".to_owned()
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else if seconds < 2_592_000 {
        format!("{}d", seconds / 86_400)
    } else {
        format!("{}mo", seconds / 2_592_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_use_workspace_then_cwd_and_sort_newest_first() {
        let sessions = vec![
            serde_json::json!({"sessionId":"old","cwd":"/src/noel","modified":1000}),
            serde_json::json!({"sessionId":"new","workspace":"ssh://host/src/noel","modified":2000}),
            serde_json::json!({"sessionId":"other","cwd":"/src/other","modified":1500}),
        ];
        let groups = group_sessions(&sessions);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].workspace, "noel");
        assert_eq!(groups[0].sessions[0]["sessionId"], "new");
        assert_eq!(groups[0].sessions[1]["sessionId"], "old");
        assert_eq!(groups[1].workspace, "other");
    }

    #[test]
    fn relative_time_is_compact() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000_000);
        let now_ms = now.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        assert_eq!(relative_time_label(now_ms, now), "now");
        assert_eq!(relative_time_label(now_ms - 1_980_000, now), "33m");
        assert_eq!(relative_time_label(now_ms - 345_600_000, now), "4d");
        assert_eq!(relative_time_label(now_ms - 2_592_000_000, now), "1mo");
    }
}
