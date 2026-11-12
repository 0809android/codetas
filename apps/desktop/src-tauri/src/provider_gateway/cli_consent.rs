//! The user's decision about reusing other apps' logins.
//!
//! CODETAS can import the logins of installed CLIs and apps (Claude Code, Kimi,
//! Qwen, Antigravity and others) and send them to each provider the way that
//! provider's own client does. Provider terms may not allow this, and an account can
//! be suspended for it. Nothing is imported, and no in-app sign-in with another
//! app's client starts, until the user has accepted that risk in a native dialog.
//! The decision is kept next to `providers.json`.

use super::*;

const CONSENT_FILE: &str = "cli-login-consent.json";
const CONSENT_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConsentRecord {
    version: u8,
    accepted: bool,
    decided_at_unix: u64,
}

fn consent_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(settings_path(app)?.with_file_name(CONSENT_FILE))
}

/// `Some(true)` when the user accepted, `Some(false)` when they declined, `None`
/// before they were asked.
fn recorded_decision(app: &AppHandle) -> Option<bool> {
    let bytes = fs::read(consent_path(app).ok()?).ok()?;
    let record = serde_json::from_slice::<ConsentRecord>(&bytes).ok()?;
    (record.version == CONSENT_VERSION).then_some(record.accepted)
}

fn record_decision(app: &AppHandle, accepted: bool) -> Result<(), String> {
    let record = ConsentRecord {
        version: CONSENT_VERSION,
        accepted,
        decided_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
    };
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("同意の記録を作れません: {error}"))?;
    atomic_write(&consent_path(app)?, &bytes)
}

/// Whether imports of other apps' logins may run without asking.
pub(crate) fn cli_login_reuse_allowed(app: &AppHandle) -> bool {
    recorded_decision(app) == Some(true)
}

const CONSENT_DESCRIPTION: &str = "CODETAS は、Claude Code・Kimi・Qwen・Antigravity などの CLI やアプリでサインイン済みのログインを取り込み、それぞれの公式クライアントと同じ形式で各社のサービスに送ります。アプリ内でのサインインも、同じように各社の公式クライアントとして行います。\n\nこの使い方は各社の利用規約で認められていない場合があり、アカウントの停止や利用制限を受けるおそれがあります。各社の規約を確認したうえで、自己責任で使ってください。\n\nOK を押すと、ログインの取り込みと利用を許可します。キャンセルすると取り込みません。あとで接続の「登録」やサインインから改めて許可できます。";

async fn ask(app: &AppHandle) -> Result<bool, String> {
    let result = rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("CODETAS: 他のアプリのログインの利用について")
        .set_description(CONSENT_DESCRIPTION)
        .set_buttons(rfd::MessageButtons::OkCancel)
        .show()
        .await;
    let accepted = matches!(
        result,
        rfd::MessageDialogResult::Ok | rfd::MessageDialogResult::Yes
    );
    record_decision(app, accepted)?;
    Ok(accepted)
}

/// At startup: asks once, before any automatic import, if the user was never asked.
/// A recorded decision, either way, is kept without asking again.
pub(crate) async fn ask_cli_login_reuse_once(app: &AppHandle) -> Result<(), String> {
    if recorded_decision(app).is_none() {
        ask(app).await?;
    }
    Ok(())
}

/// Before an import or sign-in the user started: asks again when they have not
/// accepted, and fails when they decline.
pub(crate) async fn require_cli_login_reuse(app: &AppHandle) -> Result<(), String> {
    if cli_login_reuse_allowed(app) || ask(app).await? {
        Ok(())
    } else {
        Err(
            "他のアプリのログインの利用が許可されていないため、取り込みとサインインを中止しました"
                .into(),
        )
    }
}
