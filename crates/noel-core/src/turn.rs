//! The turn loop: a prompt, a resumed or continued run, its provider rounds, retries and overflow compaction, and each tool call's run.

use super::*;

pub async fn run_user_turn_streaming_events_with_media(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    prompt: String,
    media: Vec<MediaInput>,
    run: &RunControl,
    on_event: impl FnMut(&TurnEvent) + Send,
) -> Result<String, AgentError> {
    // Calls a stop left without a result answer as interrupted: the new
    // prompt ends their turn.
    runtime.settle_unanswered_calls()?;
    let message = user_message(prompt, media);
    runtime.push_message(message.clone())?;
    run_saved_prompt_streaming_events(runtime, provider, message, run, on_event).await
}

/// Runs `message`, a user prompt the caller has just saved (as
/// [`Runtime::start_run`] does): a tool command runs its tool without the
/// model, and any other prompt runs provider rounds until a reply calls no
/// tools. The prompt is taken as it is: a caller expands typed input with
/// [`expand_input`] first.
pub async fn run_saved_prompt_streaming_events(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    message: ConversationMessage,
    run: &RunControl,
    on_event: impl FnMut(&TurnEvent) + Send,
) -> Result<String, AgentError> {
    if let Some(command) = parse_tool_command(&message.content)? {
        if !model_tool_enabled(runtime, &command.name) {
            return Err(AgentError::DisabledTool(command.name));
        }
        let edited_path = match &command.request {
            ToolRequest::Edit { path, .. } | ToolRequest::Write { path, .. } => Some(path.clone()),
            _ => None,
        };
        let env = runtime
            .tool_env()
            .map_err(|message| AgentError::Tool(ToolError::Failed(message)))?;
        let result = run
            .until_cancelled(execute_tool_in(
                env.as_ref(),
                command.request,
                &tool_runtime_options(runtime, Vec::new()),
            ))
            .await??;
        if let Some(path) = edited_path {
            runtime.note_edited_files(vec![path])?;
        }
        runtime.push_tool_event(ToolEvent {
            id: format!("tool-{}", runtime.session.tool_history.len() + 1),
            name: command.name.clone(),
            result: result.output.clone(),
            is_error: result.is_error,
        })?;
        runtime.push_message(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Tool,
            content: result.output.clone(),
            media: tool_result_media(result.images),
            tool_call_id: None,
            tool_name: Some(command.name.clone()),
            tool_calls: Vec::new(),
            aborted: false,
        })?;
        return Ok(result.output);
    }

    run_model_turn(runtime, provider, run, Some(message), 0, on_event).await
}

/// Continues an unfinished turn from its saved context, without adding a
/// user prompt. Saved results go back to the provider. A tool call without
/// a result runs again only if it is replay-safe ([`Runtime::replay_safe`]);
/// any other call never runs again: its result is the output it had saved,
/// and says it was interrupted, so an uncertain edit or command never runs
/// twice. A request the stop cut off is sent again; what its reply had
/// streamed was saved as an aborted reply when the session opened, and is
/// not part of the request.
pub async fn continue_turn_streaming_events(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    run: &RunControl,
    mut on_event: impl FnMut(&TurnEvent) + Send,
) -> Result<String, AgentError> {
    if let Some(store) = runtime.store.clone() {
        store.recover_partial(&mut runtime.session)?;
    }
    if !runtime.session().has_unfinished_turn() {
        return Ok(String::new());
    }
    let retries = match run.interrupted_attempt {
        Some(attempt) => charge_interrupted_request(runtime, run, attempt, &mut on_event).await?,
        None => 0,
    };
    settle_pending_calls(runtime, provider, run, &mut on_event).await?;
    run_model_turn(runtime, provider, run, None, retries, on_event).await
}

/// The text of a model request the host's death cut off (Durable).
pub fn interrupted_request_text(attempt: u64) -> String {
    format!("The host stopped while the model was answering, on attempt {attempt}")
}

