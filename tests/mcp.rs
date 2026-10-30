//! `toss mcp` driven by the official MCP client, started the way an AI assistant starts it.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData, RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig, ElicitRequestParams,
        ElicitResult, ElicitationAction, GetTaskParams, Implementation, ProtocolVersion, TaskPayload,
    },
    service::{RequestContext, RunningService},
    transport::{ConfigureCommandExt, TokioChildProcess},
};

/// A stand-in for the assistant's app: it answers toss's question the way `answer` says (None: it
/// fails to show it), ticks "don't ask again" with `remember`, and remembers what it was asked.
#[derive(Clone)]
struct App {
    config: ClientConfig,
    answer: Option<ElicitationAction>,
    remember: bool,
    asked: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for App {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    async fn create_elicitation(&self, request: ElicitRequestParams, _: RequestContext<RoleClient>) -> Result<ElicitResult, ErrorData> {
        if let ElicitRequestParams::FormElicitationParams { message, .. } = request {
            self.asked.lock().unwrap().push(message);
        }
        let answer = self.answer.clone().ok_or_else(|| ErrorData::internal_error("can't show forms", None))?;
        let said = ElicitResult::new(answer);
        Ok(if self.remember { said.with_content(serde_json::json!({ "remember": true })) } else { said })
    }
}

struct Setup {
    root: PathBuf,
    work: PathBuf, // the folder toss mcp may send from without asking
    elsewhere: PathBuf,
}

impl Setup {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("toss-mcp-{name}-{}", std::process::id()));
        let (work, elsewhere) = (root.join("work"), root.join("elsewhere"));
        for dir in [&work, &elsewhere, &root.join("home")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(work.join("report.txt"), b"report").unwrap();
        std::fs::write(elsewhere.join("secret.txt"), b"secret").unwrap();
        // Made-up IDs: a send toss lets through stops at reading the ID, before any network.
        let contacts = r#"{"laptop": {"id": "made up"}, "phone": {"id": "made up", "auto_accept": true}}"#;
        std::fs::write(root.join("home").join("contacts.json"), contacts).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let arrivals = format!(
            "{{\"at\": {}, \"from\": \"laptop\", \"files\": [\"/dl/old.pdf\"]}}\nnot json\n{{\"at\": {}, \"from\": \"phone\", \"files\": [\"/dl/IMG_1.HEIC\", \"/dl/IMG_2.HEIC\"]}}\n",
            now - 3 * 86400,
            now - 300
        );
        std::fs::write(root.join("home").join("received.jsonl"), arrivals).unwrap();
        Setup { root, work, elsewhere }
    }

