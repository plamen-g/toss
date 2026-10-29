//! `toss mcp`: lets an AI assistant (Claude, Cursor, ...) send files to your paired devices, through
//! the Model Context Protocol on stdin and stdout. It can list devices and send; pairing, removing
//! devices and settings stay with you, and the other device still accepts each transfer.

use std::{future::Future, path::PathBuf, sync::Mutex};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ProgressNotificationParam, ProgressToken},
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use tokio::sync::mpsc;

use crate::{Answer, Event, Ui, human};

pub async fn serve() -> anyhow::Result<()> {
    Toss.serve(stdio()).await?.waiting().await?;
    Ok(())
}

#[derive(Clone)]
struct Toss;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SendArgs {
    /// The paired device to send to, as list_devices names it.
    device: String,
    /// Absolute paths of the files and folders to send.
    paths: Vec<PathBuf>,
}

#[tool_router]
impl Toss {
    #[tool(
        description = "List the devices paired with toss. send_files takes these names.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn list_devices(&self) -> Result<CallToolResult, McpError> {
        Ok(match crate::home().and_then(|home| crate::load_contacts(&home)) {
            Ok(contacts) if contacts.is_empty() => text("No paired devices yet. The user pairs one in the toss window (Add device) or with `toss invite`."),
            Ok(contacts) => {
                let names: Vec<String> = contacts
                    .iter()
                    .map(|(name, c)| if c.auto_accept { format!("{name} (takes files from here without asking)") } else { name.clone() })
                    .collect();
                text(&names.join("\n"))
            }
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }

    #[tool(
        description = "Send files or folders to a paired device, end-to-end encrypted. Someone on the device accepts them first, \
                       unless it takes files from here without asking. Only send what the user asked to send.",
        annotations(destructive_hint = false)
    )]
    async fn send_files(&self, Parameters(args): Parameters<SendArgs>, context: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let (tx, mut rx) = mpsc::unbounded_channel::<ProgressNotificationParam>();
        let peer = context.peer.clone();
        // One task passes progress on in order, and only rising values, as the protocol requires.
        let forward = tokio::spawn(async move {
            let mut last = -1.0;
            while let Some(p) = rx.recv().await {
                if p.progress > last {
                    last = p.progress;
                    let _ = peer.notify_progress(p).await;
                }
            }
        });
        let report = Report { token: context.meta.get_progress_token(), progress: tx, sent: Mutex::default() };
        let res = tokio::select! {
            res = crate::send(&args.device, &args.paths, &report) => res,
            // What got across stays on their side, so sending again resumes.
            () = context.ct.cancelled() => Err(anyhow::anyhow!("cancelled")),
        };
        let sent = report.sent.lock().expect("not poisoned").take();
        drop(report);
        let _ = forward.await; // progress first, then the result
        Ok(match res {
            Ok(()) => text(&sent.unwrap_or_default()),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }
}

#[tool_handler(name = "toss", instructions = "toss sends files to the user's own devices and friends, peer to peer and end-to-end \
                               encrypted. Use list_devices for the names, and only send files the user asked to send.")]
impl ServerHandler for Toss {}

fn text(s: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

/// What a send tells the assistant: progress, when it asked for it, and how it ended.
struct Report {
    token: Option<ProgressToken>,
    progress: mpsc::UnboundedSender<ProgressNotificationParam>,
    sent: Mutex<Option<String>>,
}

impl Report {
    fn notify(&self, done: u64, total: Option<u64>, message: String) {
        let Some(token) = &self.token else { return };
        let mut p = ProgressNotificationParam::new(token.clone(), done as f64).with_message(message);
        if let Some(total) = total {
            p = p.with_total(total as f64);
        }
        let _ = self.progress.send(p);
    }
}

impl Ui for Report {
    fn event(&self, event: Event) {
        if matches!(event, Event::Sent { .. }) {
            *self.sent.lock().expect("not poisoned") = Some(event.line());
        } else {
            self.notify(0, None, event.line()); // waiting for them to accept
        }
    }

    fn progress(&self, done: u64, total: u64) {
        self.notify(done, Some(total), format!("{} / {}", human(done), human(total)));
    }

    fn accept(&self, _: &str, _: &[(String, u64)], _: impl Future<Output = ()> + Send) -> impl Future<Output = Answer> + Send {
        async { Answer::No } // only receiving asks, and this never receives
    }
}