/// Charges a request the host's death cut off to the retry policy, as
/// Durable does: when another attempt is allowed the turn waits out its
/// backoff (reported as a retry) and returns the retries so far; otherwise
/// the turn ends `interrupted`.
pub(crate) async fn charge_interrupted_request(
    runtime: &Runtime,
    run: &RunControl,
    attempt: u64,
    on_event: &mut (impl FnMut(&TurnEvent) + Send),
) -> Result<u64, AgentError> {
    let message = interrupted_request_text(attempt);
    let policy = retry_policy(&runtime.systems.retry);
    let used = attempt.saturating_sub(1);
    if used >= policy.max_retries {
        return Err(AgentError::Interrupted(message));
    }
    let delay = noel_agent::retry_delay_ms(&policy, used);
    on_event(&TurnEvent::RetryStarted(RetryNotice {
        attempt,
        max_attempts: policy.max_retries,
        delay_ms: delay,
        error_message: message.clone(),
    }));
    let backoff = run.retry.backoff(Duration::from_millis(delay));
    if run.until_cancelled(backoff).await? {
        return Ok(attempt);
    }
    on_event(&TurnEvent::RetryFinished {
        attempt,
        error: Some(RETRY_CANCELLED.into()),
    });
    Err(AgentError::Interrupted(message))
}

/// Settles the calls the last reply still owes results for, in order:
/// replay-safe calls run again, as the end of that reply's round, and any
/// other call gets its interrupted result, unreported.
pub(crate) async fn settle_pending_calls(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    run: &RunControl,
    on_event: &mut (impl FnMut(&TurnEvent) + Send),
) -> Result<(), AgentError> {
    let Some((reply, pending)) = runtime.session.pending_tool_calls() else {
        return Ok(());
    };
    let reply = reply.clone();
    let calls = pending.into_iter().cloned().collect::<Vec<_>>();
    let rerun = calls.iter().any(|call| runtime.replay_safe(&call.name));
    if rerun {
        on_event(&TurnEvent::RoundResumed(reply));
    }
    for call in calls {
        if runtime.replay_safe(&call.name) {
            // The journal does not record how the reply stopped. A reply
            // cut off at the output limit gets its calls' error results
            // right after it is saved, so its calls are not left for here;
            // these only read.
            run_tool_call(runtime, provider, run, call, false, on_event).await?;
        } else {
            runtime.push_interrupted_result(&call)?;
        }
    }
    if rerun {
        on_event(&TurnEvent::RoundFinished);
    }
    Ok(())
}

