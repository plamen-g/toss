//! `toss mcp`: lets an AI assistant (Claude, Cursor, ...) send files to your paired devices and your
//! phone, through the Model Context Protocol on stdin and stdout. It can list devices and send; pairing,
//! removing devices and settings stay with you, and the other device still accepts each transfer.
//!
//! What an assistant reads can steer it, so toss itself decides what may leave. Files in the folders
//! `toss mcp` was given, or that the user let through before, go at once. Anything else, and anything
//! that looks like a secret, waits for the user's yes, which toss asks for itself: in a window of its
//! own, or through the assistant's app on a computer without a screen. toss's own folder, with the
//! device key, never goes.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, ensure};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{tool::InputResponses, wrapper::Parameters},
    model::{
        CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock, CreateTaskResult, ElicitRequest,
        ElicitRequestParams, ElicitResult, ElicitationAction, ElicitationSchema, GetTaskParams, GetTaskResult,
        Implementation, InputRequest, InputRequiredResult, ProgressNotificationParam, ProgressToken,
        ProtocolVersion, ServerCapabilities, ServerConfig,
    },
    schemars,
    service::{RequestContext, ServerInitializeError},
    task_manager::{TaskContext, TaskExit, TaskManager, TaskOptions},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use tokio::{io::AsyncWriteExt as _, sync::mpsc};

use crate::{
    Answer, Event, Ui, count,
    gui::{Question, Said},
    human,
};

