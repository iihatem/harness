use std::path::Path;

use crate::redact::Redactor;

/// Tool output above this many bytes is saved to a file instead of being sent whole.
pub const DEFAULT_OUTPUT_LIMIT: usize = 10 * 1024;

/// Caps tool output at roughly `limit` bytes. Larger output is saved in full to `dir/<call_id>.txt`,
/// without the secrets `redactor` knows; the model receives the head, the tail, the omitted size,
/// and the file path. The cuts never run through a secret `redactor` knows.
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
    let saved = save(
        dir,
        &file,
        &redactor.map_or_else(|| content.to_string(), |r| r.redact(content)),
    );

    let keep = limit * 2 / 5;
    // The head and the tail go to the model as they are, and then to the session file and the
    // event stream, which can only redact secrets they hold whole: a secret the cut would run
    // through goes wholly to the omitted part. Only the text around each cut is searched, and a
    // cut moves by the longest secret's length at most.
    let mut head_end = floor_boundary(content, keep);
    let mut tail_start = ceil_boundary(content, content.len() - keep);
    if let Some(redactor) = redactor {
        head_end = redactor.clear_cut(content, head_end, true);
        tail_start = redactor.clear_cut(content, tail_start, false);
    }
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

/// Writes `content` to `file` in `dir`. It holds what the session file holds, so like session
/// files only its owner may read it: the directories it creates are `0700`, the file `0600`.
fn save(dir: &Path, file: &Path, content: &str) -> std::io::Result<()> {
    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    // One that was already there keeps its mode unless it is set.
    out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    out.write_all(content.as_bytes())
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