/// Runs provider rounds until a reply calls no tools. `prompt` is the user
/// message the caller just saved; it is reported as the first round's.
/// `retries` are those the reply asked for has had already.
pub(crate) async fn run_model_turn(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    run: &RunControl,
    mut prompt: Option<ConversationMessage>,
    retries: u64,
    mut on_event: impl FnMut(&TurnEvent) + Send,
) -> Result<String, AgentError> {
    runtime.load_workspace_context(run).await?;
    // The v2 hosts start with the run that first needs them, so their
    // handshakes' registrations reach the prompt and the provider request
    // of this turn (upstream loads its extensions at startup; noel's are
    // processes, started when they can affect something). A host that
    // cannot start stays fail-closed: its tool calls report the failure.
    runtime.start_extension_hosts().await;
    let system_prompt = runtime_system_prompt(runtime);
    runtime.thinking_level = provider.thinking_level();
    let mut final_text = String::new();
    let max_tool_rounds = runtime.systems.max_tool_rounds;
    // Auto-compaction that failed on unchanged history will fail again; skip
    // re-attempts (and repeated summarizer calls) until history changes.
    let mut failed_compaction_fingerprint: Option<(usize, Option<usize>)> = None;
    // Steering that arrived during a final reply has opened the next round.
    let mut round_open = false;
    // Replies kept so far. A round whose reply is dropped is not counted.
    let mut rounds = 0;
    let mut recovery = Recovery {
        retries,
        ..Recovery::default()
    };
    // What the reply being asked for has streamed, saved as it goes.
    let mut progress = Mutex::new(ReplyProgress::new(
        runtime.store.clone(),
        run.run_id.clone(),
        runtime.systems.progress.partial_interval,
    ));
    while rounds < max_tool_rounds {
        if !std::mem::take(&mut round_open) {
            on_event(&TurnEvent::RoundStarted);
        }
        if let Some(prompt) = prompt.take() {
            on_event(&TurnEvent::MessageAdded(prompt));
        }
        if let Some((message, placed)) = steering_message(&run.steering) {
            add_placed_message(runtime, message, placed, &mut on_event)?;
        }
        let fingerprint = (
            runtime.session.messages.len(),
            runtime.session.messages.last().map(|m| m.content.len()),
        );
        if runtime.needs_auto_compaction() && failed_compaction_fingerprint != Some(fingerprint) {
            on_event(&TurnEvent::CompactionStarted);
            // Compaction saves its checkpoint only once complete, so a
            // cancelled one leaves the history untouched.
            match run
                .until_cancelled(runtime.auto_compact_midturn(provider))
                .await?
            {
                Ok(Some(record)) => {
                    failed_compaction_fingerprint = None;
                    on_event(&TurnEvent::HistoryRewritten(
                        runtime.session.messages.clone(),
                    ));
                    on_event(&TurnEvent::CompactionFinished(record));
                }
                Ok(None) => {}
                Err(error) => {
                    // A failed compaction must not strand the session: keep
                    // the original history and continue the turn so the user
                    // can still steer, /compact manually, or start a new
                    // session. Only the provider knows the true limit.
                    failed_compaction_fingerprint = Some(fingerprint);
                    on_event(&TurnEvent::CompactionFailed(error.to_string()));
                }
            }
        }
        let request = provider_request(runtime, system_prompt.clone());
        let reply = request_reply(provider, request, run, &progress, &mut on_event).await;
        let progress = progress.get_mut().unwrap_or_else(PoisonError::into_inner);
        let events = match reply {
            Ok(events) => events,
            // The failed reply was never saved: recovering only asks again.
            Err(AgentError::Provider(error)) => {
                let recovered = recovery
                    .after_failure(runtime, provider, run, error, progress, &mut on_event)
                    .await;
                match recovered {
                    Ok(()) => continue,
                    Err(error) => return Err(abort_reply(runtime, progress, error)),
                }
            }
            Err(error) => return Err(abort_reply(runtime, progress, error)),
        };
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = None;
        let mut truncated = false;
        for event in events {
            match event {
                StreamEvent::Text(delta) => {
                    text.push_str(&delta);
                }
                StreamEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_calls.push(ChatToolCall {
                    id,
                    name,
                    arguments,
                }),
                StreamEvent::Thinking(delta) => thinking.push_str(&delta),
                StreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => usage = Some((input_tokens, output_tokens)),
                StreamEvent::Stop { reason } => truncated |= noel_ai::is_output_limit_stop(&reason),
            }
        }

        if let Some((input, output)) = usage {
            runtime.record_usage(input, output)?;
        }
        let reply = ConversationMessage {
            thinking,
            role: MessageRole::Assistant,
            content: text,
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: tool_calls.clone(),
            aborted: false,
        };
        // A final reply cut off below the model's own output limit ran out
        // of context, like a refused request. (One with tool calls goes on:
        // its calls get error results and the model issues them again.)
        let output_tokens = usage.map_or(0, |(_, output)| output);
        if truncated
            && tool_calls.is_empty()
            && provider
                .output_limit()
                .is_none_or(|limit| output_tokens < limit)
            && recovery.can_compact(runtime)
        {
            on_event(&TurnEvent::Overflowed { error: None });
            recovery.finish_retries(None, &mut on_event);
            // What was saved of the reply no longer stands; the reply is
            // still saved below if nothing replaces it.
            progress.dropped(progress::Dropped::Compaction)?;
            if recovery
                .compact(runtime, provider, run, &mut on_event)
                .await?
            {
                continue;
            }
            // Nothing replaces the reply, so it stands, already reported.
            final_text.push_str(&reply.content);
            runtime.push_message(reply)?;
            return Ok(final_text);
        }
        rounds += 1;
        final_text.push_str(&reply.content);
        add_message(runtime, reply, &mut on_event)?;
        progress.kept();
        recovery.finish_retries(None, &mut on_event);
        if !truncated {
            recovery.overflowed = false;
        }

        if tool_calls.is_empty() {
            on_event(&TurnEvent::RoundFinished);
            // A steering message that arrived while the final response was
            // streaming keeps the turn going instead of being dropped.
            let Some((message, placed)) = steering_message(&run.steering) else {
                // The turn settled: the v2 hosts hear it (upstream's
                // `agent_settled`, notification work after the run).
                {
                    let cwd = runtime.session.cwd.clone();
                    let hosts = runtime.extension_hosts();
                    let mut hosts = hosts.lock().await;
                    let _ = hosts.notify("agentSettled", &cwd).await;
                }
                return Ok(final_text);
            };
            on_event(&TurnEvent::RoundStarted);
            add_placed_message(runtime, message, placed, &mut on_event)?;
            round_open = true;
            continue;
        }

        for (index, tool_call) in tool_calls.iter().enumerate() {
            let result = run_tool_call(
                runtime,
                provider,
                run,
                tool_call.clone(),
                truncated,
                &mut on_event,
            );
            if let Err(error) = result.await {
                if matches!(error, AgentError::Cancelled) {
                    // The abort stopped this call, which saved what it had
                    // printed, or kept it and those after it from starting.
                    for call in &tool_calls[index..] {
                        if !runtime.session.has_result(&call.id) {
                            let output = progress::ABORTED_TOOL.to_owned();
                            runtime.push_tool_result(call, output, Vec::new(), true)?;
                        }
                    }
                }
                return Err(error);
            }
        }
        on_event(&TurnEvent::RoundFinished);
    }

    Err(AgentError::RoundLimitReached {
        max_rounds: max_tool_rounds,
    })
}

