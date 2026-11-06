//! Follows one session on the local OpenCode service the way Zed will, then
//! checks the client's copy against a fresh snapshot from the server.
//!
//! cargo run -p opencode_agent --example follow_session -- \
//!     [--session <id>] [--seconds <n>] [--drop-after <n>]
//!
//! Without `--session`, follows the first running top-level session.
//! `--drop-after` closes the event stream partway through and reconnects, to
//! exercise the resync path.

use anyhow::{Context as _, Result, bail};
use futures::{FutureExt as _, StreamExt as _};
use opencode_agent::{
    Activity, Applied, AssistantContent, Change, Message, MessageBody, OpenCodeClient,
    SessionTranscript, Snapshot, ToolState, read_registration, registration_path,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Options {
    session: Option<String>,
    seconds: u64,
    drop_after: Option<u64>,
}

fn parse_options() -> Result<Options> {
    let mut options = Options {
        session: None,
        seconds: 30,
        drop_after: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--session" => options.session = Some(value()?),
            "--seconds" => options.seconds = value()?.parse()?,
            "--drop-after" => options.drop_after = Some(value()?.parse()?),
            _ => bail!("unknown argument {arg}"),
        }
    }
    Ok(options)
}

fn main() -> Result<()> {
    let options = parse_options()?;
    smol::block_on(run(options))
}

async fn run(options: Options) -> Result<()> {
    let registration = read_registration(&registration_path())?
        .context("no OpenCode service registration; is the service running?")?;
    let http = Arc::new(reqwest_client::ReqwestClient::new());
    let client = OpenCodeClient::new(http, &registration)?;
    let info = client.info().await?;
    println!("connected to OpenCode {} (pid {:?})", info.version, info.pid);

    let session_id = match options.session {
        Some(session_id) => session_id,
        None => first_running_root_session(&client).await?,
    };
    let session = client.session(&session_id).await?;
    println!(
        "following {session_id}: {:?}",
        session.title.as_deref().unwrap_or("(untitled)")
    );

    let mut transcript = SessionTranscript::new(session_id.clone());
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let mut drop_at = options
        .drop_after
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let mut stats = Stats::default();

    while Instant::now() < deadline {
        // Subscribe before taking the snapshot: anything that happens while
        // the snapshot loads waits in the stream and is applied afterwards.
        let mut events = client.events().await?;
        resync(&client, &mut transcript, &mut stats).await?;

        let cycle_end = match drop_at {
            Some(drop_at) if drop_at < deadline => drop_at,
            _ => deadline,
        };
        loop {
            let remaining = cycle_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let next = futures::select_biased! {
                event = events.next().fuse() => event,
                _ = timer(remaining).fuse() => break,
            };
            let Some(event) = next else {
                println!("event stream ended; reconnecting");
                break;
            };
            let event = event?;
            stats.events += 1;
            match transcript.apply(&event) {
                Applied::Ignored => {}
                Applied::Changed(changes) => {
                    stats.applied += 1;
                    print_changes(&transcript, &event.kind, &changes);
                }
                Applied::NeedsResync(reason) => {
                    println!("{} asked for a resync: {reason:?}", event.kind);
                    resync(&client, &mut transcript, &mut stats).await?;
                }
            }
        }
        if drop_at.take().is_some() && Instant::now() < deadline {
            println!("-- dropping the event stream to test reconnect --");
            stats.reconnects += 1;
        }
    }

    println!(
        "\n{} events received, {} changed the transcript, {} snapshots, {} reconnects",
        stats.events, stats.applied, stats.snapshots, stats.reconnects
    );
    verify(&client, &transcript).await
}

#[derive(Default)]
struct Stats {
    events: usize,
    applied: usize,
    snapshots: usize,
    reconnects: usize,
}

async fn first_running_root_session(client: &OpenCodeClient) -> Result<String> {
    for session_id in client.active_sessions().await?.into_keys() {
        if client.session(&session_id).await?.parent_id.is_none() {
            return Ok(session_id);
        }
    }
    bail!("no running top-level session; pass --session <id>")
}

async fn resync(
    client: &OpenCodeClient,
    transcript: &mut SessionTranscript,
    stats: &mut Stats,
) -> Result<()> {
    let running = client
        .active_sessions()
        .await?
        .contains_key(transcript.session_id());
    let session = client.session(transcript.session_id()).await?;
    let messages = client.all_messages(transcript.session_id()).await?;
    let diff = transcript.replace_snapshot(Snapshot {
        messages,
        running,
        revert: session.revert,
    });
    stats.snapshots += 1;
    println!(
        "snapshot: {} messages ({} visible), {} changed, {} removed, {}",
        transcript.messages().len(),
        transcript.visible_messages().len(),
        diff.changed.len(),
        diff.removed.len(),
        if running { "running" } else { "idle" }
    );
    Ok(())
}