/// `folders`: where the assistant may send from without asking.
pub async fn serve(folders: &[&str]) -> anyhow::Result<()> {
    let mut allowed = Vec::new();
    for folder in folders.iter().filter(|f| !f.is_empty()) {
        // (an MCP bundle passes "" when its folder setting is left empty)
        let real = std::fs::canonicalize(folder).with_context(|| format!("can't find {folder}"))?;
        ensure!(real.is_dir(), "{folder} isn't a folder");
        allowed.push(real);
    }
    let home = std::fs::canonicalize(crate::home()?)?;
    let toss = Toss { folders: Arc::new(allowed), home: Arc::new(home), tasks: TaskManager::new() };
    match toss.serve(stdio()).await {
        Ok(running) => drop(running.waiting().await?),
        Err(ServerInitializeError::ConnectionClosed(_)) => {} // the assistant left without asking for anything
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[derive(Clone)]
struct Toss {
    folders: Arc<Vec<PathBuf>>, // the ones `toss mcp` was started with
    home: Arc<PathBuf>,
    tasks: TaskManager,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SendArgs {
    /// The paired device to send to, as list_devices names it.
    device: String,
    /// Absolute paths of the files and folders to send.
    paths: Vec<PathBuf>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct PhoneArgs {
    /// Absolute paths of the files to offer (files, not folders).
    paths: Vec<PathBuf>,
}

/// The key the user's answer comes back under, on protocols where the client asks for toss.
const CONFIRM: &str = "confirm";
/// Long enough to wait overnight for someone to accept (the SDK's default fails a task after 5 minutes).
const TASK_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const NOT_APPROVED: &str = "The user didn't approve sending these files.";

#[tool_router]
impl Toss {
    #[tool(
        description = "List the devices paired with toss. send_files takes these names.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn list_devices(&self) -> Result<CallToolResult, McpError> {
        Ok(match crate::load_contacts(&self.home) {
            Ok(contacts) if contacts.is_empty() => text("No paired devices yet. The user pairs one in the toss window (Add device) or with `toss invite`."),
            // Names only: whether a device takes files without asking is set on that device, out of sight from here.
            Ok(contacts) => text(&contacts.into_keys().collect::<Vec<_>>().join("\n")),
            Err(e) => failed(format!("{e:#}")),
        })
    }

    #[tool(
        description = "List what arrived on this computer through toss lately, newest first: files from paired devices \
                       and from the phone page, with where they were saved. Open them with your own file tools.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn received_files(&self) -> Result<CallToolResult, McpError> {
        let log = std::fs::read_to_string(self.home.join("received.jsonl")).unwrap_or_default();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let batches: Vec<String> = log
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<crate::Arrival>(line).ok())
            .take(20)
            .map(|a| {
                let files: Vec<String> = a.files.iter().map(|f| f.display().to_string()).collect();
                format!("{} from {}:\n{}", ago(now.saturating_sub(a.at)), a.from, files.join("\n"))
            })
            .collect();
        Ok(text(&if batches.is_empty() { "Nothing has arrived through toss yet.".to_string() } else { batches.join("\n\n") }))
    }

    #[tool(
        description = "Send files or folders to a paired device, end-to-end encrypted. Someone on the device accepts them first, \
                       unless it takes files from here without asking. Files from outside the folders the user lets you send from, \
                       and files that look like secrets, need the user's yes, which toss asks for. Only send what the user asked to send.",
        annotations(destructive_hint = false)
    )]
    async fn send_files(
        &self,
        Parameters(args): Parameters<SendArgs>,
        InputResponses(answers): InputResponses,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let files = match self.fence(&args.paths, Some(&args.device), answers, &context).await {
            Ok(files) => files,
            Err(stop) => return Ok(stop),
        };
        let (by, home) = (assistant(&context), self.home.to_path_buf());

        // A send can wait a long time for someone to accept: as a task the assistant checks on it
        // instead of waiting on one call that may time out.
        if context.client_capabilities().is_some_and(|c| c.supports_tasks()) {
            let task = self.tasks.spawn(TaskOptions::new().with_ttl_ms(TASK_TTL_MS), move |ctx| {
                Box::pin(async move {
                    let report = Report { sink: Sink::Task(ctx.clone()), sent: Mutex::default() };
                    tokio::select! {
                        res = deliver(&args.device, files, by, &home, &report) => Ok(report.result(res)),
                        () = ctx.cancelled() => Err(TaskExit::Cancelled), // what got across resumes next time
                    }
                })
            });
            return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
        }

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
        let report = Report { sink: Sink::Progress(context.meta.get_progress_token(), tx), sent: Mutex::default() };
        let res = tokio::select! {
            res = deliver(&args.device, files, by, &home, &report) => res,
            // What got across stays on their side, so sending again resumes.
            () = context.ct.cancelled() => Err(anyhow::anyhow!("cancelled")),
        };
        let result = report.result(res);
        drop(report);
        let _ = forward.await; // progress first, then the result
        Ok(result.into())
    }

    #[tool(
        description = "Offer files to the user's phone, with no app on the phone: toss's window shows a QR code, and the phone's \
                       camera opens a page that downloads them, over the same Wi-Fi as this computer. Files, not folders. As with \
                       send_files, only offer what the user asked for; toss asks the user first where needed.",
        annotations(destructive_hint = false)
    )]
    async fn show_on_phone(
        &self,
        Parameters(args): Parameters<PhoneArgs>,
        InputResponses(answers): InputResponses,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if let Some(folder) = args.paths.iter().find(|p| p.is_dir()) {
            return Ok(failed(format!("The phone page offers files, not folders: {}", folder.display())).into());
        }
        if let Err(stop) = self.fence(&args.paths, None, answers, &context).await {
            return Ok(stop);
        }
        Ok(match crate::gui::show_on_phone(&self.home, &args.paths) {
            Ok(()) => text(
                "toss's window shows a QR code now. The user scans it with the phone's camera, on the same Wi-Fi as this \
                 computer, and the phone's browser downloads the files.",
            ),
            Err(e) => failed(format!("{e:#}")),
        }
        .into())
    }
}

impl Toss {
    /// The folders an assistant may send from without asking: those `toss mcp` was started with, and
    /// those the user told toss's question not to ask about again.
    fn allowed(&self) -> Vec<PathBuf> {
        let mut allowed = self.folders.to_vec();
        allowed.extend(remembered(&self.home));
        allowed
    }

