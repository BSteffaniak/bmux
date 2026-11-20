pub(super) fn status_ready_lines(next: Option<&str>) -> Vec<String> {
    let mut lines = vec!["Status: ready".to_string()];
    if let Some(value) = next {
        lines.push(format!("Next: {value}"));
    }
    lines
}

pub(super) fn status_not_ready_lines(
    reason: Option<&str>,
    fix: &str,
    advanced: Option<&str>,
) -> Vec<String> {
    let mut lines = vec!["Status: not ready".to_string()];
    if let Some(value) = reason {
        lines.push(format!("Reason: {value}"));
    }
    lines.push(format!("Fix: {fix}"));
    if let Some(value) = advanced {
        lines.push(format!("Advanced: {value}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{status_not_ready_lines, status_ready_lines};

    #[test]
    fn status_ready_lines_include_next_when_provided() {
        let lines = status_ready_lines(Some("bmux hosts"));
        assert_eq!(
            lines,
            vec!["Status: ready".to_string(), "Next: bmux hosts".to_string()]
        );
    }

    #[test]
    fn status_not_ready_lines_include_reason_fix_and_advanced() {
        let lines = status_not_ready_lines(
            Some("host is offline"),
            "bmux setup",
            Some("bmux host --daemon"),
        );
        assert_eq!(
            lines,
            vec![
                "Status: not ready".to_string(),
                "Reason: host is offline".to_string(),
                "Fix: bmux setup".to_string(),
                "Advanced: bmux host --daemon".to_string(),
            ]
        );
    }
}
