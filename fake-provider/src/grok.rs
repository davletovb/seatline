//! Fake Grok Build persona for the PRO-09 adapter tests.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

pub fn is_grok(program: &OsString) -> bool {
    Path::new(program)
        .file_stem()
        .is_some_and(|stem| stem == "grok")
}

pub fn main(args: Vec<OsString>) -> ExitCode {
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::from(1),
    }
}

fn run(args: Vec<OsString>) -> Result<(), ()> {
    if args.first().is_some_and(|arg| arg == OsStr::new("models")) {
        let auth = std::env::var_os("GROK_AUTH_PATH")
            .map(PathBuf::from)
            .is_some_and(|path| path.exists());
        if auth {
            println!("You are logged in with grok.com.");
        } else {
            println!("You are not authenticated.");
        }
        println!();
        println!("Default model: grok-4.6");
        println!();
        println!("Available models:");
        println!("  * grok-4.6 (default)");
        println!("  - grok-4.5");
        return Ok(());
    }

    crate::record_launch("grok");

    assert_isolated_environment()?;

    let prompt = arg_value(&args, "--prompt-file")
        .and_then(|path| fs::read_to_string(path).ok())
        .ok_or(())?;
    let agent = arg_value(&args, "--agent")
        .and_then(|path| fs::read_to_string(path).ok())
        .ok_or(())?;
    crate::record("grok", "prompts", &format!("{prompt}\0"));
    crate::record("grok", "agents", &format!("{agent}\0"));
    crate::record(
        "grok",
        "invocations",
        &format!(
            "{}\n",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        ),
    );
    let requested_model = arg_value(&args, "--model")
        .and_then(|value| value.to_str())
        .unwrap_or("grok-4.6");

    if args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("Current user question"))
    {
        return Err(());
    }
    if !agent.contains("tools: []")
        || !args
            .windows(2)
            .any(|pair| pair[0] == "--disallowed-tools" && pair[1] == "Agent,search_tool,use_tool")
        || !args.iter().any(|arg| arg == "--disable-web-search")
    {
        return Err(());
    }

    let cwd = std::env::current_dir().map_err(|_| ())?;
    let api_key_source = if requested_model == "grok-init-auth" {
        "user"
    } else {
        "oauth"
    };
    let actual_model = match requested_model {
        "grok-4" => "grok-4-0709",
        "grok-init-model" => "grok-other",
        other => other,
    };
    let reported_cwd = if requested_model == "grok-init-cwd" {
        cwd.parent().unwrap_or(&cwd).to_path_buf()
    } else {
        cwd.clone()
    };
    let tools = if requested_model == "grok-init-tools" {
        serde_json::json!(["search_tool"])
    } else {
        serde_json::json!([])
    };
    let skills = if requested_model == "grok-init-skills" {
        serde_json::json!(["unexpected-skill"])
    } else {
        serde_json::json!([])
    };
    let mcp_servers = if requested_model == "grok-init-mcp" {
        serde_json::json!([{"name":"unexpected","status":"connected"}])
    } else {
        serde_json::json!([])
    };

    line(&serde_json::json!({
        "type":"system",
        "subtype":"init",
        "session_id":"grok-fake-session",
        "apiKeySource":api_key_source,
        "model":actual_model,
        "cwd":reported_cwd.to_string_lossy(),
        "permissionMode":"dontAsk",
        "tools":tools,
        "slash_commands":[],
        "mcp_servers":mcp_servers,
        "skills":skills
    }))?;

    if requested_model.starts_with("grok-init-") {
        hang_briefly();
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--include-partial-messages") {
        line(&serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"..."}}
        }))?;
    }

    match requested_model {
        "grok-tool-violation" => {
            line(&serde_json::json!({
                "type":"assistant",
                "message":{"content":[{"type":"tool_use","id":"tool-1","name":"run_terminal_cmd","input":{"command":"pwd"}}]}
            }))?;
            hang_briefly();
            return Ok(());
        }
        "grok-result-auth" => {
            line(&serde_json::json!({
                "type":"result","subtype":"error_during_execution","is_error":true,
                "errors":["Not signed in. To authenticate, run grok login."]
            }))?;
            return Err(());
        }
        "grok-hang" => {
            std::thread::sleep(Duration::from_secs(30));
            return Ok(());
        }
        _ => {}
    }

    let answer = if prompt.contains("Earlier assistant answer") {
        "Grok follow-up answer"
    } else {
        "Grok answer"
    };
    line(&serde_json::json!({
        "type":"assistant",
        "message":{"content":[{"type":"thinking","thinking":"done"},{"type":"text","text":answer}]}
    }))?;
    line(&serde_json::json!({
        "type":"result","subtype":"success","is_error":false,
        "result":answer,"stop_reason":"end_turn"
    }))?;
    Ok(())
}

fn assert_isolated_environment() -> Result<(), ()> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let grok_home = std::env::var_os("GROK_HOME").ok_or(())?;
    if home.as_ref() == Some(&grok_home) {
        return Err(());
    }
    for secret in [
        "XAI_API_KEY",
        "GROK_CODE_XAI_API_KEY",
        "GROK_MODELS_BASE_URL",
    ] {
        if std::env::var_os(secret).is_some() {
            return Err(());
        }
    }
    for (name, wanted) in [
        ("GROK_DISABLE_API_KEY_AUTH", "1"),
        ("GROK_DISABLE_AUTOUPDATER", "1"),
        ("GROK_SUBAGENTS", "0"),
        ("GROK_MEMORY", "0"),
        ("GROK_WEB_FETCH", "0"),
    ] {
        if std::env::var_os(name).as_deref() != Some(OsStr::new(wanted)) {
            return Err(());
        }
    }
    Ok(())
}

fn arg_value<'a>(args: &'a [OsString], name: &str) -> Option<&'a OsStr> {
    args.windows(2)
        .find(|pair| pair[0] == OsStr::new(name))
        .map(|pair| pair[1].as_os_str())
}

fn line(value: &serde_json::Value) -> Result<(), ()> {
    let mut stdout = io::stdout();
    writeln!(stdout, "{value}").map_err(|_| ())?;
    stdout.flush().map_err(|_| ())
}

fn hang_briefly() {
    std::thread::sleep(Duration::from_millis(750));
}