    /// toss's fence around what an assistant sends to `device` (None: the phone). Ok: exactly the files
    /// to send. Err: what the call returns instead, a refusal or, on the newest protocol, "ask the user,
    /// then call again".
    async fn fence(
        &self,
        paths: &[PathBuf],
        device: Option<&str>,
        answers: Option<rmcp::model::InputResponses>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Vec<(String, PathBuf, u64)>, CallToolResponse> {
        let stop = |why: String| CallToolResponse::from(failed(why));
        let located = locate(paths, &self.allowed(), &self.home).map_err(stop)?;
        let files = crate::gather(paths).map_err(|e| stop(format!("{e:#}")))?;
        let lines = question(paths, &located, &files);
        if lines.is_empty() {
            return Ok(files);
        }
        let question = match device {
            // Checked first, so toss's question never shows a device name the assistant made up.
            Some(device) if !crate::load_contacts(&self.home).is_ok_and(|c| c.contains_key(device)) => {
                return Err(stop(format!("no contact called {device}")));
            }
            Some(device) => format!("Send these to {device} with toss?"),
            None => "Offer these to your phone with toss? They go over this Wi-Fi, unencrypted.".into(),
        };
        let remember = rememberable(&located, files.iter().any(|(rel, ..)| looks_secret(rel)));
        let ask = Question {
            question,
            details: lines.join("\n"),
            yes: if device.is_some() { "Send" } else { "Offer" }.into(),
            remember: remember.as_deref().map(shown),
        };
        if self.approve(&ask, answers, context).await?
            && let Some(folder) = remember
        {
            let _ = remember_folder(&self.home, folder); // the send goes either way; this only saves asking next time
        }
        Ok(files)
    }

    /// Asks the user `ask`: in toss's own window where this computer has a screen, else through the
    /// assistant's app. Ok on a yes, with whether to remember the folder.
    async fn approve(
        &self,
        ask: &Question,
        answers: Option<rmcp::model::InputResponses>,
        context: &RequestContext<RoleServer>,
    ) -> Result<bool, CallToolResponse> {
        let said = |result: Option<ElicitResult>| match result {
            Some(r) if r.action == ElicitationAction::Accept => Ok(r.content.is_some_and(|c| c["remember"] == true)),
            _ => Err(CallToolResponse::from(failed(NOT_APPROVED.into()))),
        };
        if let Some(answers) = answers {
            // On the newest protocol the client asked already, and calls again with the answer.
            return said(answers.get(CONFIRM).and_then(|a| serde_json::from_value(a.clone()).ok()));
        }
        // A window no app can answer for the user. (TOSS_NO_WINDOW: tests and the eval answer as the app.)
        if std::env::var_os("TOSS_NO_WINDOW").is_none()
            && let Some(answer) = in_window(ask, context).await
        {
            return if answer.send { Ok(answer.remember) } else { Err(failed(NOT_APPROVED.into()).into()) };
        }
        if !context.client_capabilities().is_some_and(|c| c.elicitation.is_some()) {
            return Err(failed(format!(
                "This assistant can't show toss's question, and toss's own window couldn't open, so toss only sends from \
                 the folders the user lets it send from, and nothing that looks like a secret. It would have asked about:\n{}",
                ask.details
            ))
            .into());
        }
        let form = ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: format!("{}\n\n{}", ask.question, ask.details),
            requested_schema: form(ask),
        };
        if context.protocol_version().is_some_and(|v| v >= ProtocolVersion::V_2026_07_28) {
            // On this protocol the client asks, then calls again with the answer.
            let form = InputRequest::Elicitation(ElicitRequest::new(form));
            return Err(InputRequiredResult::from_input_requests(BTreeMap::from([(CONFIRM.to_string(), form)])).into());
        }
        match context.peer.create_elicitation(form).await {
            Ok(result) => said(Some(result)),
            Err(e) => Err(failed(format!("toss couldn't ask the user, so nothing was sent: {e}")).into()),
        }
    }
}

#[tool_handler]
impl ServerHandler for Toss {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tasks().build())
            .with_server_info(Implementation::new("toss", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "toss sends files to the user's own devices and friends, peer to peer and end-to-end encrypted, and offers \
                 files to the user's phone. Use list_devices for the names, and only send files the user asked to send.",
            )
    }

    async fn get_task(&self, request: GetTaskParams, _: RequestContext<RoleServer>) -> Result<GetTaskResult, McpError> {
        Ok(GetTaskResult::new(self.tasks.get_task(&request.task_id)?))
    }

    async fn cancel_task(&self, request: CancelTaskParams, _: RequestContext<RoleServer>) -> Result<(), McpError> {
        self.tasks.cancel_task(&request.task_id)
    }
}

