//! What an agent sends its parent: its report (`finish`).

use super::*;

/// `finish`'s run: the child's report queued to its parent, the child
/// `finished`, and the parent woken. The turn loop ends the child's turn
/// after a successful call (`turn::run_tools`).
pub(crate) async fn finish(pool: &PgPool, child: i64, input: &serde_json::Value) -> Result<String, String> {
    let report = input
        .get("report")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing or non-string field: report")?;
    let chars = report.chars().count();
    if report.trim().is_empty() || chars > MAX_REPORT_CHARS {
        return Err(format!("report must be 1 to {MAX_REPORT_CHARS} characters (it has {chars})"));
    }
    let outcome = db::finish_agent(pool, child, &label(child, None), report)
        .await
        .map_err(|e| format!("couldn't send your report: {e}"))?;
    match outcome {
        db::Finish::Queued { parent } => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
            Ok("Your report was sent to your parent. Your turn ends here; the user may still write to you.".to_string())
        }
        db::Finish::NotActive(Some(state)) if state == db::AGENT_FINISHED => {
            Err("you've already finished: your report was sent".to_string())
        }
        db::Finish::NotActive(Some(state)) if state == db::AGENT_STOPPED => {
            Err("you were stopped by your parent, so there's no one to report to".to_string())
        }
        db::Finish::NotActive(state) => Err(format!(
            "you can't finish now (your state is {})",
            state.as_deref().unwrap_or("unknown")
        )),
    }
}
