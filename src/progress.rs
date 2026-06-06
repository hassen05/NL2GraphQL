#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PipelineProgressEvent {
    pub(crate) stage: String,
    pub(crate) status: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) step_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) op: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) row_count: Option<usize>,
}

pub(crate) type ProgressCallback<'a> = &'a (dyn Fn(PipelineProgressEvent) + Send + Sync + 'a);

pub(crate) fn emit_progress(progress: Option<ProgressCallback<'_>>, event: PipelineProgressEvent) {
    if let Some(progress) = progress {
        progress(event);
    }
}

impl PipelineProgressEvent {
    pub(crate) fn stage(stage: &str, status: &str, message: impl Into<String>) -> Self {
        Self {
            stage: stage.to_string(),
            status: status.to_string(),
            message: message.into(),
            step_id: None,
            op: None,
            root: None,
            row_count: None,
        }
    }

    pub(crate) fn step(
        status: &str,
        step_id: &str,
        op: &str,
        root: Option<&str>,
        row_count: Option<usize>,
    ) -> Self {
        let root_suffix = root.map(|r| format!(" from `{r}`")).unwrap_or_default();
        let row_suffix = row_count
            .map(|count| format!(" ({count} row(s))"))
            .unwrap_or_default();
        Self {
            stage: "plan_step".to_string(),
            status: status.to_string(),
            message: format!("Step `{step_id}` {status}: {op}{root_suffix}{row_suffix}."),
            step_id: Some(step_id.to_string()),
            op: Some(op.to_string()),
            root: root.map(str::to_string),
            row_count,
        }
    }
}