/// Where each of `paths` really is (`..`, symlinks and junctions followed), and whether that's outside
/// the `allowed` folders. Anything in toss's own folder (`home`), or holding it, never goes: that folder
/// has the device key.
fn locate(paths: &[PathBuf], allowed: &[PathBuf], home: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    paths
        .iter()
        .map(|path| {
            let real = std::fs::canonicalize(path).map_err(|_| format!("can't read {}", path.display()))?;
            if real.starts_with(home) || home.starts_with(&real) {
                return Err(format!("toss won't send {}: its own folder, which holds your device key, would go with it", path.display()));
            }
            let outside = !allowed.iter().any(|dir| real.starts_with(dir));
            Ok((real, outside))
        })
        .collect()
}

/// The lines of toss's question, one for each item outside the allowed folders or holding something
/// that looks like a secret: where it really is (never the assistant's text: `..` or padding could
/// dress up a key as a project file), how much goes, and why toss asks. None: no question.
fn question(paths: &[PathBuf], located: &[(PathBuf, bool)], files: &[(String, PathBuf, u64)]) -> Vec<String> {
    paths
        .iter()
        .zip(located)
        .filter_map(|(path, (real, outside))| {
            let top = path.file_name()?.to_str()?; // as `gather` names it
            let inside = format!("{top}/");
            let its: Vec<_> = files.iter().filter(|(rel, ..)| rel == top || rel.starts_with(&inside)).collect();
            let secrets: Vec<&str> = its.iter().map(|(rel, ..)| rel.as_str()).filter(|rel| looks_secret(rel)).collect();
            if !outside && secrets.is_empty() {
                return None;
            }
            let size = human(its.iter().map(|f| f.2).sum());
            let mut line = format!("{} ({})", shown(real), if real.is_dir() { format!("{}, {size}", count(its.len())) } else { size });
            if *outside {
                line += "\n    outside the folders you let assistants send from";
            }
            if !secrets.is_empty() {
                let more = if secrets.len() > 3 { format!(" and {} more", secrets.len() - 3) } else { String::new() };
                line += &format!("\n    holds {}{more}, which look like keys, passwords or tokens", secrets[..secrets.len().min(3)].join(", "));
            }
            Some(line)
        })
        .collect()
}

/// Whether a file, by its path ('/'-separated), looks like it holds a secret: keys, tokens, passwords.
/// Those always need the user's yes, even inside the allowed folders.
fn looks_secret(rel: &str) -> bool {
    rel.split('/').map(str::to_lowercase).any(|part| {
        matches!(
            part.as_str(),
            ".ssh" | ".gnupg" | ".aws" | ".netrc" | ".npmrc" | ".pypirc" | ".git-credentials" | "id_rsa" | "id_ecdsa" | "id_ed25519" | "id_dsa" | "credentials.json"
        ) || part == ".env"
            || part.starts_with(".env.")
            || [".pem", ".key", ".p12", ".pfx", ".kdbx", ".keystore", ".jks"].iter().any(|ext| part.ends_with(ext))
    })
}

/// The folder "don't ask again" would let through: the one folder all the outside items are, or are in,
/// unless that's the home folder or a whole drive. Never when something looks like a secret: those always ask.
fn rememberable(located: &[(PathBuf, bool)], secrets: bool) -> Option<PathBuf> {
    if secrets {
        return None;
    }
    let mut folders = located.iter().filter(|(_, outside)| *outside).map(|(real, _)| if real.is_dir() { Some(real.as_path()) } else { real.parent() });
    let folder = folders.next()??;
    if !folders.all(|f| f == Some(folder)) || folder.parent().is_none() {
        return None;
    }
    let home = dirs::home_dir().and_then(|h| std::fs::canonicalize(h).ok());
    (home.as_deref() != Some(folder)).then(|| folder.to_path_buf())
}