/// What a turn has done so far to recover the reply it is asking for, as
/// upstream's agent session does after a failed run: a transient failure is
/// retried after a backoff, and a context overflow is answered by one
/// compaction. Both drop the failed reply and ask again.
#[derive(Default)]
pub(crate) struct Recovery {
    /// Retries of the reply being requested.
    retries: u64,
    /// Whether an overflow was already answered by compaction since the
    /// last reply that was not cut off.
    overflowed: bool,
}

impl Recovery {
    /// Recovers from a failed request for a reply: `Ok` when the turn may
    /// ask again, or the error that ends it.
    /// `progress` is the failed reply's, which is dropped once the turn
    /// asks again.
    async fn after_failure(
        &mut self,
        runtime: &mut Runtime,
        provider: &dyn Provider,
        run: &RunControl,
        error: ProviderError,
        progress: &mut ReplyProgress,
        on_event: &mut (impl FnMut(&TurnEvent) + Send),
    ) -> Result<(), AgentError> {
        let model = runtime.session.active_model.as_ref();
        let model_provider = model.map(|model| model.provider.clone());
        if error.is_context_overflow(model_provider.as_deref().unwrap_or_default()) {
            if self.can_compact(runtime) {
                on_event(&TurnEvent::Overflowed {
                    error: Some(error.to_string()),
                });
                self.finish_retries(Some(&error), on_event);
                progress.dropped(progress::Dropped::Compaction)?;
                if let Some(store) = &runtime.store {
                    store.append(&SessionRecord::RetryWait {
                        run_id: run.run_id.clone(),
                        attempt: 0,
                    })?;
                }
                if self.compact(runtime, provider, run, on_event).await? {
                    return Ok(());
                }
                return Err(error.into());
            }
        } else if error.is_retryable() {
            let policy = retry_policy(&runtime.systems.retry);
            if self.retries < policy.max_retries {
                let delay = noel_agent::retry_delay_ms(&policy, self.retries);
                self.retries += 1;
                // Saved before the retry is reported, so a crash the report
                // races is never taken for one during a request.
                if let Some(store) = &runtime.store {
                    store.append(&SessionRecord::RetryWait {
                        run_id: run.run_id.clone(),
                        attempt: self.retries,
                    })?;
                }
                on_event(&TurnEvent::RetryStarted(RetryNotice {
                    attempt: self.retries,
                    max_attempts: policy.max_retries,
                    delay_ms: delay,
                    error_message: error.to_string(),
                }));
                progress.dropped(progress::Dropped::Retry)?;
                let backoff = run.retry.backoff(Duration::from_millis(delay));
                if run.until_cancelled(backoff).await? {
                    return Ok(());
                }
                on_event(&TurnEvent::RetryFinished {
                    attempt: std::mem::take(&mut self.retries),
                    error: Some(RETRY_CANCELLED.into()),
                });
                return Err(error.into());
            }
        }
        self.finish_retries(Some(&error), on_event);
        Err(error.into())
    }

