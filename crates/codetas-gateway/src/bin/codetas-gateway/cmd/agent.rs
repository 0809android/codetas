use super::*;
use serde_json::json;
use std::path::Path;
pub(crate) fn agent(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{AGENT_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "status".into());
    let json_output = take_flag(&mut args, "--json")?;
    if action == "status" || action == "show" {
        finish_args(&args, AGENT_USAGE)?;
        let settings = read_valid_settings(config)?;
        let value = json!({
            "agents": settings.agents,
            "sidecars": settings.sidecars,
            "helperIntercept": settings.helper_intercept,
        });
        return if json_output {
            print_json(&value)
        } else {
            print_json(&value)
        };
    }
    if action != "set" {
        return Err(format!("unknown agent action: {action}\n{AGENT_USAGE}"));
    }
    reject_json_for_mutation(json_output)?;
    let surface = take_option(&mut args, "--surface")?
        .map(|value| parse_surface(&value))
        .transpose()?;
    let threads = optional_u16(&mut args, "--threads")?;
    let multi_agent = take_option(&mut args, "--multi-agent-v2")?
        .map(|value| parse_bool(&value, "--multi-agent-v2"))
        .transpose()?;
    let main_effort = take_option(&mut args, "--main-effort")?;
    let subagent_effort = take_option(&mut args, "--subagent-effort")?;
    let subagents = take_option(&mut args, "--subagents")?;
    let fallback = take_option(&mut args, "--fallback")?;
    let web = take_option(&mut args, "--web-search-model")?;
    let vision = take_option(&mut args, "--vision-model")?;
    let image = take_option(&mut args, "--image-model")?;
    let video = take_option(&mut args, "--video-model")?;
    let live = take_option(&mut args, "--live-model")?;
    let helper_enabled = take_option(&mut args, "--helper-intercept")?
        .map(|value| parse_bool(&value, "--helper-intercept"))
        .transpose()?;
    let helper_target = take_option(&mut args, "--helper-target")?;
    let helper_sources = take_option(&mut args, "--helper-sources")?;
    finish_args(&args, AGENT_USAGE)?;
    let mut settings = read_valid_settings(config)?;
    if let Some(value) = surface {
        settings.agents.surface_mode = value;
    }
    if let Some(value) = threads {
        settings.agents.max_threads = value;
    }
    if let Some(value) = multi_agent {
        settings.agents.multi_agent_v2 = value;
    }
    if let Some(value) = main_effort {
        settings.agents.effort_cap = clearable(value);
    }
    if let Some(value) = subagent_effort {
        settings.agents.subagent_effort_cap = clearable(value);
    }
    if let Some(value) = subagents {
        settings.agents.subagent_models = clearable_csv(value);
    }
    if let Some(value) = fallback {
        settings.agents.subagent_fallback = clearable_csv(value);
    }
    if let Some(value) = web {
        settings.sidecars.web_search_model = clearable(value);
    }
    if let Some(value) = vision {
        settings.sidecars.vision_model = clearable(value);
    }
    if let Some(value) = image {
        settings.sidecars.image_model = clearable(value);
    }
    if let Some(value) = video {
        settings.sidecars.video_model = clearable(value);
    }
    if let Some(value) = live {
        settings.sidecars.live_model = clearable(value);
    }
    if let Some(value) = helper_enabled {
        settings.helper_intercept.enabled = value;
    }
    if let Some(value) = helper_target {
        settings.helper_intercept.target_model = clearable(value);
    }
    if let Some(value) = helper_sources {
        settings.helper_intercept.source_models = csv(&value);
    }
    save_settings(config, &settings)?;
    println!("Updated agent settings.");
    Ok(())
}
