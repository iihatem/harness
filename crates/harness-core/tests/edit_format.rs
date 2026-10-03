//! The four edit formats and their names.

use harness_core::edit_format::EditFormat;

#[test]
fn str_replace_is_the_default() {
    assert_eq!(EditFormat::default(), EditFormat::StrReplace);
}

#[test]
fn the_formats_are_named_in_snake_case() {
    for (name, format) in [
        ("str_replace", EditFormat::StrReplace),
        ("apply_patch", EditFormat::ApplyPatch),
        ("whole_file", EditFormat::WholeFile),
        ("hashline", EditFormat::Hashline),
    ] {
        assert_eq!(name.parse::<EditFormat>(), Ok(format));
        assert_eq!(format.to_string(), name);
        assert_eq!(
            serde_json::from_value::<EditFormat>(serde_json::json!(name)).unwrap(),
            format
        );
        assert_eq!(
            serde_json::to_value(format).unwrap(),
            serde_json::json!(name)
        );
    }
}

#[test]
fn another_name_is_refused_listing_the_four() {
    let error = "diff".parse::<EditFormat>().unwrap_err();
    for name in ["str_replace", "apply_patch", "whole_file", "hashline"] {
        assert!(error.contains(name), "{error}");
    }
    assert!(serde_json::from_value::<EditFormat>(serde_json::json!("diff")).is_err());
}
