//! One operation at a time, with completion as a first-class wakeup (never polled only after input).
use crate::queue::PromptQueue;
use crate::{UiCommand, UiNotice};
use loon_core::{Agent, AgentEvent, TurnStatus};
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

struct Job {
    task: JoinHandle<()>,
    /// False for slash commands: only a cancelled turn gets a synthetic finish.
    turn: bool,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn turn(agent: Arc<Mutex<Agent>>, text: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        agent.lock().await.run_turn(&text).await;
    })
}

fn slash_job(
    agent: Arc<Mutex<Agent>>,
    command: loon_core::commands::SlashCommand,
    results: mpsc::Sender<String>,
    events: mpsc::Sender<AgentEvent>,
) -> Job {
    let is_turn = matches!(
        &command,
        loon_core::commands::SlashCommand::Skill { .. }
            | loon_core::commands::SlashCommand::UltraCode { .. }
            | loon_core::commands::SlashCommand::UltraLoop { .. }
    );
    let operation = matches!(
        &command,
        loon_core::commands::SlashCommand::Shell { .. }
            | loon_core::commands::SlashCommand::Monitors { .. }
            | loon_core::commands::SlashCommand::Jobs { .. }
            | loon_core::commands::SlashCommand::Compact
            | loon_core::commands::SlashCommand::Recap
    );
    Job {
        turn: is_turn || operation,
        task: tokio::spawn(async move {
            let mut agent = agent.lock().await;
            if operation {
                let _ = events.send(AgentEvent::TurnStarted).await;
            }
            let output = agent.run_slash(command).await;
            if operation {
                let status = output
                    .as_ref()
                    .map(|_| TurnStatus::Completed)
                    .unwrap_or_else(|e| TurnStatus::Failed(e.clone()));
                let _ = events.send(AgentEvent::TurnFinished { status }).await;
            }
            let _ = results
                .send(output.unwrap_or_else(|e| format!("Error: {e}")))
                .await;
        }),
    }
}

fn publish(queue: &PromptQueue, notices: &mpsc::Sender<UiNotice>) {
    let _ = notices.try_send(UiNotice::Queue(queue.snapshot()));
}

/// Stop drops the running future and its process guards. Conversation repair is
/// performed by the caller after the agent mutex has been released.
async fn stop(running: &mut Option<Job>, cancel: &watch::Sender<bool>) -> bool {
    let Some(mut job) = running.take() else {
        return false;
    };
    let _ = cancel.send(true);
    let finished = job.task.is_finished();
    if !finished {
        job.task.abort();
    }
    let _ = (&mut job.task).await;
    job.turn && !finished
}

