use std::path::Path;

use crate::redact::Redactor;

/// Tool output above this many bytes is saved to a file instead of being sent whole.
pub const DEFAULT_OUTPUT_LIMIT: usize = 10 * 1024;

/// Caps tool output at roughly `limit` bytes. Larger output is saved in full to `dir/<call_id>.txt`,
/// without the secrets `redactor` knows; the model receives the head, the tail, the omitted size,
/// and the file path.
pub fn limit_output(
    content: &str,
    limit: usize,
    dir: &Path,
    call_id: &str,
    redactor: Option<&Redactor>,
) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let safe_id: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file = dir.join(format!("{safe_id}.txt"));
    let saved = std::fs::create_dir_all(dir).and_then(|_| {
        let content = redactor.map_or_else(|| content.to_string(), |r| r.redact(content));
        std::fs::write(&file, content)
    });

    let keep = limit * 2 / 5;
    let head_end = floor_boundary(content, keep);
    let tail_start = ceil_boundary(content, content.len() - keep);
    let omitted = &content[head_end..tail_start];
    let location = match saved {
        Ok(()) => format!("full output saved to {}", file.display()),
        Err(e) => format!("full output could not be saved: {e}"),
    };
    format!(
        "{}\n[... {} bytes / {} lines omitted; {location} ...]\n{}",
        &content[..head_end],
        omitted.len(),
        omitted.lines().count(),
        &content[tail_start..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
