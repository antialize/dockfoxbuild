//! Parses a Dockerfile and extracts the operations in order.
use crate::state::Operation;
use anyhow::{Context, Result};
use std::borrow::Cow;
use std::path::Path;

/// Parses a Dockerfile and extracts the operations in order, including support for line continuations and comments.
pub fn parse_dockerfile(path: &Path) -> Result<Vec<(Operation, String)>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read Dockerfile: {}", path.display()))?;
    let mut operations = Vec::new();
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("# CHECKPOINT") {
            operations.push((Operation::Checkpoint, "".to_string()));
            continue;
        }
        if line.starts_with("#") {
            continue;
        }
        let line = if let Some(line) = line.strip_suffix('\\') {
            let mut full = line.trim().to_string();
            loop {
                let line = lines
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Unexpected end of file while parsing line"))?;
                if let Some(line) = line.strip_suffix('\\') {
                    full.push('\n');
                    full.push_str(line.trim());
                } else {
                    full.push_str(line.trim());
                    break;
                }
            }
            Cow::Owned(full)
        } else {
            Cow::Borrowed(line.trim())
        };
        if line.is_empty() {
            continue;
        }
        let (invocation, line) = line
            .split_once([' ', '\t', '\n'])
            .ok_or_else(|| anyhow::anyhow!("Failed to parse line: {}", line))?;
        let line = line.trim().to_string();
        match invocation.trim() {
            "FROM" => operations.push((Operation::From, line)),
            "ARG" => operations.push((Operation::Arg, line)),
            "ENV" => operations.push((Operation::Env, line)),
            "RUN" => operations.push((Operation::Run, line)),
            "WORKDIR" => operations.push((Operation::Workdir, line)),
            "COPY" => operations.push((Operation::Copy, line)),
            "ADD" => operations.push((Operation::Add, line)),
            "LABEL" => operations.push((Operation::Label, line)),
            "USER" => operations.push((Operation::User, line)),
            "ENTRYPOINT" => operations.push((Operation::Entrypoint, line)),
            "CMD" => operations.push((Operation::Cmd, line)),
            "EXPOSE" => operations.push((Operation::Expose, line)),
            "VOLUME" => operations.push((Operation::Volume, line)),
            "STOPSIGNAL" => operations.push((Operation::StopSignal, line)),
            "HEALTHCHECK" => operations.push((Operation::HealthCheck, line)),
            _ => {
                anyhow::bail!("Unsupported instruction: '{}'", invocation);
            }
        }
    }
    Ok(operations)
}