/// The folders the user told toss's question not to ask about again, kept in toss's folder.
pub fn remembered(home: &Path) -> Vec<PathBuf> {
    std::fs::read(home.join("folders.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn remember_folder(home: &Path, folder: PathBuf) -> anyhow::Result<()> {
    let mut folders = remembered(home);
    if !folders.contains(&folder) {
        folders.push(folder);
    }
    crate::write_private(&home.join("folders.json"), &serde_json::to_vec_pretty(&folders)?)
}

/// Takes a folder off that list, the way `shown` writes it.
pub fn forget(home: &Path, folder: &str) -> anyhow::Result<()> {
    let folders: Vec<PathBuf> = remembered(home).into_iter().filter(|f| shown(f) != folder).collect();
    crate::write_private(&home.join("folders.json"), &serde_json::to_vec_pretty(&folders)?)
}

/// A path as people write it: without the `\\?\` that Windows puts in front of real paths.
pub fn shown(path: &Path) -> String {
    let path = path.display().to_string();
    path.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(path)
}

/// toss's own window (`toss ask`), where this computer has a screen: the user's answer, or None when no
/// window could open. If the call is called off, the window closes.
async fn in_window(ask: &Question, context: &RequestContext<RoleServer>) -> Option<Said> {
    let mut cmd = tokio::process::Command::new(std::env::current_exe().ok()?);
    cmd.arg("ask").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing up
    let mut child = cmd.spawn().ok()?;
    child.stdin.take()?.write_all(&serde_json::to_vec(ask).ok()?).await.ok()?; // closed after, so `toss ask` reads to the end
    let out = tokio::select! {
        out = child.wait_with_output() => out.ok()?,
        () = context.ct.cancelled() => return Some(Said { send: false, remember: false }),
    };
    serde_json::from_slice(&out.stdout).ok()
}

/// The question for the assistant's app: a yes or no, plus "don't ask again" when there's a folder for it.
fn form(ask: &Question) -> ElicitationSchema {
    match &ask.remember {
        Some(folder) => ElicitationSchema::builder()
            .optional_bool_with("remember", |b| b.title(format!("Don't ask again for {folder}")).with_default(false))
            .build_unchecked(),
        None => ElicitationSchema::new(BTreeMap::new()),
    }
}

/// The assistant's app, by the name it gave when it connected.
fn assistant(context: &RequestContext<RoleServer>) -> String {
    context.peer.peer_info().map_or("An assistant".into(), |info| info.client_info.name.clone())
}

/// Sends `files` to `device`. Once they're across, notes it in toss's folder, where the toss window shows it.
async fn deliver(device: &str, files: Vec<(String, PathBuf, u64)>, by: String, home: &Path, report: &Report) -> anyhow::Result<()> {
    let local: Vec<PathBuf> = files.iter().map(|f| f.1.clone()).collect();
    crate::send_list(device, files, report).await?;
    let at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let _ = crate::append(&home.join("sent.jsonl"), &crate::AssistantSent { at, by, to: device.into(), files: local }); // only a note
    Ok(())
}

/// "just now", "5 min ago", "3 h ago", "2 days ago"
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

fn text(s: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn failed(why: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(why)])
}

/// Where a running send reports to: progress notifications on a plain call (when the assistant asked
/// for them), or its task's status.
enum Sink {
    Progress(Option<ProgressToken>, mpsc::UnboundedSender<ProgressNotificationParam>),
    Task(TaskContext),
}

/// What a send tells the assistant: how it's going, and how it ended.
struct Report {
    sink: Sink,
    sent: Mutex<Option<String>>,
}

impl Report {
    fn notify(&self, done: u64, total: Option<u64>, message: String) {
        match &self.sink {
            Sink::Progress(Some(token), tx) => {
                let mut p = ProgressNotificationParam::new(token.clone(), done as f64).with_message(message);
                if let Some(total) = total {
                    p = p.with_total(total as f64);
                }
                let _ = tx.send(p);
            }
            Sink::Progress(None, _) => {}
            Sink::Task(ctx) => ctx.set_status_message(message),
        }
    }

    /// The call's result: what was sent, or why not.
    fn result(&self, res: anyhow::Result<()>) -> CallToolResult {
        match res {
            Ok(()) => text(&self.sent.lock().expect("not poisoned").take().unwrap_or_default()),
            Err(e) => failed(format!("{e:#}")),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_allowed_folders_go_without_asking() {
        let root = std::env::temp_dir().join(format!("toss-consent-test-{}", std::process::id()));
        let (home, allowed, allowed2, outside) = (root.join("home"), root.join("work"), root.join("work2"), root.join("other"));
        for (dir, file) in [(&home, "key"), (&allowed, "a.txt"), (&allowed2, "b.txt"), (&outside, "c.txt")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(file), b"x").unwrap();
        }
        let (home_real, allowed_real) = (std::fs::canonicalize(&home).unwrap(), vec![std::fs::canonicalize(&allowed).unwrap()]);
        // What toss asks before sending `path`: nothing when there are no lines.
        let ask = |path: PathBuf| -> Result<Vec<String>, String> {
            let paths = [path];
            let located = locate(&paths, &allowed_real, &home_real)?;
            Ok(question(&paths, &located, &crate::gather(&paths).map_err(|e| e.to_string())?))
        };

        assert_eq!(ask(allowed.join("a.txt")), Ok(vec![]));
        assert_eq!(ask(allowed.clone()), Ok(vec![]), "the allowed folder itself");
        for asks in [outside.join("c.txt"), allowed2.join("b.txt"), allowed.join("..").join("other").join("c.txt")] {
            assert!(ask(asks.clone()).is_ok_and(|lines| lines.len() == 1), "{} must ask: outside, a look-alike name, or a way out", asks.display());
        }
        assert!(ask(home.join("key")).is_err(), "toss's own folder never goes");
        assert!(ask(root.clone()).is_err(), "nor a folder holding it");
        assert!(ask(allowed.join("missing.txt")).is_err());
        let c = shown(&std::fs::canonicalize(outside.join("c.txt")).unwrap());
        let sneaky = &ask(allowed.join("..").join("other").join("c.txt")).unwrap()[0];
        assert!(sneaky.starts_with(&format!("{c} (")), "where it really is, not the way the assistant wrote it: {sneaky}");

        std::fs::write(allowed.join(".env"), b"KEY=1").unwrap();
        let lines = ask(allowed.clone()).unwrap();
        assert!(lines.len() == 1 && lines[0].contains("work/.env") && lines[0].contains("2 files"), "a secret asks even inside: {lines:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn secrets_look_like_secrets() {
        for secret in [".env", "app/.env.local", ".ssh/config", "keys/server.PEM", "id_ed25519", "vault.kdbx", ".aws/credentials"] {
            assert!(looks_secret(secret), "{secret}");
        }
        for plain in ["notes.md", "app/environment.md", "id_rsa.pub", "monkey", "keynote.txt"] {
            assert!(!looks_secret(plain), "{plain}");
        }
    }

    #[test]
    fn remembers_one_folder_but_never_home_or_secrets() {
        let root = std::env::temp_dir().join(format!("toss-remember-test-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"x").unwrap();
        }
        let (a, b) = (std::fs::canonicalize(&a).unwrap(), std::fs::canonicalize(&b).unwrap());
        assert_eq!(rememberable(&[(a.join("f.txt"), true)], false), Some(a.clone()), "a file's folder");
        assert_eq!(rememberable(&[(a.clone(), true)], false), Some(a.clone()), "a folder itself");
        assert_eq!(rememberable(&[(a.join("f.txt"), true), (b.join("f.txt"), true)], false), None, "two folders");
        assert_eq!(rememberable(&[(a.join("f.txt"), true)], true), None, "secrets always ask");
        let home = std::fs::canonicalize(dirs::home_dir().unwrap()).unwrap();
        assert_eq!(rememberable(&[(home, true)], false), None, "never the whole home folder");

        let toss_home = root.join("toss");
        std::fs::create_dir_all(&toss_home).unwrap();
        remember_folder(&toss_home, a.clone()).unwrap();
        remember_folder(&toss_home, b.clone()).unwrap();
        remember_folder(&toss_home, a.clone()).unwrap();
        assert_eq!(remembered(&toss_home), [a.clone(), b.clone()]);
        forget(&toss_home, &shown(&a)).unwrap();
        assert_eq!(remembered(&toss_home), [b]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
