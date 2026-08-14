use super::*;
use codetas_gateway::ExternalAccessKey;
use serde_json::json;
use std::path::Path;
pub(crate) fn access(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{ACCESS_USAGE}");
        return Ok(());
    }
    let area = positional(&mut args).unwrap_or_else(|| "key".into());
    let json_output = take_flag(&mut args, "--json")?;
    if area == "endpoints" {
        finish_args(&args, ACCESS_USAGE)?;
        let settings = read_valid_settings(config)?;
        let base = format!("http://{}:{}", settings.runtime.host, settings.runtime.port);
        let value = json!({
            "baseUrl": base.clone(),
            "responsesEndpoint": format!("{base}/v1/responses"),
            "chatEndpoint": format!("{base}/v1/chat/completions"),
            "messagesEndpoint": format!("{base}/v1/messages"),
            "geminiEndpointTemplate": format!("{base}/v1beta/models/{{model}}:generateContent"),
            "geminiStreamEndpointTemplate": format!("{base}/v1beta/models/{{model}}:streamGenerateContent"),
            "geminiModelsEndpoint": format!("{base}/v1beta/models"),
            "modelsEndpoint": format!("{base}/v1/models"),
            "websocketEndpoint": format!("ws://{}:{}/v1/responses", settings.runtime.host, settings.runtime.port),
            "sidecarEndpoints": {
                "webSearch": format!("{base}/v1/sidecars/web-search"),
                "vision": format!("{base}/v1/sidecars/vision"),
                "image": format!("{base}/v1/sidecars/image"),
                "video": format!("{base}/v1/sidecars/video"),
                "videoStatusTemplate": format!("{base}/v1/sidecars/video/{{requestId}}"),
            },
        });
        return if json_output {
            print_json(&value)
        } else {
            print_json(&value)
        };
    }
    if area != "key" && area != "keys" {
        return Err(format!("unknown access area: {area}\n{ACCESS_USAGE}"));
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    match action.as_str() {
        "list" => {
            finish_args(&args, ACCESS_USAGE)?;
            let settings = read_valid_settings(config)?;
            if json_output {
                print_json(&settings.security.external_access_keys)
            } else {
                for item in settings.security.external_access_keys {
                    println!(
                        "{}  {}  {}  {}",
                        item.id,
                        item.label,
                        item.env_var,
                        item.scopes.join(",")
                    );
                }
                Ok(())
            }
        }
        "add" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "key id", ACCESS_USAGE)?;
            let label = take_option(&mut args, "--label")?.ok_or("key add requires --label")?;
            let env_var = take_option(&mut args, "--env")?.ok_or("key add requires --env")?;
            let scopes =
                csv(&take_option(&mut args, "--scopes")?.ok_or("key add requires --scopes")?);
            let expires_at_unix = optional_u64(&mut args, "--expires-at")?;
            finish_args(&args, ACCESS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            if settings
                .security
                .external_access_keys
                .iter()
                .any(|item| item.id == id)
            {
                return Err(format!("access key id already exists: {id}"));
            }
            settings
                .security
                .external_access_keys
                .push(ExternalAccessKey {
                    id: id.clone(),
                    label,
                    env_var,
                    scopes,
                    enabled: true,
                    expires_at_unix,
                });
            save_settings(config, &settings)?;
            println!("Added environment-backed access key {id}.");
            Ok(())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "key id", ACCESS_USAGE)?;
            finish_args(&args, ACCESS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            access_key_mut(&mut settings, &id)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} access key {id}.",
                if action == "enable" {
                    "Enabled"
                } else {
                    "Disabled"
                }
            );
            Ok(())
        }
        "remove" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "key id", ACCESS_USAGE)?;
            finish_args(&args, ACCESS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let original = settings.security.external_access_keys.len();
            settings
                .security
                .external_access_keys
                .retain(|item| item.id != id);
            if settings.security.external_access_keys.len() == original {
                return Err(format!("unknown access key: {id}"));
            }
            save_settings(config, &settings)?;
            println!("Removed access key {id}.");
            Ok(())
        }
        _ => Err(format!(
            "unknown access key action: {action}\n{ACCESS_USAGE}"
        )),
    }
}
