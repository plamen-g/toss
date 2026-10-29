//! `toss mcp` driven by the official MCP client, started the way an AI assistant starts it.

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{ConfigureCommandExt, TokioChildProcess},
};

#[tokio::test]
async fn lists_devices_and_explains_failed_sends() {
    let home = std::env::temp_dir().join(format!("toss-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let contacts = r#"{"laptop": {"id": "not needed here"}, "phone": {"id": "not needed here", "auto_accept": true}}"#;
    std::fs::write(home.join("contacts.json"), contacts).unwrap();
    let toss = tokio::process::Command::new(env!("CARGO_BIN_EXE_toss")).configure(|cmd| {
        cmd.arg("mcp").env("TOSS_HOME", &home);
    });
    let client = ().serve(TokioChildProcess::new(toss).unwrap()).await.unwrap();
    let server = client.peer_info().expect("initialized").server_info.clone().expect("server info");
    assert_eq!((server.name.as_str(), server.version.as_str()), ("toss", env!("CARGO_PKG_VERSION")));

    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    assert_eq!(names, ["list_devices", "send_files"]);

    let devices = client.call_tool(CallToolRequestParams::new("list_devices")).await.unwrap();
    assert_eq!(text(&devices), "laptop\nphone (takes files from here without asking)");

    let args = serde_json::json!({ "device": "nobody", "paths": [home.join("contacts.json")] });
    let sent = client.call_tool(CallToolRequestParams::new("send_files").with_arguments(args.as_object().unwrap().clone())).await.unwrap();
    assert_eq!(sent.is_error, Some(true), "a failed send is a tool error, which the assistant gets to read");
    assert_eq!(text(&sent), "no contact called nobody");

    client.cancel().await.unwrap();
    std::fs::remove_dir_all(&home).unwrap();
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().expect("text").text
}
