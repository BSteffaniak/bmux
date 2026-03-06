pub struct AttachTab {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) active: bool,
}

pub fn build_attach_status_line(
    session_label: &str,
    tabs: &[AttachTab],
    mode_label: &str,
    role_label: &str,
    follow_label: Option<&str>,
    hint: &str,
) -> String {
    let mut status = format!(" bmux [{mode_label}] [{role_label}] | session: {session_label} | ");

    if tabs.is_empty() {
        status.push_str("tabs: (none)");
    } else {
        status.push_str("tabs: ");
        for tab in tabs {
            if tab.active {
                status.push_str(&format!("[{}:{}] ", tab.index, tab.title));
            } else {
                status.push_str(&format!(" {}:{} ", tab.index, tab.title));
            }
        }
    }

    if let Some(follow) = follow_label {
        status.push_str("| ");
        status.push_str(follow);
        status.push(' ');
    }

    status.push_str("| ");
    status.push_str(hint);
    status.push(' ');
    status
}
