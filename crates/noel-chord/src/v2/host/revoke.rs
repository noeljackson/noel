//! Revocation (upstream `host.revoke` and `RevocationKeys`): what
//! `revoked_by` says revokes a principal, compared as canonical JSON.

use super::relay::Holder;
use super::*;

fn revoked_error(label: &str) -> ChordError {
    ChordError::with_reason(Code::Forbidden, "revoked", format!("{label}: revoked"))
}

impl Actor {
    /// Ends every object whose principal `revocation` revokes and every
    /// unbound hold acting for one, sends `revoke` down route links, and
    /// closes child links of revoked principals.
    pub(super) fn revoke(&mut self, revocation: &Value) {
        let key = canonical_json(revocation);
        let revoked_by = self.revoked_by.clone();
        let revoked = |principal: &Value| {
            revoked_by.as_ref().is_some_and(|revoked_by| {
                revoked_by(principal)
                    .iter()
                    .any(|revocation| canonical_json(revocation) == key)
            })
        };

        // Served holds: of a revoked object, or unbound and acting for a
        // revoked principal.
        let mut ending = Vec::new();
        let mut waiting = Vec::new();
        for (link_id, link) in &self.links {
            for (origin, provider) in &link.providers {
                for (hold, served) in &provider.holds {
                    let label = match &served.object {
                        Some(object) => {
                            let object = &self.objects[object];
                            if !revoked(&object.principal) {
                                continue;
                            }
                            object.contract.declaration.name.clone()
                        }
                        None if revoked(&served.principal) => "an unbound request".to_owned(),
                        None => continue,
                    };
                    ending.push((*link_id, *origin, *hold, label));
                }
                for (hold, relayed) in &provider.relayed {
                    if relayed.upstream.is_none() && revoked(&relayed.principal) {
                        waiting.push((*link_id, *origin, *hold));
                    }
                }
            }
        }
        for (link, origin, hold, label) in ending {
            let error = revoked_error(&label);
            let Some(served) = self
                .provider(link, origin)
                .and_then(|p| p.holds.remove(&hold))
            else {
                continue;
            };
            self.forget_calls(link, origin, hold, error.clone());
            self.drop_hold(link, origin, hold, served);
            let error: WireError = error.into();
            self.send_origin(
                link,
                origin,
                Message::Gone {
                    origin: None,
                    hold,
                    error: error.to_json(),
                },
            );
        }
        // Relayed holds waiting for their path's link: the link they are held
        // on next gets no past `revoke`.
        for (link, origin, hold) in waiting {
            let Some(provider) = self.provider(link, origin) else {
                continue;
            };
            let Some(relayed) = provider.relayed.remove(&hold) else {
                continue;
            };
            provider.relayed_calls.retain(|_, held| *held != hold);
            self.path_detach(&relayed.path, Holder::Relayed { link, origin, hold });
            let error: WireError = revoked_error("an unbound request").into();
            self.send_origin(
                link,
                origin,
                Message::Gone {
                    origin: None,
                    hold,
                    error: error.to_json(),
                },
            );
        }
        // Down the route links before the child links close.
        let routes: Vec<u64> = self
            .links
            .iter()
            .filter(|(_, link)| link.role == Role::Route)
            .map(|(id, _)| *id)
            .collect();
        for link in routes {
            self.send(
                link,
                &Message::Revoke {
                    revocation: revocation.clone(),
                },
            );
        }
        let children: Vec<u64> = self
            .links
            .iter()
            .filter(|(_, link)| link.role == Role::Child && revoked(&link.principal))
            .map(|(id, _)| *id)
            .collect();
        // What was sent to them goes first.
        self.pump();
        for link in children {
            self.close_link(link);
        }
    }
}
