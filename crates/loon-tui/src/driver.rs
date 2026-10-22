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
    /// The exact prompt this job is running, so cancelling it can hand the text back
    /// to the input bar instead of losing it.
    prompt: Option<String>,
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
) -> Job {
    let is_turn = matches!(
        &command,
        loon_core::commands::SlashCommand::Skill { .. }
            | loon_core::commands::SlashCommand::UltraCode { .. }
            | loon_core::commands::SlashCommand::UltraLoop { .. }
    );
    Job {
        turn: is_turn,
        prompt: None,
        task: tokio::spawn(async move {
            let mut agent = agent.lock().await;
            let output = agent.run_slash(command).await;
            let _ = results
                .send(output.unwrap_or_else(|e| format!("Error: {e}")))
                .await;
        }),
    }
}

fn publish(queue: &PromptQueue, notices: &mpsc::Sender<UiNotice>) {
    let _ = notices.try_send(UiNotice::Queue(queue.snapshot()));
}

/// Hard stop: signal the lock-free cancel handle and abort the task. Aborting drops
/// the tokio Mutex guard, so the next turn can take the agent immediately. Returns
/// true when a synthetic `TurnFinished(Cancelled)` is needed (the task had not
/// already finished and emitted its own), plus the prompt that was cancelled so the
/// UI can put it back in the input bar.
async fn stop(running: &mut Option<Job>, cancel: &watch::Sender<bool>) -> (bool, Option<String>) {
    let Some(mut job) = running.take() else {
        return (false, None);
    };
    let _ = cancel.send(true);
    let finished = job.task.is_finished();
    if !finished {
        job.task.abort();
    }
    let _ = (&mut job.task).await;
    // Only an aborted turn hands its prompt back: a turn that had already finished
    // produced its answer, so its text is history, not a draft. Slash commands never
    // own prompt text.
    let restore = if job.turn && !finished {
        job.prompt.take()
    } else {
        None
    };
    (job.turn && !finished, restore)
}

pub async fn run(
    agent: Arc<Mutex<Agent>>,
    mut commands: mpsc::Receiver<UiCommand>,
    events: mpsc::Sender<AgentEvent>,
    notices: mpsc::Sender<UiNotice>,
    results: mpsc::Sender<String>,
    loops: Arc<loon_core::loops::LoopRegistry>,
) {
    let (mut cancel, mut live_worklog) = {
        let a = agent.lock().await;
        (a.cancel_handle(), a.worklog.clone())
    };
    let mut queue = PromptQueue::new(16);
    publish(&queue, &notices);
    let mut running: Option<Job> = None;
    let mut settings_queue = std::collections::VecDeque::new();
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(250));
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
            }
            if let Some(command) = settings_queue.pop_front() {
                running = Some(slash_job(agent.clone(), command, results.clone()));
            } else if !queue.is_empty() {
                // All prompts typed while the turn ran ship with the NEXT request, not
                // one per turn boundary. Order is preserved, newest last.
                let combined = queue.drain_all().join("\n\n");
                publish(&queue, &notices);
                running = Some(Job {
                    task: turn(agent.clone(), combined.clone()),
                    turn: true,
                    prompt: Some(combined),
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
                        let (finished, prompt) = stop(&mut running, &cancel).await;
                        // Forget the cancelled turn in the agent too: its user message and
                        // anything it journaled are dropped, so the prompt is not left in
                        // the conversation and a resume will not replay it.
                        let mut forgotten = None;
                        if finished {
                            forgotten = agent.lock().await.forget_cancelled_turn();
                        }
                        if finished {
                            let _=events.send(AgentEvent::TurnFinished{status:TurnStatus::Cancelled}).await;
                        }
                        // Hand the prompt back to the input bar; prefer the text the agent
                        // actually recorded, falling back to what this job was running.
                        if let Some(text) = forgotten.or(prompt) {
                            let _ = notices.send(UiNotice::RestorePrompt(text)).await;
                        }
                    }
                    // Send now: the old turn is aborted and this prompt runs as the
                    // next turn; everything already queued folds into the same request.
                    // Cancel the running turn and take over: the typed prompt runs now,
                    // with everything already queued appended. An empty prompt just
                    // stops the turn so the queue can drain.
                    UiCommand::SendNow(text)=>{
                        let (finished, previous) = stop(&mut running, &cancel).await;
                        if finished {
                            let _=events.send(AgentEvent::TurnFinished{status:TurnStatus::Cancelled}).await;
                        }
                        // A replaced in-flight prompt is still the user's text: keep it.
                        if let Some(kept) = previous {
                            let _ = notices.send(UiNotice::RestorePrompt(kept)).await;
                        }
                        cancel=agent.lock().await.cancel_handle();
                        if text.trim().is_empty() {
                            running=None;
                        } else {
                            let mut prompts = queue.drain_all();
                            prompts.insert(0, text);
                            let combined = prompts.join("\n\n");
                            publish(&queue,&notices);
                            running=Some(Job{task:turn(agent.clone(),combined.clone()),turn:true,prompt:Some(combined)});
                        }
                    }
                    UiCommand::TakeLastQueued=>{
                        queue.remove_last();
                        publish(&queue, &notices);
                    }
                    UiCommand::Submit(text)=>{
                        if running.is_none() {
                            running=Some(Job{task:turn(agent.clone(),text.clone()),turn:true,prompt:Some(text)});
                        } else {match queue.push(text) {
                            Ok(n)=>{publish(&queue,&notices);let _=results.send(format!("queued {n} prompt(s); current operation is still running")).await;},
                            Err(e)=>{let _=results.send(e).await;}
                        }}
                    }
                    UiCommand::CycleMode=>{
                        if running.is_some(){let _=results.send("change permissions after the current operation finishes".into()).await;continue;}
                        let mut a=agent.lock().await;
                        let mode=Agent::next_permission_mode(a.cfg.permissions.mode);a.set_permission_mode(mode);
                        let _=events.send(AgentEvent::ModeChanged {mode:format!("{mode:?}").to_lowercase()}).await;
                    }
                    UiCommand::Slash(command)=>{
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
                        let cmd = command.clone();
                        // Settings that only touch the next request's context go live
                        // immediately: they ride the agent's lock-free queue and apply at
                        // the next request boundary instead of after the turn ends.
                        {
                            let guard = agent.lock().await;
                            let live_tx = guard.live_command_sender();
                            let live = matches!(&cmd,
                                loon_core::commands::SlashCommand::Effort { .. }
                                | loon_core::commands::SlashCommand::AutoCompact { .. }
                                | loon_core::commands::SlashCommand::Context { .. }
                                | loon_core::commands::SlashCommand::Config { .. });
                            if running.is_some() && live {
                                drop(guard);
                                let _ = live_tx.send(cmd).await;
                                let _ = results.send("applies to the next request".into()).await;
                                continue;
                            }
                        }
                        running=Some(slash_job(agent.clone(),command,results.clone()));
                    }
                }
            }
            _=timer.tick()=>{
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
