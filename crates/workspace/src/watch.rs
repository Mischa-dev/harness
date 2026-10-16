//! The polling fallback: a look at the inbox every few minutes, in case a
//! signed wakeup was lost or the daemon was down when it came.

use std::collections::HashSet;

use crate::api::{InboxEntry, Workspace};
use crate::client::McpError;

/// Workspace's largest inbox page.
pub const INBOX_LIMIT: u16 = 50;

/// Reply requests the agent hasn't been told about yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewRequests {
    pub count: usize,
    /// Of those, how many need an answer or mention the agent.
    pub pressing: usize,
}

/// Remembers which pending requests it has reported, so each one wakes the
/// agent once. A request that comes back (released, or its claim expired)
/// with a new generation is new again.
#[derive(Debug, Default)]
pub struct InboxWatch {
    reported: HashSet<(String, i32)>,
}

impl InboxWatch {
    pub async fn check(&mut self, workspace: &Workspace) -> Result<Option<NewRequests>, McpError> {
        let entries = workspace.inbox(INBOX_LIMIT).await?;
        Ok(self.update(&entries))
    }

    /// What's new in this page of the inbox. Only the pending requests
    /// still listed are remembered, so the memory stays one page long.
    pub fn update(&mut self, entries: &[InboxEntry]) -> Option<NewRequests> {
        let pending: Vec<&InboxEntry> = entries.iter().filter(|e| e.status == "pending").collect();
        let mut new = NewRequests {
            count: 0,
            pressing: 0,
        };
        for entry in &pending {
            if !self
                .reported
                .contains(&(entry.request_id.clone(), entry.generation))
            {
                new.count += 1;
                if entry.required || entry.mentioned {
                    new.pressing += 1;
                }
            }
        }
        self.reported = pending
            .iter()
            .map(|e| (e.request_id.clone(), e.generation))
            .collect();
        (new.count > 0).then_some(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, generation: i32, status: &str, required: bool) -> InboxEntry {
        InboxEntry {
            request_id: id.into(),
            required,
            mentioned: false,
            status: status.into(),
            generation,
        }
    }

    #[test]
    fn each_pending_request_is_reported_once() {
        let mut watch = InboxWatch::default();
        let first = [
            entry("a", 1, "pending", true),
            entry("b", 1, "pending", false),
        ];
        assert_eq!(
            watch.update(&first),
            Some(NewRequests {
                count: 2,
                pressing: 1
            })
        );
        assert_eq!(watch.update(&first), None);
        // The agent claims one; another arrives.
        let second = [
            entry("a", 1, "claimed", true),
            entry("b", 1, "pending", false),
            entry("c", 1, "pending", false),
        ];
        assert_eq!(
            watch.update(&second),
            Some(NewRequests {
                count: 1,
                pressing: 0
            })
        );
        // The claim lapses and the request comes back as a new generation.
        let third = [
            entry("a", 2, "pending", true),
            entry("b", 1, "pending", false),
            entry("c", 1, "pending", false),
        ];
        assert_eq!(
            watch.update(&third),
            Some(NewRequests {
                count: 1,
                pressing: 1
            })
        );
        // Claimed requests never wake the agent.
        assert_eq!(watch.update(&[entry("d", 1, "claimed", true)]), None);
    }
}
