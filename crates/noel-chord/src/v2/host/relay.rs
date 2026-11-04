//! Paths, routes and relayed holds (upstream `src/remote/path.ts`,
//! `relay.ts`, `host/route.ts` and `host/destination.ts`).
//!
//! A path is where upstream holds go: the parent link, or one address of a
//! route, across its links. Each new link of a path holds every holder of
//! the path again; when it drops, every holder is told. A hold this host
//! does not serve is relayed: held again on its path's link with the same id
//! under its downstream origin's number there, with messages forwarded both
//! ways, origins rewritten and parts untouched. Acknowledgements of relayed
//! `bound` and `state` messages go back to the link that sent them.

use super::*;

/// A path: the parent link, or a route's address (its kind and its params
/// as canonical JSON).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum PathKey {
    Parent,
    Route { kind: String, params: String },
}

/// What a path holds on its links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Holder {
    /// One of this host's references.
    Ref(u64),
    /// A hold this host relays: (downstream link, origin, hold).
    Relayed { link: u64, origin: u64, hold: u64 },
}

#[derive(Default)]
pub(super) struct PathState {
    link: Option<u64>,
    /// Whether it ever had a link.
    pub(super) linked: bool,
    holders: Vec<Holder>,
    /// A route path's address.
    address: Option<Address>,
    resolving: bool,
    /// Why its route could not reach it, until the next resolve.
    failure: Option<ChordError>,
}

/// What a route's `resolve` gives: the link to the address and how to run
/// it.
pub struct Resolved {
    pub link: Link,
    pub options: LinkOptions,
}

/// A host's route for one address kind: holds for its addresses go over
/// the link it resolves, one per address, resolved again when it drops.
pub trait Route: Send + Sync + 'static {
    /// Whether `principal` may reach `address`; runs for every hold routed
    /// over the route.
    fn authorize(&self, _principal: &Value, _address: &Address) -> bool {
        true
    }

    /// The link to `address`. Runs at most once at a time per address.
    fn resolve(&self, address: Address) -> BoxFuture<Result<Resolved, ChordError>>;
}

/// A hold this host relays.
pub(super) struct RelayedHold {
    pub(super) request: Request,
    /// Whom it acts for and its origin's incarnation, named on route links.
    pub(super) principal: Value,
    pub(super) incarnation: String,
    pub(super) chain: Option<Vec<String>>,
    pub(super) limit: Option<u64>,
    pub(super) path: PathKey,
    /// The upstream link and this origin's number on it, while held there.
    pub(super) upstream: Option<(u64, u64)>,
}

/// Where a hold goes.
pub(super) enum Placed {
    Here,
    Path(PathKey),
}

impl Actor {
    /// Whether `address` is this host's.
    pub(super) fn is_here(&self, address: &Address) -> bool {
        self.options.address.as_ref().is_some_and(|own| {
            own.kind == address.kind
                && canonical_json(&own.params) == canonical_json(&address.params)
        })
    }

    /// The route path to `address` when this host routes its kind, after
    /// its `authorize`.
    fn route_path(
        &mut self,
        address: &Address,
        principal: &Value,
    ) -> Result<Option<PathKey>, ChordError> {
        let Some(route) = self.routes.get(&address.kind) else {
            return Ok(None);
        };
        if !route.authorize(principal, address) {
            return Err(ChordError::new(
                Code::Forbidden,
                format!("{}: not authorized", address.kind),
            ));
        }
        let key = PathKey::Route {
            kind: address.kind.clone(),
            params: canonical_json(&address.params),
        };
        self.paths.entry(key.clone()).or_default().address = Some(address.clone());
        Ok(Some(key))
    }

    /// Where a hold arriving on `link` goes, by the address it names: here,
    /// over the route of its kind, else over the parent link, unless it came
    /// on the parent link or this host never had one.
    pub(super) fn place_hold(
        &mut self,
        link: u64,
        type_name: &str,
        address: &Address,
        principal: &Value,
    ) -> Result<Placed, ChordError> {
        if self.is_here(address) {
            return Ok(Placed::Here);
        }
        if let Some(key) = self.route_path(address, principal)? {
            return Ok(Placed::Path(key));
        }
        let parent_linked = self.paths.get(&PathKey::Parent).is_some_and(|p| p.linked);
        if self.links[&link].role == Role::Parent || !parent_linked {
            return Err(ChordError::new(
                Code::Unavailable,
                format!("{type_name} is not served here"),
            ));
        }
        Ok(Placed::Path(PathKey::Parent))
    }

    /// Where one of this host's references goes.
    pub(super) fn place_reference(&mut self, address: &Address) -> Result<PathKey, ChordError> {
        if self.is_here(address) {
            return Err(ChordError::new(
                Code::Unavailable,
                format!("{}: placed on this host", address.kind),
            ));
        }
        Ok(self
            .route_path(address, &Value::Null)?
            .unwrap_or(PathKey::Parent))
    }

