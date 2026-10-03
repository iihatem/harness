//! The live-eval results checked in under `eval/results/`, and the rule that built-in profiles
//! change an edit format only where one backs the change.

use std::path::Path;

use globset::GlobBuilder;
use harness_core::edit_format::EditFormat;
use serde::Deserialize;

/// What a result file says that the rule needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    pub model: String,
    pub format: EditFormat,
    pub pass_rate: f64,
}

#[derive(Deserialize)]
struct File {
    model: String,
    format: EditFormat,
    summary: Summary,
}

#[derive(Deserialize)]
struct Summary {
    pass_rate: f64,
}

/// The results in `dir` (its `.json` files); none when there is no such directory.
pub fn load(dir: &Path) -> Result<Vec<Stored>, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let file: File = serde_json::from_str(&text)
                .map_err(|e| format!("{}: not a result file: {e}", path.display()))?;
            Ok(Stored {
                model: file.model,
                format: file.format,
                pass_rate: file.summary.pass_rate,
            })
        })
        .collect()
}

fn best(results: &[&Stored], format: EditFormat) -> Option<f64> {
    results
        .iter()
        .filter(|r| r.format == format)
        .map(|r| r.pass_rate)
        .reduce(f64::max)
}

/// Whether `results` back a built-in profile for the models matching `key` using `format`:
/// `str_replace` needs no result; another format needs a result for a model of the family that
/// beats the `str_replace` result for the family on pass rate. An error says what is missing.
pub fn backing(results: &[Stored], key: &str, format: EditFormat) -> Result<(), String> {
    if format == EditFormat::StrReplace {
        return Ok(());
    }
    let matcher = GlobBuilder::new(key)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("{key} is not a valid glob: {e}"))?
        .compile_matcher();
    let family: Vec<&Stored> = results
        .iter()
        .filter(|r| matcher.is_match(&r.model))
        .collect();
    let Some(got) = best(&family, format) else {
        return Err(format!(
            "no result under eval/results/ covers {key} in {format}: run `cargo xtask eval run --format {format} --save` for a model of the family and check the file in"
        ));
    };
    let Some(baseline) = best(&family, EditFormat::StrReplace) else {
        return Err(format!(
            "eval/results/ has {format} for {key} but no str_replace result of the family to compare it with"
        ));
    };
    if got > baseline {
        Ok(())
    } else {
        Err(format!(
            "{format} does not beat str_replace for {key} in eval/results/ ({:.0}% against {:.0}%)",
            got * 100.0,
            baseline * 100.0
        ))
    }
}