pub async fn run(
    agent: Arc<Mutex<Agent>>,
    mut commands: mpsc::Receiver<UiCommand>,
    events: mpsc::Sender<AgentEvent>,
    notices: mpsc::Sender<UiNotice>,
    results: mpsc::Sender<String>,
    loops: Arc<loon_core::loops::LoopRegistry>,
) {
    let (mut cancel, mut live_worklog, mut live_tx, mut queue) = {
        let a = agent.lock().await;
        (
            a.cancel_handle(),
            a.worklog.clone(),
            a.live_command_sender(),
            a.inbox.clone(),
        )
    };
    publish(&queue, &notices);
    let mut running: Option<Job> = None;
    let mut queue_paused = false;
    let (mut status_tools, mut status_ctx, mut status_permissions) = {
        let a = agent.lock().await;
        (a.tools.clone(), a.ctx.clone(), a.cfg.permissions.clone())
    };
    let mut queries = tokio::task::JoinSet::new();
    let mut published = Vec::new();
    let mut settings_queue = std::collections::VecDeque::new();
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        if running
            .as_ref()
            .map(|j| j.task.is_finished())
            .unwrap_or(false)
        {
            if let Some(mut job) = running.take() {
                if let Err(e) = (&mut job.task).await {
                    let _ = results.send(format!("operation failed: {e}")).await;
                }
            }
        }
        if running.is_none() {
            {
                let a = agent.lock().await;
                cancel = a.cancel_handle();
                live_worklog = a.worklog.clone();
                live_tx = a.live_command_sender();
                queue = a.inbox.clone();
                status_tools = a.tools.clone();
                status_ctx = a.ctx.clone();
                status_permissions = a.cfg.permissions.clone();
            }
            if let Some(command) = settings_queue.pop_front() {
                running = Some(slash_job(
                    agent.clone(),
                    command,
                    results.clone(),
                    events.clone(),
                ));
            } else if !queue_paused && !queue.is_empty() {
                // All prompts typed while the turn ran ship with the NEXT request, not
                // one per turn boundary. Order is preserved, newest last.
                let prompts = queue.drain_all();
                let combined = prompts.join("\n\n");
                let _ = events
                    .send(AgentEvent::QueuedMessagesApplied { messages: prompts })
                    .await;
                publish(&queue, &notices);
                running = Some(Job {
                    task: turn(agent.clone(), combined.clone()),
                    turn: true,
                });
            }
        }
        tokio::select! {
            biased;
            done=async {
                match running.as_mut() {
                    Some(job)=>(&mut job.task).await,
                    None=>Ok(()),
                }
            },if running.is_some()=>{
                running=None;
                if let Err(e)=done {let _=results.send(format!("operation failed: {e}")).await;}
            }
            result = queries.join_next(), if !queries.is_empty() => {
                if let Some(Ok(output)) = result { let _ = results.send(output).await; }
            }
            command=commands.recv()=>{
                let Some(command)=command else{break;};
                // A TurnFinished event may have reached the UI a few instructions before
                // the task returned. Let that return complete before classifying a new prompt.
                tokio::task::yield_now().await;
                if running.as_ref().map(|j|j.task.is_finished()).unwrap_or(false) {
                    if let Some(mut job)=running.take(){let _=(&mut job.task).await;}
                    cancel=agent.lock().await.cancel_handle();
                }
                match command {
                    UiCommand::Cancel=>{
                        let finished = stop(&mut running, &cancel).await;
                        queue_paused = true;
                        let _ = notices.send(UiNotice::QueuePaused(true)).await;
                        if finished {
                            agent.lock().await.seal_cancelled_turn();
                            let _=events.send(AgentEvent::TurnFinished{status:TurnStatus::Cancelled}).await;
                        }
                    }
                    // Send now: the old turn is aborted and this prompt runs as the
                    // next turn; everything already queued folds into the same request.
                    // Cancel the running turn and take over: the typed prompt runs now,
                    // with everything already queued appended. An empty prompt just
                    // stops the turn so the queue can drain.
                    UiCommand::SendNow(text)=>{
                        let finished = stop(&mut running, &cancel).await;
                        queue_paused = false;
                        let _ = notices.send(UiNotice::QueuePaused(false)).await;
                        if finished {
                            let _=events.send(AgentEvent::TurnFinished{status:TurnStatus::Cancelled}).await;
                        }
                        if finished { agent.lock().await.seal_cancelled_turn(); }
                        cancel=agent.lock().await.cancel_handle();
                        if text.trim().is_empty() {
                            running=None;
                        } else {
                            let mut prompts = queue.drain_all();
                            prompts.push(text);
                            let combined = prompts.join("\n\n");
                            let _ = events.send(AgentEvent::QueuedMessagesApplied { messages: prompts }).await;
                            publish(&queue,&notices);
                            running=Some(Job{task:turn(agent.clone(),combined.clone()),turn:true});
                        }
                    }
                    UiCommand::TakeLastQueued=>{
                        if let Some(text) = queue.remove_last() { let _ = notices.send(UiNotice::Draft(text)).await; }
                        publish(&queue, &notices);
                    }
                    UiCommand::Submit(text)=>{
                        queue_paused = false;
                        let _ = notices.send(UiNotice::QueuePaused(false)).await;
                        if running.is_none() && queue.is_empty() {
                            let _ = events.send(AgentEvent::QueuedMessagesApplied { messages: vec![text.clone()] }).await;
                            running=Some(Job{task:turn(agent.clone(),text.clone()),turn:true});
                        } else {
                            let backup = text.clone();
                            match queue.push(text) {
                                Ok(_) => publish(&queue, &notices),
                                Err(e) => {
                                    let _ = notices.send(UiNotice::Draft(backup)).await;
                                    let _ = results.send(e).await;
                                }
                            }
                        }
                    }
                    UiCommand::CycleMode=>{
                        if running.is_some(){let _=results.send("change permissions after the current operation finishes".into()).await;continue;}
                        let mut a=agent.lock().await;
                        let mode=Agent::next_permission_mode(a.cfg.permissions.mode);a.set_permission_mode(mode);
                        let _=events.send(AgentEvent::ModeChanged {mode:format!("{mode:?}").to_lowercase()}).await;
                    }
                    UiCommand::Slash(command)=>{
                        if running.is_some() {
                            if let Some((name, args)) = observation(&command) {
                                if queries.len() >= 8 { let _ = results.send("too many pending status queries".into()).await; continue; }
                                let tool = status_tools.get(name);
                                let ctx = status_ctx.clone();
                                let permissions = status_permissions.clone();
                                queries.spawn(async move {
                                    let Some(tool) = tool else { return format!("{name} is unavailable"); };
                                    let policy = loon_core::permissions::PermissionEngine::new(&permissions, None);
                                    let decision = policy.evaluate_call(name, &args, ctx.ws.root(), false);
                                    if !decision.is_allowed() { return format!("status query refused by permissions: {decision:?}"); }
                                    if let Err(e) = loon_tools::validate_args(&tool.spec(), &args) { return e.to_string(); }
                                    match tokio::time::timeout(std::time::Duration::from_secs(10), tool.run(args, &ctx)).await {
                                        Ok(Ok(output)) => output.text,
                                        Ok(Err(e)) => e.to_string(),
                                        Err(_) => "status query timed out; the operation is still running".into(),
                                    }
                                });
                                continue;
                            }
                        }

                        let live = matches!(&command,
                            loon_core::commands::SlashCommand::Effort { .. }
                            | loon_core::commands::SlashCommand::AutoCompact { .. }
                            | loon_core::commands::SlashCommand::Context { size: Some(_) }
                            | loon_core::commands::SlashCommand::Model { reference: Some(_) }) || live_config(&command);
                        if running.is_some() && live {
                            let _ = live_tx.send(command).await;
                            let _ = results.send("applies to the next request".into()).await;
                            continue;
                        }
                        // /new and /resume are explicit session switches, also while busy.
                        if running.is_some() && matches!(&command, loon_core::commands::SlashCommand::New | loon_core::commands::SlashCommand::Resume { target: Some(_) }) {
                            let finished = stop(&mut running, &cancel).await;
                            if finished {
                                agent.lock().await.seal_cancelled_turn();
                                let _ = events.send(AgentEvent::TurnFinished { status: TurnStatus::Cancelled }).await;
                            }
                            queue.drain_all();
                            queue_paused = false;
                        let _ = notices.send(UiNotice::QueuePaused(false)).await;
                            publish(&queue, &notices);
                        }

                        let setting_key=match &command {loon_core::commands::SlashCommand::Config {args} if args.first().is_some_and(|s|s=="menu-set")=>args.get(1).cloned(),_=>None};
                        let browser_change=matches!(&command,loon_core::commands::SlashCommand::Config {args} if args.first().is_some_and(|s|s=="ui"));
                        if running.is_some() && browser_change { settings_queue.push_back(command);continue; }
                        if running.is_some() && setting_key.is_some() {
                            // Coalesce only queued writes to the same setting; the active write finishes normally.
                            settings_queue.retain(|pending| !matches!(pending,loon_core::commands::SlashCommand::Config {args} if args.get(1)==setting_key.as_ref()));
                            settings_queue.push_back(command);
                            continue;
                        }
                        if running.is_some() {
                            if let loon_core::commands::SlashCommand::UltraLoop {args}=&command {
                                let op=args.first().map(String::as_str).unwrap_or("status");
                                if ["off","stop","on","status"].contains(&op) {
                                    let state=if op=="status" {live_worklog.read()}else{live_worklog.update(|s|{s.ultraloop=op=="on";Ok(())})};
                                    match state {
                                        Ok(state)=>{let _=results.send(format!("ultraloop: {}",if state.ultraloop{"on"}else{"off"})).await;let _=events.send(AgentEvent::WorkStateChanged {state}).await;}
                                        Err(e)=>{let _=results.send(e.to_string()).await;}
                                    }
                                    continue;
                                }
                            }
                        }
                        if running.is_some(){let _=results.send("current operation is still running; Esc cancels it".into()).await;continue;}
                        running=Some(slash_job(agent.clone(),command,results.clone(),events.clone()));
                    }
                }
            }
            _=timer.tick()=>{
                let snapshot = queue.snapshot();
                if snapshot != published {
                    published = snapshot;
                    publish(&queue, &notices);
                }

                let mut changed=false;
                for (_,prompt) in loops.take_due(){
                    match queue.push(prompt){Ok(_)=>changed=true,Err(e)=>{let _=results.send(e).await;}}
                }
                if changed{publish(&queue,&notices);}
            }
        }
    }
    let _ = cancel.send(true);
    if let Some(mut job) = running {
        job.task.abort();
        let _ = (&mut job.task).await;
    }
    agent.lock().await.finish_session().await;
}