    /// Reports the end of the reply's retries, if it had any: with the
    /// reply, or with the `error` no retry follows.
    fn finish_retries(
        &mut self,
        error: Option<&ProviderError>,
        on_event: &mut impl FnMut(&TurnEvent),
    ) {
        let attempt = std::mem::take(&mut self.retries);
        if attempt > 0 {
            on_event(&TurnEvent::RetryFinished {
                attempt,
                error: error.map(ToString::to_string),
            });
        }
    }

    /// Whether an overflow can be answered by compaction: automatic
    /// compaction is on, there is something to summarize, and the reply
    /// before this one did not overflow too. Asking takes the one attempt.
    fn can_compact(&mut self, runtime: &Runtime) -> bool {
        let can = !self.overflowed && runtime.compaction_policy().enabled && runtime.can_compact();
        self.overflowed |= can;
        can
    }

    /// Compacts after an overflow; whether it succeeded, so that asking
    /// again has a smaller context.
    async fn compact(
        &mut self,
        runtime: &mut Runtime,
        provider: &dyn Provider,
        run: &RunControl,
        on_event: &mut (impl FnMut(&TurnEvent) + Send),
    ) -> Result<bool, AgentError> {
        on_event(&TurnEvent::CompactionStarted);
        // Like any compaction, this one saves its checkpoint only once
        // complete: cancelled or failed, it leaves the history untouched.
        let compaction = runtime.compact_after_overflow(provider);
        match run.until_cancelled(compaction).await? {
            Ok(record) => {
                on_event(&TurnEvent::HistoryRewritten(
                    runtime.session.messages.clone(),
                ));
                on_event(&TurnEvent::CompactionFinished(record));
                Ok(true)
            }
            Err(error) => {
                on_event(&TurnEvent::CompactionFailed(error.to_string()));
                Ok(false)
            }
        }
    }
}