    /// Holds `holder` over `key`: on its link now, and on every next one.
    pub(super) fn path_attach(&mut self, key: &PathKey, holder: Holder) {
        let path = self.paths.entry(key.clone()).or_default();
        path.holders.push(holder);
        if let Some(link) = path.link {
            self.holder_attach(holder, link);
        } else if let Some(failure) = path.failure.clone() {
            self.holder_unreachable(holder, failure);
        } else if matches!(key, PathKey::Route { .. }) && !path.resolving {
            self.resolve(key);
        }
    }

    /// `holder` was released or ended; no later link holds it.
    pub(super) fn path_detach(&mut self, key: &PathKey, holder: Holder) {
        if let Some(path) = self.paths.get_mut(key) {
            path.holders.retain(|held| *held != holder);
        }
    }

    /// `link` is the path's link now: every holder is held on it.
    pub(super) fn path_connected(&mut self, key: &PathKey, link: u64) {
        let path = self.paths.entry(key.clone()).or_default();
        path.link = Some(link);
        path.linked = true;
        path.failure = None;
        for holder in path.holders.clone() {
            self.holder_attach(holder, link);
        }
    }

    /// `link` closed: the holders of its path go loading until the next
    /// link; a route resolves its address again while it has holders.
    pub(super) fn path_disconnected(&mut self, link: u64) {
        let Some((key, path)) = self
            .paths
            .iter_mut()
            .find(|(_, path)| path.link == Some(link))
        else {
            return;
        };
        let key = key.clone();
        path.link = None;
        let holders = path.holders.clone();
        for holder in &holders {
            if let Holder::Relayed { link, origin, hold } = *holder {
                self.relayed_dropped(link, origin, hold);
            }
        }
        if matches!(key, PathKey::Route { .. }) && !holders.is_empty() {
            self.resolve(&key);
        }
    }