async fn verify(client: &OpenCodeClient, transcript: &SessionTranscript) -> Result<()> {
    let server = client.all_messages(transcript.session_id()).await?;
    let server_revert = client
        .session(transcript.session_id())
        .await?
        .revert
        .map(|revert| revert.message_id);
    let mut problems = Vec::new();
    if server_revert.as_deref() != transcript.revert() {
        problems.push(format!(
            "revert point differs: client {:?}, server {server_revert:?}",
            transcript.revert()
        ));
    }
    for message in &server {
        let Some(local) = transcript
            .messages()
            .iter()
            .find(|local| local.id == message.id)
        else {
            // Created after our last event; a later event would have added it.
            if !is_after_last(transcript, message) {
                problems.push(format!("missing {}", message.id));
            }
            continue;
        };
        if is_in_progress(message) {
            continue;
        }
        if normalized(local) != normalized(message) {
            problems.push(format!(
                "differs {}\n  client: {:?}\n  server: {:?}",
                message.id,
                normalized(local),
                normalized(message)
            ));
        }
    }
    for local in transcript.messages() {
        if !server.iter().any(|message| message.id == local.id) {
            problems.push(format!("extra {}", local.id));
        }
    }
    if problems.is_empty() {
        println!(
            "PASS: client copy matches the server ({} messages)",
            server.len()
        );
        Ok(())
    } else {
        for problem in &problems {
            println!("FAIL: {problem}");
        }
        bail!("{} differences", problems.len())
    }
}

fn is_after_last(transcript: &SessionTranscript, message: &Message) -> bool {
    transcript
        .messages()
        .last()
        .is_some_and(|last| message.id > last.id)
}

fn is_in_progress(message: &Message) -> bool {
    message
        .as_assistant()
        .is_some_and(|assistant| assistant.time.completed.is_none())
}

/// Live-only tool progress isn't stored by the server.
fn normalized(message: &Message) -> Message {
    let mut message = message.clone();
    if let Some(assistant) = message.as_assistant_mut() {
        for item in &mut assistant.content {
            if let AssistantContent::Tool(tool) = item
                && let ToolState::Running { metadata, .. } = &mut tool.state
            {
                *metadata = serde_json::Value::Null;
            }
        }
    }
    message
}

fn print_changes(transcript: &SessionTranscript, kind: &str, changes: &[Change]) {
    for change in changes {
        match change {
            Change::Activity(Activity::Running) => println!("{kind}: ● running"),
            Change::Activity(Activity::Idle) => println!("{kind}: ○ idle"),
            Change::Title(title) => println!("{kind}: title {title:?}"),
            Change::Revert(Some(message_id)) => println!(
                "{kind}: reverted from {message_id}; {} of {} messages visible",
                transcript.visible_messages().len(),
                transcript.messages().len()
            ),
            Change::Revert(None) => println!(
                "{kind}: revert cleared; {} messages visible",
                transcript.visible_messages().len()
            ),
            Change::Message(id) => {
                if let Some(message) = transcript.messages().iter().find(|m| &m.id == id) {
                    println!("{kind}: {}", summarize(message));
                }
            }
        }
    }
}

fn summarize(message: &Message) -> String {
    let body = match &message.body {
        MessageBody::User(user) => format!("user {:?}", truncate(&user.text)),
        MessageBody::Synthetic(text) => format!("synthetic {:?}", truncate(&text.text)),
        MessageBody::Idle(idle) => format!("idle ({})", idle.outcome),
        MessageBody::Assistant(assistant) => {
            let items: Vec<String> = assistant
                .content
                .iter()
                .map(|item| match item {
                    AssistantContent::Text { text } => format!("text[{}]", text.len()),
                    AssistantContent::Reasoning { text, .. } => format!("thought[{}]", text.len()),
                    AssistantContent::Tool(tool) => {
                        format!("{}:{}", tool.name, tool.state.status())
                    }
                })
                .collect();
            format!("assistant [{}]", items.join(", "))
        }
        other => format!("{other:?}").chars().take(60).collect(),
    };
    format!("{} {body}", &message.id[..message.id.len().min(16)])
}

fn truncate(text: &str) -> String {
    text.chars().take(50).collect()
}

#[allow(clippy::disallowed_methods)]
async fn timer(duration: Duration) {
    smol::Timer::after(duration).await;
}