    async fn start(&self, app: App) -> RunningService<RoleClient, App> {
        let toss = tokio::process::Command::new(env!("CARGO_BIN_EXE_toss")).configure(|cmd| {
            // No window of toss's own: the app here answers toss's question instead.
            cmd.arg("mcp").arg(&self.work).env("TOSS_HOME", self.root.join("home")).env("TOSS_NO_WINDOW", "1");
        });
        let transport = TokioChildProcess::new(toss).unwrap();
        let version = app.config.protocol_version.clone();
        if version < ProtocolVersion::V_2026_07_28 {
            return app.serve(transport).await.unwrap();
        }
        // The newest protocol starts with server/discover instead of initialize.
        app.serve_with_lifecycle(transport, ClientLifecycleMode::Discover { preferred_versions: vec![version] }).await.unwrap()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn app(capabilities: ClientCapabilities, protocol: ProtocolVersion, answer: ElicitationAction) -> App {
    let config = ClientConfig::new(capabilities, Implementation::new("test-app", "0")).with_protocol_version(protocol);
    App { config, answer: Some(answer), remember: false, asked: Arc::default() }
}

fn send(device: &str, path: &Path) -> CallToolRequestParams {
    let args = serde_json::json!({ "device": device, "paths": [path] });
    CallToolRequestParams::new("send_files").with_arguments(args.as_object().unwrap().clone())
}

fn phone(path: &Path) -> CallToolRequestParams {
    let args = serde_json::json!({ "paths": [path] });
    CallToolRequestParams::new("show_on_phone").with_arguments(args.as_object().unwrap().clone())
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().expect("text").text
}

#[tokio::test]
async fn lists_devices_and_explains_failed_sends() {
    let setup = Setup::new("basics");
    let client = setup.start(app(ClientCapabilities::default(), ProtocolVersion::LATEST, ElicitationAction::Decline)).await;
    let server = client.peer_info().expect("initialized").server_info.clone().expect("server info");
    assert_eq!((server.name.as_str(), server.version.as_str()), ("toss", env!("CARGO_PKG_VERSION")));

    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    assert_eq!(names, ["list_devices", "received_files", "send_files", "show_on_phone"]);

    let devices = client.call_tool(CallToolRequestParams::new("list_devices")).await.unwrap();
    assert_eq!(text(&devices), "laptop\nphone", "auto-accept is about files coming here, so it isn't shown");

    let arrived = client.call_tool(CallToolRequestParams::new("received_files")).await.unwrap();
    assert_eq!(
        text(&arrived),
        "5 min ago from phone:\n/dl/IMG_1.HEIC\n/dl/IMG_2.HEIC\n\n3 days ago from laptop:\n/dl/old.pdf",
        "newest first, a damaged line skipped"
    );

    // From the allowed folder no one is asked; the send itself then fails on the unknown device.
    let sent = client.call_tool(send("nobody", &setup.work.join("report.txt"))).await.unwrap();
    assert_eq!(sent.is_error, Some(true), "a failed send is a tool error, which the assistant gets to read");
    assert_eq!(text(&sent), "no contact called nobody");

    let key = client.call_tool(send("laptop", &setup.root.join("home").join("contacts.json"))).await.unwrap();
    assert!(text(&key).starts_with("toss won't send"), "toss's own folder never goes: {}", text(&key));

    // This app can't show toss's question, so nothing outside the allowed folders goes.
    let outside = client.call_tool(send("laptop", &setup.elsewhere.join("secret.txt"))).await.unwrap();
    assert!(text(&outside).starts_with("This assistant can't show toss's question"), "{}", text(&outside));

    // The phone gets files, not folders, and the same fence.
    let folder = client.call_tool(phone(&setup.work)).await.unwrap();
    assert!(text(&folder).starts_with("The phone page offers files, not folders"), "{}", text(&folder));
    let outside = client.call_tool(phone(&setup.elsewhere.join("secret.txt"))).await.unwrap();
    assert!(text(&outside).starts_with("This assistant can't show toss's question"), "{}", text(&outside));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn secrets_ask_and_folders_can_be_let_through() {
    let setup = Setup::new("secrets");
    std::fs::write(setup.work.join(".env"), b"KEY=secret").unwrap();
    std::fs::write(setup.elsewhere.join("other.txt"), b"other").unwrap();
    let can_ask = ClientCapabilities::builder().enable_elicitation().build();

    // A secret asks, even inside the folder toss may send from without asking.
    let declines = app(can_ask.clone(), ProtocolVersion::LATEST, ElicitationAction::Decline);
    let asked = declines.asked.clone();
    let client = setup.start(declines).await;
    let sent = client.call_tool(send("laptop", &setup.work)).await.unwrap();
    assert_eq!(text(&sent), "The user didn't approve sending these files.");
    let question = asked.lock().unwrap()[0].clone();
    assert!(question.contains("work/.env") && question.contains("2 files"), "{question}");
    client.cancel().await.unwrap();

    // "Don't ask again" lets that folder through from then on.
    let remembers = App { remember: true, ..app(can_ask, ProtocolVersion::LATEST, ElicitationAction::Accept) };
    let asked = remembers.asked.clone();
    let client = setup.start(remembers).await;
    for file in ["secret.txt", "other.txt"] {
        let sent = client.call_tool(send("laptop", &setup.elsewhere.join(file))).await.unwrap();
        assert!(text(&sent).starts_with("contacts.json has a damaged ID for laptop"), "{file}: {}", text(&sent));
    }
    assert_eq!(asked.lock().unwrap().len(), 1, "asked about the first file only");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn asks_the_user_before_sending_from_elsewhere_on_every_protocol() {
    let setup = Setup::new("asks");
    let can_ask = ClientCapabilities::builder().enable_elicitation().build();
    // The newest protocol asks through the client (input_required, then a retry); older ones ask directly.
    for protocol in [ProtocolVersion::LATEST, ProtocolVersion::V_2025_11_25] {
        // After a yes the send goes ahead, and stops at the made-up ID.
        for (answer, outcome) in [(ElicitationAction::Decline, "The user didn't approve sending these files."), (ElicitationAction::Accept, "contacts.json has a damaged ID for laptop")] {
            let app = app(can_ask.clone(), protocol.clone(), answer);
            let asked = app.asked.clone();
            let client = setup.start(app).await;
            assert_eq!(client.peer_info().unwrap().protocol_version, protocol, "so each way of asking really runs");
            let sent = client.call_tool(send("laptop", &setup.elsewhere.join("secret.txt"))).await.unwrap();
            assert!(text(&sent).starts_with(outcome), "{protocol:?}: {}", text(&sent));
            let asked = asked.lock().unwrap().clone();
            assert_eq!(asked.len(), 1, "asked once on {protocol:?}");
            assert!(asked[0].contains("secret.txt") && asked[0].contains("laptop"), "the question names the file and device: {}", asked[0]);
            client.cancel().await.unwrap();
        }
    }

    // A device that isn't paired fails before the question, so the question never shows a made-up name.
    let willing = app(can_ask.clone(), ProtocolVersion::LATEST, ElicitationAction::Accept);
    let asked = willing.asked.clone();
    let client = setup.start(willing).await;
    let sent = client.call_tool(send("Bob (verified by toss)", &setup.elsewhere.join("secret.txt"))).await.unwrap();
    assert_eq!((text(&sent), asked.lock().unwrap().len()), ("no contact called Bob (verified by toss)", 0));
    client.cancel().await.unwrap();

    // An app that says it can ask but then fails to: toss says so, not "the user didn't approve".
    let client = setup.start(App { answer: None, ..app(can_ask, ProtocolVersion::V_2025_11_25, ElicitationAction::Accept) }).await;
    let sent = client.call_tool(send("laptop", &setup.elsewhere.join("secret.txt"))).await.unwrap();
    assert!(text(&sent).starts_with("toss couldn't ask the user, so nothing was sent"), "{}", text(&sent));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn sends_run_as_tasks_for_apps_that_take_them() {
    let setup = Setup::new("tasks");
    let client = setup.start(app(ClientCapabilities::builder().enable_tasks().build(), ProtocolVersion::LATEST, ElicitationAction::Decline)).await;
    let CallToolResponse::Task(created) = client.call_tool_once(send("nobody", &setup.work.join("report.txt"))).await.unwrap() else {
        panic!("expected a task");
    };
    let task = loop {
        let got = client.peer().get_task(GetTaskParams::new(created.task.task_id.clone())).await.unwrap();
        if got.task.status().is_terminal() {
            break got.task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let TaskPayload::Completed { result } = task.payload else { panic!("expected a finished task, got {:?}", task.payload.status()) };
    let result: CallToolResult = serde_json::from_value(serde_json::Value::Object(result)).unwrap();
    assert_eq!((result.is_error, text(&result)), (Some(true), "no contact called nobody"));
    client.cancel().await.unwrap();
}