    fn resolve(&mut self, key: &PathKey) {
        let PathKey::Route { kind, .. } = key else {
            return;
        };
        let Some(route) = self.routes.get(kind).cloned() else {
            return;
        };
        let path = self.paths.get_mut(key).expect("the path");
        let Some(address) = path.address.clone() else {
            return;
        };
        path.resolving = true;
        let tx = self.tx.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let result = route.resolve(address).await;
            let _ = tx.send(Msg::Resolved { key, result });
        });
    }

    /// A route's `resolve` settled: the link becomes the path's, or its
    /// holders fail waiting until the next resolve.
    pub(super) fn resolved(&mut self, key: PathKey, result: Result<Resolved, ChordError>) {
        let Some(path) = self.paths.get_mut(&key) else {
            return;
        };
        path.resolving = false;
        match result {
            Ok(Resolved { link, options }) => {
                let id = link.id();
                self.attach(link, Role::Route, options);
                if self.links.contains_key(&id) {
                    self.path_connected(&key, id);
                }
            }
            Err(error) => {
                path.failure = Some(error.clone());
                for holder in path.holders.clone() {
                    self.holder_unreachable(holder, error.clone());
                }
            }
        }
    }

    fn holder_attach(&mut self, holder: Holder, link: u64) {
        match holder {
            Holder::Ref(reference) => self.hold_on(reference, link),
            Holder::Relayed {
                link: down,
                origin,
                hold,
            } => self.relayed_attach(down, origin, hold, link),
        }
    }

    /// The path's address cannot be reached for now: its holders fail
    /// waiting.
    fn holder_unreachable(&mut self, holder: Holder, error: ChordError) {
        match holder {
            Holder::Ref(reference) => {
                if let Some(entry) = self.refs.get(&reference) {
                    entry.set_status(RefStatus::Failed {
                        cause: CauseKind::Waiting,
                        error: error.clone().into(),
                        retry_in: None,
                    });
                }
                self.fail_waiting_calls(reference, error);
            }
            Holder::Relayed { link, origin, hold } => {
                self.send_origin(
                    link,
                    origin,
                    Message::Failed {
                        origin: None,
                        hold,
                        cause: CauseKind::Waiting,
                        error: WireError::from(error).to_json(),
                        retry_in: None,
                    },
                );
            }
        }
    }

    // ─── relayed holds ─────────────────────────────────────────────────────

    /// A hold from downstream that goes over `path`.
    pub(super) fn relay_hold(&mut self, link: u64, origin: u64, hold: u64, relayed: RelayedHold) {
        let path = relayed.path.clone();
        let Some(provider) = self.provider(link, origin) else {
            return;
        };
        provider.relayed.insert(hold, relayed);
        self.path_attach(&path, Holder::Relayed { link, origin, hold });
    }

    /// The downstream origin's number on upstream links, with `up` as one
    /// it relays to.
    fn relay_origin(&mut self, link: u64, origin: u64, up: u64) -> u64 {
        let next = self.next_relay + 1;
        let provider = self.provider(link, origin).expect("the provider");
        let relay = match provider.relay {
            Some(relay) => relay,
            None => {
                provider.relay = Some(next);
                next
            }
        };
        if !provider.relay_links.contains(&up) {
            provider.relay_links.push(up);
        }
        if relay == next {
            self.next_relay = next;
            self.relays.insert(relay, (link, origin));
        }
        relay
    }

    /// Holds a relayed hold on `up`, its path's link now.
    fn relayed_attach(&mut self, link: u64, origin: u64, hold: u64, up: u64) {
        if self
            .provider(link, origin)
            .is_none_or(|p| !p.relayed.contains_key(&hold))
        {
            return;
        }
        let relay = self.relay_origin(link, origin, up);
        let route = self.links.get(&up).is_some_and(|l| l.role == Role::Route);
        let provider = self.provider(link, origin).expect("the provider");
        let relayed = provider.relayed.get_mut(&hold).expect("the hold");
        relayed.upstream = Some((up, relay));
        let request = relayed.request.clone();
        let message = Message::Hold {
            origin: Some(relay),
            hold,
            type_name: request.type_name,
            version: request.version,
            fingerprint: request.fingerprint,
            params: request.params,
            address: request.address,
            principal: route.then(|| relayed.principal.clone()),
            incarnation: route.then(|| relayed.incarnation.clone()),
            chain: relayed.chain.clone(),
            limit: relayed.limit,
        };
        self.send(up, &message);
    }

    /// The upstream link of a relayed hold dropped: its calls fail and it
    /// goes loading downstream until the path's next link.
    fn relayed_dropped(&mut self, link: u64, origin: u64, hold: u64) {
        let Some(provider) = self.provider(link, origin) else {
            return;
        };
        let Some(relayed) = provider.relayed.get_mut(&hold) else {
            return;
        };
        if relayed.upstream.take().is_none() {
            return;
        }
        let calls: Vec<u64> = provider
            .relayed_calls
            .iter()
            .filter(|(_, held)| **held == hold)
            .map(|(call, _)| *call)
            .collect();
        for call in &calls {
            provider.relayed_calls.remove(call);
        }
        for call in calls {
            let error: WireError = link_closed().into();
            self.send_origin(
                link,
                origin,
                Message::Error {
                    origin: None,
                    call,
                    error: error.to_json(),
                },
            );
        }
        self.send_origin(link, origin, Message::Loading { origin: None, hold });
    }

    /// A message from downstream for a relayed hold, forwarded upstream.
    /// Returns `false` when the hold is not relayed.
    pub(super) fn relayed_message(&mut self, link: u64, message: &Message) -> bool {
        let origin = message.origin().unwrap_or(0);
        let Some(provider) = self.provider(link, origin) else {
            return false;
        };
        match message {
            Message::Release { hold, .. } => {
                let Some(relayed) = provider.relayed.remove(hold) else {
                    return false;
                };
                provider.relayed_calls.retain(|_, held| held != hold);
                self.path_detach(
                    &relayed.path,
                    Holder::Relayed {
                        link,
                        origin,
                        hold: *hold,
                    },
                );
                if let Some((up, relay)) = relayed.upstream {
                    self.send(
                        up,
                        &Message::Release {
                            origin: Some(relay),
                            hold: *hold,
                        },
                    );
                }
                true
            }
            Message::Call { call, hold, .. } => {
                let Some(relayed) = provider.relayed.get(hold) else {
                    return false;
                };
                match relayed.upstream {
                    Some((up, relay)) => {
                        provider.relayed_calls.insert(*call, *hold);
                        let mut forwarded = message.clone();
                        set_origin(&mut forwarded, Some(relay));
                        self.send(up, &forwarded);
                    }
                    None => {
                        let error: WireError = ChordError::new(
                            Code::Unavailable,
                            format!("hold {hold}: its upstream link is down"),
                        )
                        .into();
                        self.send_origin(
                            link,
                            origin,
                            Message::Error {
                                origin: None,
                                call: *call,
                                error: error.to_json(),
                            },
                        );
                    }
                }
                true
            }
            Message::Abort { call, .. } => {
                let Some(hold) = provider.relayed_calls.remove(call) else {
                    return false;
                };
                if let Some((up, relay)) = provider.relayed.get(&hold).and_then(|r| r.upstream) {
                    self.send(
                        up,
                        &Message::Abort {
                            origin: Some(relay),
                            call: *call,
                        },
                    );
                }
                true
            }
            Message::Retry { hold, .. } => {
                let Some(relayed) = provider.relayed.get(hold) else {
                    return false;
                };
                if let Some((up, relay)) = relayed.upstream {
                    let mut forwarded = message.clone();
                    set_origin(&mut forwarded, Some(relay));
                    self.send(up, &forwarded);
                }
                true
            }
            _ => false,
        }
    }

    /// The downstream origin is gone (its link or a `closed`): its relayed
    /// holds leave their paths and its upstream links are told `closed`.
    pub(super) fn relay_closed(&mut self, link: u64, origin: u64, provider: &mut Provider) {
        for (hold, relayed) in provider.relayed.drain() {
            self.path_detach(&relayed.path, Holder::Relayed { link, origin, hold });
        }
        if let Some(relay) = provider.relay.take() {
            self.relays.remove(&relay);
            for up in std::mem::take(&mut provider.relay_links) {
                self.send(up, &Message::Closed { origin: relay });
            }
        }
    }

    /// A message from upstream link `from` for a relayed hold, forwarded
    /// downstream. A link answers only for what was relayed over it.
    pub(super) fn forward(&mut self, from: u64, message: Message) {
        let Some(relay) = message.origin() else {
            return;
        };
        let Some(&(link, origin)) = self.relays.get(&relay) else {
            return;
        };
        let Some(provider) = self.provider(link, origin) else {
            return;
        };
        if !provider.relay_links.contains(&from) {
            return;
        }
        let over = |provider: &Provider, hold: u64| {
            provider
                .relayed
                .get(&hold)
                .is_some_and(|r| r.upstream.map(|(up, _)| up) == Some(from))
        };
        let forwarded = match message {
            Message::Bound { holds, .. } => {
                let holds: Vec<BoundEntry> = holds
                    .into_iter()
                    .filter(|e| over(provider, e.hold))
                    .collect();
                if holds.is_empty() {
                    self.send(
                        from,
                        &Message::Ack {
                            origin: Some(relay),
                        },
                    );
                    return;
                }
                provider.sent.push_back(Some(from));
                Message::Bound {
                    origin: None,
                    holds,
                }
            }
            Message::State { holds, .. } => {
                let holds: Vec<StateEntry> = holds
                    .into_iter()
                    .filter(|e| over(provider, e.hold))
                    .collect();
                if holds.is_empty() {
                    self.send(
                        from,
                        &Message::Ack {
                            origin: Some(relay),
                        },
                    );
                    return;
                }
                provider.sent.push_back(Some(from));
                Message::State {
                    origin: None,
                    holds,
                }
            }
            Message::Result {
                call,
                value,
                segments,
                ..
            } => {
                let Some(hold) = provider.relayed_calls.get(&call).copied() else {
                    return;
                };
                if !over(provider, hold) {
                    return;
                }
                provider.relayed_calls.remove(&call);
                Message::Result {
                    origin: None,
                    call,
                    value,
                    segments,
                }
            }
            Message::Error { call, error, .. } => {
                let Some(hold) = provider.relayed_calls.get(&call).copied() else {
                    return;
                };
                if !over(provider, hold) {
                    return;
                }
                provider.relayed_calls.remove(&call);
                Message::Error {
                    origin: None,
                    call,
                    error,
                }
            }
            Message::Gone { hold, error, .. } => {
                if !over(provider, hold) {
                    return;
                }
                let relayed = provider.relayed.remove(&hold).expect("the hold");
                provider.relayed_calls.retain(|_, held| *held != hold);
                self.path_detach(&relayed.path, Holder::Relayed { link, origin, hold });
                Message::Gone {
                    origin: None,
                    hold,
                    error,
                }
            }
            Message::Failed {
                hold,
                cause,
                error,
                retry_in,
                ..
            } => {
                if !over(provider, hold) {
                    return;
                }
                Message::Failed {
                    origin: None,
                    hold,
                    cause,
                    error,
                    retry_in,
                }
            }
            Message::Loading { hold, .. } => {
                if !over(provider, hold) {
                    return;
                }
                Message::Loading { origin: None, hold }
            }
            _ => return,
        };
        self.send_origin(link, origin, forwarded);
    }
}

/// Sets a message's origin.
pub(super) fn set_origin(message: &mut Message, origin: Option<u64>) {
    match message {
        Message::Hold { origin: o, .. }
        | Message::Release { origin: o, .. }
        | Message::Call { origin: o, .. }
        | Message::Abort { origin: o, .. }
        | Message::Retry { origin: o, .. }
        | Message::Bound { origin: o, .. }
        | Message::State { origin: o, .. }
        | Message::Ack { origin: o }
        | Message::Failed { origin: o, .. }
        | Message::Loading { origin: o, .. }
        | Message::Result { origin: o, .. }
        | Message::Error { origin: o, .. }
        | Message::Gone { origin: o, .. } => *o = origin,
        Message::Closed { .. }
        | Message::Hello { .. }
        | Message::Revoke { .. }
        | Message::Ping
        | Message::Pong => {}
    }
}