/// Runs one tool call of a reply, saving and reporting its result. A call
/// of a reply cut off at the output limit (`truncated`) is not run: its
/// saved and reported result is the error asking to re-issue it.
pub(crate) async fn run_tool_call(
    runtime: &mut Runtime,
    provider: &dyn Provider,
    run: &RunControl,
    tool_call: ChatToolCall,
    truncated: bool,
    on_event: &mut (impl FnMut(&TurnEvent) + Send),
) -> Result<(), AgentError> {
    if run.cancellation.is_cancelled() {
        return Err(AgentError::Cancelled);
    }
    on_event(&TurnEvent::ToolStarted {
        call: tool_call.clone(),
    });
    let started = Instant::now();
    let result = if truncated {
        fail_truncated_tool_call(runtime, &tool_call)
    } else {
        // The user's pre-hooks (spec §9.5) run before anything else: an
        // exit-2 hook blocks the call, its stderr the reason, exactly as
        // the extension gate blocks. A failing hook is a notice, not a
        // block (Claude's non-blocking error).
        if !truncated {
            let (decision, notices) =
                crate::hooks::run_pre_hooks(runtime, &tool_call.name, &tool_call.arguments).await;
            for notice in &notices {
                on_event(&TurnEvent::Notice(notice.clone()));
            }
            if let crate::hooks::HookDecision::Block(reason) = decision {
                runtime.push_tool_result(&tool_call, reason.clone(), Vec::new(), true)?;
                on_event(&TurnEvent::ToolOutput {
                    call: tool_call.clone(),
                    output: reason,
                    images: Vec::new(),
                    is_error: true,
                    duration: Some(started.elapsed()),
                });
                on_event(&TurnEvent::ToolFinished {
                    name: tool_call.name.clone(),
                    is_error: true,
                });
                return Ok(());
            }
        }
        // The `toolCall` gate of any gate-registered v2 extension runs
        // first, fail closed: a blocked call never runs and its reason is
        // the model's error result (upstream's permission gates).
        if !truncated {
            let gate = {
                let hosts = runtime.extension_hosts();
                let mut hosts = hosts.lock().await;
                hosts
                    .gate_tool_call(&tool_call.name, &tool_call.arguments, &runtime.session.cwd)
                    .await
            };
            if let Ok(decision) = &gate
                && !decision.allow
            {
                runtime.push_tool_result(&tool_call, decision.reason.clone(), Vec::new(), true)?;
                on_event(&TurnEvent::ToolOutput {
                    call: tool_call.clone(),
                    output: decision.reason.clone(),
                    images: Vec::new(),
                    is_error: true,
                    duration: Some(started.elapsed()),
                });
                on_event(&TurnEvent::ToolFinished {
                    name: tool_call.name.clone(),
                    is_error: true,
                });
                return Ok(());
            }
        }
        // What the call prints, saved as it goes (spec §4.2).
        let progress = Mutex::new(ToolProgress::new(
            runtime.store.clone(),
            run.run_id.clone(),
            tool_call.id.clone(),
            runtime.systems.progress.output_interval,
        ));
        let put_off = tokio::sync::Notify::new();
        // Both callbacks report events while the call runs.
        let events = Mutex::new(&mut *on_event);
        let mut on_output = |output: OutputSoFar<'_>| {
            (lock(&events))(&TurnEvent::ToolUpdate {
                call: tool_call.clone(),
                output: output.text.to_owned(),
            });
            if lock(&progress).observe(output, tokio::time::Instant::now()) {
                put_off.notify_one();
            }
        };
        let mut on_nested = |calls: Vec<NestedToolCall>| {
            (lock(&events))(&TurnEvent::NestedToolCalls {
                call: tool_call.clone(),
                calls,
            });
        };
        // A codemode script ends itself on an abort, with upstream's
        // `Script aborted` result: it is not dropped.
        let ends_itself = tool_call.name == codemode::CODEMODE;
        let execution = execute_model_tool_call_with(
            runtime,
            provider,
            &tool_call,
            &mut on_output,
            &mut on_nested,
            Some(&run.cancellation),
        );
        let execution = async {
            tokio::pin!(execution);
            loop {
                let due = lock(&progress).due();
                tokio::select! {
                    biased;
                    result = &mut execution => break result,
                    () = put_off.notified() => {}
                    () = sleep_until(due) => lock(&progress).save_due(tokio::time::Instant::now()),
                }
            }
        };
        let result = if ends_itself {
            Ok(execution.await)
        } else {
            run.until_cancelled(execution).await
        };
        let mut progress = progress
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(error) = progress.take_error() {
            return Err(error.into());
        }
        match result {
            Ok(result) => result,
            Err(cancelled) => {
                // An abort: the result is what the call had printed.
                let output = progress::aborted_output(progress.output().0);
                runtime.push_tool_result(&tool_call, output.clone(), Vec::new(), true)?;
                on_event(&TurnEvent::ToolOutput {
                    call: tool_call.clone(),
                    output,
                    images: Vec::new(),
                    is_error: true,
                    duration: None,
                });
                on_event(&TurnEvent::ToolFinished {
                    name: tool_call.name,
                    is_error: true,
                });
                return Err(cancelled);
            }
        }
    };
    let duration = (!truncated).then(|| started.elapsed());
    let event = runtime
        .session
        .tool_history
        .last()
        .filter(|event| event.id == tool_call.id);
    let is_error = result.is_err() || event.is_some_and(|event| event.is_error);
    if let Some(event) = event {
        let images = runtime
            .session
            .messages
            .last()
            .filter(|message| message.tool_call_id.as_deref() == Some(&tool_call.id))
            .map(|message| message.media.clone())
            .unwrap_or_default();
        on_event(&TurnEvent::ToolOutput {
            call: tool_call.clone(),
            output: event.result.clone(),
            images,
            is_error,
            duration,
        });
    }
    if tool_call.name == "todo" {
        on_event(&TurnEvent::TodosUpdated(runtime.session.todos.clone()));
    }
    // The user's post-hooks (spec §9.5): after the result is saved and
    // reported. Output ignored; a non-zero exit is a notice.
    if !truncated && result.is_ok() {
        for notice in
            crate::hooks::run_post_hooks(runtime, &tool_call.name, &tool_call.arguments).await
        {
            on_event(&TurnEvent::Notice(notice));
        }
    }
    // An aborted codemode call saved its own result; the turn still ends.
    let aborted = tool_call.name == codemode::CODEMODE && run.cancellation.is_cancelled();
    on_event(&TurnEvent::ToolFinished {
        name: tool_call.name,
        is_error,
    });
    if aborted {
        return Err(AgentError::Cancelled);
    }
    result
}