/// Only observation actions may bypass the busy agent lock. Commands that mutate
/// resources still go through the agent's ordinary permission and journal path.
fn observation(
    command: &loon_core::commands::SlashCommand,
) -> Option<(&'static str, serde_json::Value)> {
    use loon_core::commands::SlashCommand;
    use serde_json::json;
    let (name, args, key) = match command {
        SlashCommand::Shell { args } => ("shell_session", args, "name"),
        SlashCommand::Monitors { args } => ("monitor", args, "monitor_id"),
        SlashCommand::Jobs { args } => ("process", args, "job_id"),
        _ => return None,
    };
    let action = args.first().map(String::as_str).unwrap_or("list");
    let allowed = match name {
        "shell_session" => ["list", "cwd"].contains(&action),
        "monitor" => ["list", "logs", "poll"].contains(&action),
        _ => ["list", "poll"].contains(&action),
    };
    if !allowed {
        return None;
    }
    let mut value = json!({"action": action});
    if action != "list" {
        value[key] = if key == "name" {
            json!(args.get(1).map(String::as_str).unwrap_or("main"))
        } else {
            json!(args.get(1)?.parse::<u64>().ok()?)
        };
    }
    Some((name, value))
}

fn live_config(command: &loon_core::commands::SlashCommand) -> bool {
    let loon_core::commands::SlashCommand::Config { args } = command else {
        return false;
    };
    let index = usize::from(
        args.first()
            .is_some_and(|a| matches!(a.as_str(), "set" | "menu-set")),
    );
    let Some(key) = args.get(index) else {
        return false;
    };
    let key = key.split('=').next().unwrap_or("");
    matches!(
        key,
        "effort"
            | "context_window"
            | "autocompact"
            | "autocompact_tokens"
            | "autocompact_percent"
            | "permission_mode"
            | "tool_output_bytes"
            | "max_retries"
            | "ultra_rounds"
            | "plan_mode"
            | "session_goal"
    )
}