pub(crate) fn user_message(prompt: String, media: Vec<MediaInput>) -> ConversationMessage {
    ConversationMessage {
        thinking: String::new(),
        role: MessageRole::User,
        content: prompt,
        media,
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    }
}

/// Pending steering as one user message: a burst reads as one instruction
/// (joined with [`STEERING_JOIN`]), not several interruptions. With it come
/// the queued submissions it places.
pub(crate) fn steering_message(
    steering: &SteeringMailbox,
) -> Option<(ConversationMessage, Vec<QueueItemId>)> {
    let (messages, placed) = steering.take_batch();
    (!messages.is_empty()).then(|| {
        let message = user_message(messages.join(STEERING_JOIN), Vec::new());
        (message, placed)
    })
}

/// Saves `message` to the session and reports it.
pub(crate) fn add_message(
    runtime: &mut Runtime,
    message: ConversationMessage,
    on_event: &mut impl FnMut(&TurnEvent),
) -> Result<(), AgentError> {
    add_placed_message(runtime, message, Vec::new(), on_event)
}

/// Saves `message` with the queued submissions it places, and reports it.
pub(crate) fn add_placed_message(
    runtime: &mut Runtime,
    message: ConversationMessage,
    placed: Vec<QueueItemId>,
    on_event: &mut impl FnMut(&TurnEvent),
) -> Result<(), AgentError> {
    runtime.push_placed_message(message.clone(), placed)?;
    on_event(&TurnEvent::MessageAdded(message));
    Ok(())
}

/// Requests one reply, reporting its stream events as they arrive and
/// saving its text and thinking as `progress` allows. A request that fails
/// leaves the events of its broken reply reported, and `progress` holding
/// what it streamed.
pub(crate) async fn request_reply(
    provider: &dyn Provider,
    request: ProviderRequest,
    run: &RunControl,
    progress: &Mutex<ReplyProgress>,
    on_event: &mut (impl FnMut(&TurnEvent) + Send),
) -> Result<Vec<StreamEvent>, AgentError> {
    let put_off = tokio::sync::Notify::new();
    let report = |event: &StreamEvent| {
        on_event(&TurnEvent::Provider(event.clone()));
        if lock(progress).observe(event, tokio::time::Instant::now()) {
            put_off.notify_one();
        }
    };
    let reply = noel_agent::complete_once(provider, request, &run.cancellation, report);
    tokio::pin!(reply);
    // Saves what streamed meanwhile once a save put off is due.
    let reply = loop {
        let due = lock(progress).due();
        tokio::select! {
            biased;
            reply = &mut reply => break reply,
            () = put_off.notified() => {}
            () = sleep_until(due) => lock(progress).save_due(tokio::time::Instant::now()),
        }
    };
    if let Some(error) = lock(progress).take_error() {
        return Err(error.into());
    }
    reply.map_err(AgentError::from)
}

/// Ends a turn whose reply an abort or a failure cut off: what the reply
/// had streamed is saved as an aborted reply (spec §4.2), then the turn
/// ends with `error`, or with the error saving it.
pub(crate) fn abort_reply(
    runtime: &mut Runtime,
    progress: &mut ReplyProgress,
    error: AgentError,
) -> AgentError {
    match progress
        .aborted()
        .map(|message| runtime.push_message(message))
    {
        Some(Err(save)) => save.into(),
        _ => error,
    }
}

/// Sleeps until `deadline`, or for ever without one.
pub(crate) async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
